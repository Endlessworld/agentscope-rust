//! Deterministic evidence locations for JEV code review.
//!
//! Mirrors `io.agentscope.extensions.judge.jev.review.{ReviewRegions, JevReviewInput}`.
//! Java's invariant is stated in the class comment: *evidence locations derived from supplied
//! text, never from model-generated line numbers*. So [`ReviewRegions::source`] cuts a supplied
//! file into fixed-size regions and [`ReviewRegions::patch`] only accepts a strict one-file
//! unified diff — malformed, binary and combined diffs raise [`JevError::InvalidArgument`]
//! instead of inventing line numbers.

use serde::{Deserialize, Serialize};

use crate::error::JevError;

/// Which side of a diff a [`Region`] refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Side {
    /// The pre-image (`-` lines).
    Old,
    /// The post-image (`+` and context lines).
    New,
}

/// A contiguous, reviewable slice of a source file or diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Region {
    /// Stable region id (`R1`, `R2`, ... for sources; `hunk_1`, `hunk_2`, ... for patches).
    pub id: String,
    /// First line covered, 1-based.
    pub start_line: i64,
    /// Last line covered, 1-based and inclusive.
    pub end_line: i64,
    /// Which side the lines belong to.
    pub side: Side,
    /// The region text, lines joined with `\n`.
    pub text: String,
}

/// Splits `content` into `lines_per_region`-sized regions.
///
/// Errors with [`JevError::InvalidArgument`] when `lines_per_region < 1`; blank content yields no
/// regions. A trailing newline is not counted as an extra source line.
pub fn source_regions(content: &str, lines_per_region: usize) -> Result<Vec<Region>, JevError> {
    if lines_per_region < 1 {
        return Err(JevError::invalid("positive region length required"));
    }
    if content.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut lines = split_lines(content);
    if lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    let mut regions = Vec::new();
    let mut start = 0usize;
    while start < lines.len() {
        let end = (start + lines_per_region).min(lines.len());
        regions.push(Region {
            id: format!("R{}", regions.len() + 1),
            start_line: (start + 1) as i64,
            end_line: end as i64,
            side: Side::New,
            text: lines[start..end].join("\n"),
        });
        start = end;
    }
    Ok(regions)
}

/// Derives one region per hunk from a strict single-file unified diff.
///
/// Rejects malformed hunks, multi-file diffs, binary/combined diffs and line overflow with
/// [`JevError::InvalidArgument`].
pub fn patch_regions(patch: &str) -> Result<Vec<Region>, JevError> {
    if patch.trim().is_empty() {
        return Ok(Vec::new());
    }
    let lines = split_lines(patch);
    let mut regions: Vec<Region> = Vec::new();
    let mut hunk: Option<Hunk> = None;
    let mut file_header = false;

    for (index, line) in lines.iter().enumerate() {
        if index == lines.len() - 1 && line.is_empty() {
            continue;
        }
        if let Some(header) = parse_hunk_header(line)? {
            if let Some(previous) = hunk.take() {
                flush(&mut regions, previous)?;
            }
            hunk = Some(header.into_hunk()?);
        } else if let Some(current) = hunk.as_mut() {
            if line.starts_with('+') {
                current.new_seen += 1;
            } else if line.starts_with('-') {
                current.old_seen += 1;
            } else if line.starts_with(' ') {
                current.old_seen += 1;
                current.new_seen += 1;
            } else if *line != "\\ No newline at end of file" {
                return Err(JevError::invalid("unsupported diff content"));
            }
            current.lines.push((*line).to_string());
        } else if line.starts_with("diff --git ") {
            if file_header {
                return Err(JevError::invalid("one file per diff required"));
            }
            file_header = true;
        } else if !is_known_preamble(line) {
            return Err(JevError::invalid("unsupported diff preamble"));
        }
    }

    if let Some(previous) = hunk.take() {
        flush(&mut regions, previous)?;
    }
    Ok(regions)
}

#[derive(Debug, Clone)]
struct HunkHeader {
    old_start: i64,
    old_count: i64,
    new_start: i64,
    new_count: i64,
}

#[derive(Debug, Clone)]
struct Hunk {
    old_start: i64,
    old_count: i64,
    new_start: i64,
    new_count: i64,
    old_seen: i64,
    new_seen: i64,
    lines: Vec<String>,
}

impl HunkHeader {
    fn into_hunk(self) -> Result<Hunk, JevError> {
        if (self.old_count > 0 && self.old_start == 0)
            || (self.new_count > 0 && self.new_start == 0)
            || (self.old_count == 0 && self.new_count == 0)
        {
            return Err(JevError::invalid("invalid diff range"));
        }
        Ok(Hunk {
            old_start: self.old_start,
            old_count: self.old_count,
            new_start: self.new_start,
            new_count: self.new_count,
            old_seen: 0,
            new_seen: 0,
            lines: Vec::new(),
        })
    }
}

fn flush(regions: &mut Vec<Region>, hunk: Hunk) -> Result<(), JevError> {
    if hunk.old_seen != hunk.old_count || hunk.new_seen != hunk.new_count {
        return Err(JevError::invalid("diff hunk counts do not match content"));
    }
    let (start, length, side) = if hunk.new_count > 0 {
        (hunk.new_start, hunk.new_count, Side::New)
    } else {
        (hunk.old_start, hunk.old_count, Side::Old)
    };
    // Java catches `Math.addExact` overflow; `i32::MAX` is the bound the fixture exercise uses.
    let end = start
        .checked_add(length - 1)
        .filter(|end| *end <= i32::MAX as i64)
        .ok_or_else(|| JevError::invalid("diff line overflow"))?;
    regions.push(Region {
        id: format!("hunk_{}", regions.len() + 1),
        start_line: start,
        end_line: end,
        side,
        text: hunk.lines.join("\n"),
    });
    Ok(())
}

fn is_known_preamble(line: &str) -> bool {
    const PREFIXES: [&str; 11] = [
        "--- ",
        "+++ ",
        "index ",
        "new file mode ",
        "deleted file mode ",
        "old mode ",
        "new mode ",
        "similarity index ",
        "rename from ",
        "rename to ",
        "diff --git ",
    ];
    PREFIXES.iter().any(|prefix| line.starts_with(prefix))
}

/// Parses `@@ -old[,oldCount] +new[,newCount] @@ ...`.
///
/// Returns `Ok(None)` when the line is not a hunk header (the caller then treats it as content or
/// preamble).
fn parse_hunk_header(line: &str) -> Result<Option<HunkHeader>, JevError> {
    let Some(rest) = line.strip_prefix("@@ -") else {
        return Ok(None);
    };
    let Some((old_part, after_old)) = split_optional_count(rest) else {
        return Ok(None);
    };
    let Some(rest) = after_old.strip_prefix(" +") else {
        return Ok(None);
    };
    let Some((new_part, after_new)) = split_optional_count(rest) else {
        return Ok(None);
    };
    // Java requires a literal " @@" terminator before the trailing context.
    if !after_new.starts_with(" @@") {
        return Ok(None);
    }
    Ok(Some(HunkHeader {
        old_start: old_part.0,
        old_count: old_part.1.unwrap_or(1),
        new_start: new_part.0,
        new_count: new_part.1.unwrap_or(1),
    }))
}

/// Splits a leading `\d+` or `\d+,\d+` pair off `input`.
fn split_optional_count(input: &str) -> Option<((i64, Option<i64>), &str)> {
    let digits_end = input
        .char_indices()
        .take_while(|(_, ch)| ch.is_ascii_digit())
        .map(|(index, ch)| index + ch.len_utf8())
        .last()?;
    let start = input[..digits_end].parse::<i64>().ok()?;
    if let Some(after_comma) = input[digits_end..].strip_prefix(',') {
        let count_end = after_comma
            .char_indices()
            .take_while(|(_, ch)| ch.is_ascii_digit())
            .map(|(index, ch)| index + ch.len_utf8())
            .last()?;
        let count = after_comma[..count_end].parse::<i64>().ok()?;
        return Some(((start, Some(count)), &after_comma[count_end..]));
    }
    Some(((start, None), &input[digits_end..]))
}

/// Splits on `\r\n`, `\n` or `\r`, keeping a trailing empty segment (Java `split("\\R", -1)`).
fn split_lines(content: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    let bytes = content.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'\n' => {
                lines.push(&content[start..index]);
                index += 1;
                start = index;
            }
            b'\r' => {
                lines.push(&content[start..index]);
                index += if bytes.get(index + 1) == Some(&b'\n') {
                    2
                } else {
                    1
                };
                start = index;
            }
            _ => index += 1,
        }
    }
    lines.push(&content[start..]);
    lines
}

/// An authorized immutable review snapshot; no filesystem or repository is discovered implicitly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JevReviewInput {
    /// The revision under review (bounded, non-blank).
    pub revision: String,
    /// Whether the snapshot is a change set or a whole codebase.
    pub mode: ReviewMode,
    /// Source files, each with its related test paths.
    pub files: Vec<ReviewFile>,
    /// Test evidence referenced by the source files.
    pub tests: Vec<TestEvidence>,
}

/// How a [`JevReviewInput`] was assembled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum ReviewMode {
    /// Only the changed files.
    Changes,
    /// The whole codebase.
    Codebase,
}

/// One source file in a review snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFile {
    /// Normalized relative path.
    pub path: String,
    /// File contents.
    pub text: String,
    /// Paths of tests covering this file.
    pub related_tests: Vec<String>,
}

/// Test evidence referenced by a [`ReviewFile`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestEvidence {
    /// Normalized relative path.
    pub path: String,
    /// Test contents.
    pub text: String,
}

impl JevReviewInput {
    /// Validates and normalizes a review snapshot (`JevReviewInput` compact constructor).
    ///
    /// Enforces: bounded non-blank revision, unique source paths, unique/non-overlapping test
    /// paths, and every `related_tests` entry present in the snapshot.
    pub fn new(
        revision: impl Into<String>,
        mode: ReviewMode,
        files: Vec<ReviewFile>,
        tests: Vec<TestEvidence>,
    ) -> Result<Self, JevError> {
        let revision = revision.into();
        if revision.trim().is_empty() || revision.chars().count() > 512 {
            return Err(JevError::invalid("bounded revision required"));
        }
        for file in &files {
            require_relative_path(&file.path)?;
            let mut seen = std::collections::BTreeSet::new();
            for related in &file.related_tests {
                require_relative_path(related)?;
                if !seen.insert(related.clone()) {
                    return Err(JevError::invalid("duplicate related test path"));
                }
            }
        }
        for test in &tests {
            require_relative_path(&test.path)?;
        }

        let source_paths: std::collections::BTreeSet<&str> =
            files.iter().map(|file| file.path.as_str()).collect();
        if source_paths.len() != files.len() {
            return Err(JevError::invalid("duplicate source path"));
        }
        let mut test_paths = std::collections::BTreeSet::new();
        for test in &tests {
            if !test_paths.insert(test.path.as_str()) || source_paths.contains(test.path.as_str()) {
                return Err(JevError::invalid("duplicate or overlapping test path"));
            }
        }
        for file in &files {
            if !file
                .related_tests
                .iter()
                .all(|related| test_paths.contains(related.as_str()))
            {
                return Err(JevError::invalid("related test absent from snapshot"));
            }
        }
        Ok(Self {
            revision,
            mode,
            files,
            tests,
        })
    }
}

/// Requires a normalized relative path (no leading `/`, no `\\`, no `:`, no control chars,
/// no empty/`.`/`..` segments, not empty, at most 2048 chars).
fn require_relative_path(path: &str) -> Result<(), JevError> {
    let invalid = path.trim().is_empty()
        || path.chars().count() > 2048
        || path.starts_with('/')
        || path.ends_with('/')
        || path.contains('\\')
        || path.contains(':')
        || path.chars().any(|ch| ch.is_control())
        || path
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..");
    if invalid {
        return Err(JevError::invalid(
            "normalized relative source path required",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_regions_have_deterministic_boundaries_and_no_phantom_final_line() {
        let regions = source_regions("one\ntwo\nthree\n", 2).unwrap();
        assert_eq!(regions.len(), 2);
        assert_eq!(
            regions[0],
            Region {
                id: "R1".into(),
                start_line: 1,
                end_line: 2,
                side: Side::New,
                text: "one\ntwo".into(),
            }
        );
        assert_eq!(regions[1].start_line, 3);
        assert_eq!(regions[1].end_line, 3);
        assert_eq!(regions[1].text, "three");

        assert!(source_regions(" ", 80).unwrap().is_empty());
        assert!(source_regions("", 80).unwrap().is_empty());
        assert_eq!(
            source_regions("a", 80).unwrap()[0].end_line,
            1,
            "a file without a trailing newline is one line"
        );
        assert_eq!(source_regions("a\r\nb\r\nc\r\n", 2).unwrap().len(), 2);
    }

    #[test]
    fn source_regions_reject_non_positive_length() {
        let error = source_regions("one\n", 0).unwrap_err();
        assert!(error.is_invalid_argument());
        assert_eq!(error.message(), "positive region length required");
    }

    #[test]
    fn parses_multiple_hunks_and_deletion_uses_old_side() {
        let regions = patch_regions(
            "@@ -2,2 +2,2 @@\n context\n-old\n+new\n@@ -10,2 +10,0 @@\n-deleted\n-line\n",
        )
        .unwrap();
        assert_eq!(regions.len(), 2);
        assert_eq!(regions[0].id, "hunk_1");
        assert_eq!(regions[0].start_line, 2);
        assert_eq!(regions[0].end_line, 3);
        assert_eq!(regions[0].side, Side::New);
        assert_eq!(regions[1].id, "hunk_2");
        assert_eq!(regions[1].side, Side::Old);
        assert_eq!(regions[1].start_line, 10);
        assert_eq!(regions[1].end_line, 11);
    }

    #[test]
    fn patch_accepts_file_preamble_and_no_newline_marker() {
        let regions = patch_regions(
            "diff --git a/f b/f\nindex 123..456 100644\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-old\n\\ No newline at end of file\n+new\n",
        )
        .unwrap();
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].start_line, 1);
        assert_eq!(regions[0].end_line, 1);
        assert!(regions[0].text.contains("\\ No newline at end of file"));
    }

    #[test]
    fn rejects_malformed_binary_and_combined_diffs_instead_of_inventing_line_numbers() {
        for patch in [
            "@@ -1,2 +1,1 @@\n-old\n+new",
            "Binary files differ",
            "@@@ -1 +1 +1 @@@\n+x",
            "@@ -0,1 +0,1 @@\n-x\n+y",
            "@@ -1 +2147483647,2 @@\n-x\n+y\n+z",
            "diff --git a/f b/f\ndiff --git a/g b/g\n",
        ] {
            let error = patch_regions(patch).unwrap_err();
            assert!(error.is_invalid_argument(), "{patch:?} -> {error}");
        }
        assert_eq!(
            patch_regions("Binary files differ").unwrap_err().message(),
            "unsupported diff preamble"
        );
        assert_eq!(
            patch_regions("@@ -0,1 +0,1 @@\n-x\n+y")
                .unwrap_err()
                .message(),
            "invalid diff range"
        );
        assert_eq!(
            patch_regions("@@ -1,2 +1,1 @@\n-old\n+new")
                .unwrap_err()
                .message(),
            "diff hunk counts do not match content"
        );
    }

    #[test]
    fn blank_patches_have_no_regions() {
        assert!(patch_regions("").unwrap().is_empty());
        assert!(patch_regions("   \n").unwrap().is_empty());
    }

    fn file(path: &str, related: Vec<&str>) -> ReviewFile {
        ReviewFile {
            path: path.into(),
            text: "code".into(),
            related_tests: related.into_iter().map(String::from).collect(),
        }
    }

    #[test]
    fn input_rejects_duplicate_paths_traversal_and_dangling_test_references() {
        for path in [
            "../source.py",
            "/source.py",
            "a/../b",
            "a//b",
            "C:\\a",
            "a\nb",
            "",
            "a/",
        ] {
            let error =
                JevReviewInput::new("r", ReviewMode::Codebase, vec![file(path, vec![])], vec![])
                    .unwrap_err();
            assert_eq!(
                error.message(),
                "normalized relative source path required",
                "{path:?}"
            );
        }

        let duplicate = file("source.py", vec![]);
        let error = JevReviewInput::new(
            "r",
            ReviewMode::Codebase,
            vec![duplicate.clone(), duplicate],
            vec![],
        )
        .unwrap_err();
        assert_eq!(error.message(), "duplicate source path");

        let error = JevReviewInput::new(
            "r",
            ReviewMode::Codebase,
            vec![file("source.py", vec!["test.py"])],
            vec![],
        )
        .unwrap_err();
        assert_eq!(error.message(), "related test absent from snapshot");

        let error = JevReviewInput::new(
            "r",
            ReviewMode::Codebase,
            vec![file("source.py", vec!["test.py", "test.py"])],
            vec![TestEvidence {
                path: "test.py".into(),
                text: "test".into(),
            }],
        )
        .unwrap_err();
        assert_eq!(error.message(), "duplicate related test path");

        let error = JevReviewInput::new(
            "r",
            ReviewMode::Codebase,
            vec![file("source.py", vec![])],
            vec![TestEvidence {
                path: "source.py".into(),
                text: "test".into(),
            }],
        )
        .unwrap_err();
        assert_eq!(error.message(), "duplicate or overlapping test path");

        // Blank and oversized revisions are rejected.
        for revision in ["", "   "] {
            let error =
                JevReviewInput::new(revision, ReviewMode::Changes, vec![], vec![]).unwrap_err();
            assert_eq!(error.message(), "bounded revision required");
        }
        let error =
            JevReviewInput::new("r".repeat(513), ReviewMode::Changes, vec![], vec![]).unwrap_err();
        assert_eq!(error.message(), "bounded revision required");
    }

    #[test]
    fn input_snapshot_preserves_caller_order_and_reserializes() {
        let input = JevReviewInput::new(
            "r",
            ReviewMode::Codebase,
            vec![file("source.py", vec!["test.py"])],
            vec![TestEvidence {
                path: "test.py".into(),
                text: "test".into(),
            }],
        )
        .unwrap();
        assert_eq!(input.files.len(), 1);
        assert_eq!(input.files[0].related_tests, vec!["test.py".to_string()]);

        let json = serde_json::to_value(&input).unwrap();
        assert_eq!(json["mode"], serde_json::json!("CODEBASE"));
        let restored: JevReviewInput = serde_json::from_value(json).unwrap();
        assert_eq!(restored, input);
    }

    #[test]
    fn regions_and_sides_round_trip_through_serde() {
        let regions = source_regions("one\ntwo", 1).unwrap();
        let json = serde_json::to_value(&regions).unwrap();
        assert_eq!(json[0]["start_line"], serde_json::json!(1));
        let restored: Vec<Region> = serde_json::from_value(json).unwrap();
        assert_eq!(restored, regions);

        assert_eq!(
            serde_json::to_value(Side::Old).unwrap(),
            serde_json::json!("OLD")
        );
    }
}
