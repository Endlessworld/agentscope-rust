//! Token-saving compaction for shell (`execute`) output.
//!
//! Search-ish commands (`rg`, `grep -rn`, `git grep -n`, `findstr /n`) print
//! `path:line:text` once per match, so a long workspace-relative path is repeated for
//! every matching line. In a typical `rg` result the path is over half the characters,
//! and each `/` boundary tends to become its own token for the model.
//!
//! [`compact_grep_like`] rewrites such output into a `--heading`-like layout:
//!
//! ```text
//! [paths grouped; N:<text> = line N of the path above]
//! src/a.rs
//! 10:alpha
//! 12:beta
//! ```
//!
//! The transformation is deliberately **conservative**: it only touches runs of at
//! least two consecutive lines that name the same plausible path, never reorders
//! lines, never rewrites the match text, and returns the input unchanged (byte for
//! byte) whenever anything is uncertain or the rewrite would not shrink the output.
//!
//! A two-character line gutter is tolerated and kept verbatim: PowerShell's
//! `Select-String -Context` (the obvious Windows spelling of `grep -C`) renders matches
//! as `> path:line:text` and context lines as `  path:line:text`, so the path is not at
//! column 0 and the match/context marker must survive the rewrite.
//!
//! PowerShell's formatter also **hard-wraps** any result line wider than the host width,
//! cutting `  path:line:text` into two physical lines with no marker (`  …\src\v1\clie`
//! followed by `nt.rs:365:    pub fn meta…`). [`unwrap_wrapped_lines`] rejoins those
//! segments before parsing; otherwise the second segment looks like a relative
//! `nt.rs:365:` match and the real path never repeats.

use std::borrow::Cow;
use std::sync::OnceLock;

use agentscope_core::message::JsonMap;
use regex::Regex;

/// Environment variable that turns compaction off (`0`/`false`/`off`/`no`).
pub const OUTPUT_COMPACT_ENV: &str = "AGENTSCOPE_OUTPUT_COMPACT";

/// Marker that opens a compacted result; also used for idempotency.
const NOTE_PREFIX: &str = "[paths grouped";

/// Longest line gutter accepted in front of the path (`> ` for a `Select-String` match
/// line, `  ` for a context line). Capped so that indented code excerpts stay rejected.
const MAX_GUTTER_CHARS: usize = 2;

/// Shortest physical segment assumed to be the head of a formatter-wrapped result line.
///
/// A wrapped head fills the host's output width, so it is always long; the floor keeps
/// ordinary short prose (e.g. `> note: …`) out of the joining logic.
const MIN_WRAPPED_SEGMENT_CHARS: usize = 60;

/// Most physical segments joined back into one logical result line.
const MAX_WRAP_SEGMENTS: usize = 4;

/// Extensionless names a search tool may legitimately report at the workspace root.
const BARE_NAMES: &[&str] = &[
    "Makefile",
    "makefile",
    "GNUmakefile",
    "Dockerfile",
    "Containerfile",
    "Jenkinsfile",
    "Vagrantfile",
    "Rakefile",
    "Gemfile",
    "LICENSE",
    "COPYING",
    "NOTICE",
    "README",
    "CHANGELOG",
];

/// Settings for [`compact_grep_like`].
#[derive(Debug, Clone)]
pub struct CompactConfig {
    /// Master switch.
    pub enabled: bool,
    /// Minimum number of output lines before compaction is attempted.
    pub min_lines: usize,
    /// Minimum length of a same-path run before its path is hoisted (2 = the path
    /// appears on two consecutive match lines).
    pub min_occurrences: usize,
    /// Minimum characters saved before the rewritten text is used. `0` (the default)
    /// means "any strict saving wins", leaving [`Self::min_occurrences`] as the real
    /// gate: a rewrite that would grow or keep the output unchanged is never applied.
    pub min_savings_chars: usize,
    /// Longest path (in characters) accepted as a match prefix.
    pub max_path_chars: usize,
    /// Lines longer than this (in bytes) are never treated as matches.
    pub max_line_chars: usize,
    /// Strip ANSI colour/style escapes before parsing (`rg --color=always`).
    pub strip_ansi: bool,
}

impl Default for CompactConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_lines: 2,
            min_occurrences: 2,
            min_savings_chars: 0,
            max_path_chars: 240,
            max_line_chars: 8 * 1024,
            strip_ansi: true,
        }
    }
}

impl CompactConfig {
    /// Defaults, with [`OUTPUT_COMPACT_ENV`] able to switch compaction off.
    pub fn from_env() -> Self {
        let mut config = Self::default();
        if let Ok(raw) = std::env::var(OUTPUT_COMPACT_ENV) {
            let raw = raw.trim().to_ascii_lowercase();
            if matches!(raw.as_str(), "0" | "false" | "off" | "no") {
                config.enabled = false;
            }
        }
        config
    }

    /// A config that never rewrites anything.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }
}

/// Outcome of one compaction pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactReport {
    /// Text to show the model (identical to the input when not compacted).
    pub text: String,
    /// Whether `text` differs from the input.
    pub compacted: bool,
    /// Number of hoisted path headings.
    pub hoisted_files: usize,
    /// Number of match lines rendered as `N:<text>`.
    pub hoisted_lines: usize,
    /// Characters removed (`input_chars - output_chars`).
    pub saved_chars: usize,
}

/// Tool-result metadata key holding the characters saved by a compaction pass.
pub const METADATA_SAVED_CHARS: &str = "agentscope.output_compact.savedChars";
/// Tool-result metadata key holding the number of hoisted path headings.
pub const METADATA_HOISTED_FILES: &str = "agentscope.output_compact.hoistedFiles";
/// Tool-result metadata key holding the number of `N:<text>` match lines.
pub const METADATA_HOISTED_LINES: &str = "agentscope.output_compact.hoistedLines";

impl CompactReport {
    fn unchanged(text: &str) -> Self {
        Self {
            text: text.to_string(),
            compacted: false,
            hoisted_files: 0,
            hoisted_lines: 0,
            saved_chars: 0,
        }
    }

    /// Records the pass in tool-result metadata; a no-op when nothing was compacted.
    ///
    /// Shared by every tool that compacts its output (`execute`, `grep_files`) so the
    /// metadata keys stay in one place.
    pub fn record_metadata(&self, metadata: &mut JsonMap) {
        if !self.compacted {
            return;
        }
        metadata.insert(
            METADATA_SAVED_CHARS.to_string(),
            serde_json::Value::from(self.saved_chars as u64),
        );
        metadata.insert(
            METADATA_HOISTED_FILES.to_string(),
            serde_json::Value::from(self.hoisted_files as u64),
        );
        metadata.insert(
            METADATA_HOISTED_LINES.to_string(),
            serde_json::Value::from(self.hoisted_lines as u64),
        );
    }
}

/// Collapses repeated `path:` prefixes in grep-like output into per-path headings.
///
/// Returns the input untouched when it does not look like `path:line:text` output,
/// when no path repeats on two consecutive lines, or when the rewrite would not
/// strictly shrink the output (see [`CompactConfig::min_savings_chars`]).
pub fn compact_grep_like(text: &str, config: &CompactConfig) -> CompactReport {
    if !config.enabled || text.is_empty() {
        return CompactReport::unchanged(text);
    }
    // Already compacted (or produced by a tool that headings its own output).
    if text.contains(NOTE_PREFIX) {
        return CompactReport::unchanged(text);
    }
    let stripped;
    let source = if config.strip_ansi && text.contains('\u{1b}') {
        stripped = strip_ansi(text);
        stripped.as_str()
    } else {
        text
    };
    let Some(body) = compact_body(source, config) else {
        return CompactReport::unchanged(text);
    };
    // Signed on purpose: a rewrite that grows the output must never win, and
    // `saturating_sub` on unsigned lengths would hide exactly that case.
    let saved_chars = text.chars().count() as i64 - body.text.chars().count() as i64;
    if saved_chars <= 0 || saved_chars < config.min_savings_chars as i64 {
        return CompactReport::unchanged(text);
    }
    CompactReport {
        text: body.text,
        compacted: true,
        hoisted_files: body.files,
        hoisted_lines: body.lines,
        saved_chars: saved_chars.max(0) as usize,
    }
}

struct Body {
    text: String,
    files: usize,
    lines: usize,
}

/// One parsed `path:line:<text>` line; `tail` is `"<line>:<text>"` verbatim and
/// `gutter` is the leading `> ` / `  ` marker kept from the input line.
struct MatchLine<'a> {
    gutter: &'a str,
    path: &'a str,
    tail: &'a str,
}

fn compact_body(source: &str, config: &CompactConfig) -> Option<Body> {
    let raw_lines: Vec<&str> = source
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    let lines = unwrap_wrapped_lines(&raw_lines, config);
    if lines.len() < config.min_lines {
        return None;
    }
    let parsed: Vec<Option<MatchLine<'_>>> = lines
        .iter()
        .map(|line| parse_match_line(line, config))
        .collect();
    if parsed.iter().all(Option::is_none) {
        return None;
    }

    let mut out = String::with_capacity(source.len());
    let mut files = 0usize;
    let mut hoisted = 0usize;
    let mut index = 0usize;
    while index < lines.len() {
        let Some(path) = parsed[index].as_ref().map(|m| m.path) else {
            append(&mut out, lines[index].as_ref());
            index += 1;
            continue;
        };
        // Extend over the whole run of the same path (maximal, so ordering is kept).
        let mut end = index + 1;
        while end < lines.len() && parsed[end].as_ref().map(|m| m.path) == Some(path) {
            end += 1;
        }
        if end - index >= config.min_occurrences {
            // A heading has no `:<digits>:` segment, so it can never be mistaken for a
            // match line even when it directly follows inline matches.
            append(&mut out, path);
            for slot in &parsed[index..end] {
                // Every slot in the run is `Some` by construction.
                if let Some(m) = slot {
                    out.push('\n');
                    out.push_str(m.gutter);
                    out.push_str(m.tail);
                }
            }
            files += 1;
            hoisted += end - index;
        } else {
            for line in &lines[index..end] {
                append(&mut out, line.as_ref());
            }
        }
        index = end;
    }

    if hoisted == 0 {
        return None;
    }

    let mut text = String::with_capacity(out.len() + 48);
    text.push_str("[paths grouped; N:<text> = line N of the path above]");
    text.push('\n');
    text.push_str(&out);
    Some(Body {
        text,
        files,
        lines: hoisted,
    })
}

fn append(out: &mut String, line: &str) {
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(line);
}

/// Rejoins the physical lines PowerShell's output formatter wrapped.
///
/// `Select-String -Context` prints one line per match; a line wider than the host width is
/// hard-wrapped into several physical lines with no marker, e.g.
///
/// ```text
///   C:\…\src\v1\clie
/// nt.rs:365:    pub fn meta(mut self, …)
/// ```
///
/// Parse that verbatim and the second segment reads as a relative `nt.rs:365:` match, so
/// compaction hoists a bogus `nt.rs` heading and leaves the real prefix untouched. A join
/// is attempted only when all of the following hold, which keeps ordinary output — and
/// `rg` / `grep` output, which is never wrapped — byte-identical:
///
/// * the first segment does not parse as a result line on its own;
/// * it starts with a `Select-String` gutter (`> `/`  `) and carries no whitespace after
///   the gutter, i.e. it looks like a path prefix cut mid-token;
/// * it is at least [`MIN_WRAPPED_SEGMENT_CHARS`] long, so short prose is left alone;
/// * the joined text parses as `path:line:text`.
///
/// The tail may itself span more segments (a very long match text), so joining continues
/// up to [`MAX_WRAP_SEGMENTS`] while segments stay long enough to have been wrapped.
fn unwrap_wrapped_lines<'a>(lines: &[&'a str], config: &CompactConfig) -> Vec<Cow<'a, str>> {
    let mut out: Vec<Cow<'a, str>> = Vec::with_capacity(lines.len());
    let mut index = 0usize;
    while index < lines.len() {
        match join_wrapped_line(lines, index, config) {
            Some((joined, next)) => {
                out.push(Cow::Owned(joined));
                index = next;
            }
            None => {
                out.push(Cow::Borrowed(lines[index]));
                index += 1;
            }
        }
    }
    out
}

/// Joins the wrapped result line starting at `start`; `None` keeps the line verbatim.
///
/// Returns the joined text and the index of the first line *after* the joined run.
fn join_wrapped_line(
    lines: &[&str],
    start: usize,
    config: &CompactConfig,
) -> Option<(String, usize)> {
    let first = lines[start];
    if parse_match_line(first, config).is_some() {
        return None;
    }
    let (gutter, body) = split_gutter(first);
    if gutter.is_empty()
        || body.chars().count() < MIN_WRAPPED_SEGMENT_CHARS
        || body.chars().any(char::is_whitespace)
    {
        return None;
    }
    let mut joined = String::from(first);
    let mut last = start;
    while last + 1 < lines.len() && last + 1 - start < MAX_WRAP_SEGMENTS {
        let next = lines[last + 1];
        joined.push_str(next);
        last += 1;
        if parse_match_line(&joined, config).is_some() {
            return Some((joined, last + 1));
        }
        // A short segment cannot itself have been wrapped, so the join must be wrong:
        // bail out instead of swallowing an unrelated line.
        if next.chars().count() < MIN_WRAPPED_SEGMENT_CHARS {
            return None;
        }
    }
    None
}

/// Parses `path:line:<text>` (or `path:line:col:<text>`), keeping the tail verbatim.
///
/// Only the path and the line number are interpreted: everything after the line
/// number's colon is copied as-is, so a VIm-style `path:line:col:text` result is
/// preserved without having to disambiguate a column from text that starts with one.
///
/// A [`split_gutter`] prefix is skipped before looking for the path, so
/// `Select-String` results (`> path:1:text`, `  path:1:text`) parse like `rg` results.
fn parse_match_line<'a>(line: &'a str, config: &CompactConfig) -> Option<MatchLine<'a>> {
    if line.len() > config.max_line_chars || line.starts_with('[') {
        return None;
    }
    let (gutter, body) = split_gutter(line);
    let mut search_from = 0usize;
    let mut probes = 0usize;
    while let Some(offset) = body[search_from..].find(':') {
        let colon = search_from + offset;
        probes += 1;
        if probes > 16 {
            return None;
        }
        let path = &body[..colon];
        if is_plausible_path(path, config.max_path_chars) {
            let rest = &body[colon + 1..];
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            if digits > 0 && digits <= 9 && rest[digits..].starts_with(':') {
                return Some(MatchLine {
                    gutter,
                    path,
                    tail: rest,
                });
            }
            // The first plausible path is the only candidate worth trying.
            return None;
        }
        search_from = colon + 1;
    }
    None
}

/// Splits the `Select-String`-style line gutter (`> ` match, `  ` context) off a line.
///
/// Returns `("", line)` for every other producer, which keeps `rg` / `grep` parsing
/// byte-identical to before.
fn split_gutter(line: &str) -> (&str, &str) {
    let gutter = line
        .bytes()
        .take(MAX_GUTTER_CHARS)
        .take_while(|b| matches!(b, b' ' | b'>'))
        .count();
    line.split_at(gutter)
}

/// Whether `path` could be the file part of a search result.
///
/// Rejects log lines (`12:30:`, `docker: command not found`), compiler diagnostics
/// (`error[E0308]:`) and indented continuations, while accepting workspace-relative,
/// absolute (both separators) and bare `name.ext` paths.
fn is_plausible_path(path: &str, max_chars: usize) -> bool {
    if path.is_empty() || path.chars().count() > max_chars {
        return false;
    }
    if path.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    if path.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return false;
    }
    // `http://host` / `ftp://host` are URLs, not files.
    if path.contains("://") {
        return false;
    }
    if path.contains('/') || path.contains('\\') {
        return true;
    }
    match path.rsplit_once('.') {
        Some((stem, ext)) => {
            !stem.is_empty()
                && !ext.is_empty()
                && ext.len() <= 8
                && ext.chars().all(|c| c.is_ascii_alphanumeric())
        }
        None => BARE_NAMES
            .iter()
            .any(|name| name.eq_ignore_ascii_case(path)),
    }
}

fn strip_ansi(text: &str) -> String {
    static CSI: OnceLock<Regex> = OnceLock::new();
    let re = CSI.get_or_init(|| {
        Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").expect("CSI escape regex is valid")
    });
    re.replace_all(text, "").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> CompactConfig {
        // Short fixtures: relax the size floors, keep every other rule at its default.
        CompactConfig {
            min_lines: 2,
            min_savings_chars: 1,
            ..CompactConfig::default()
        }
    }

    #[test]
    fn hoists_repeated_paths_and_keeps_single_lines_inline() {
        let input = "\
crates/harness/src/agent/session/storage.rs:10:alpha
crates/harness/src/agent/session/storage.rs:12:beta
crates/harness/src/agent/session/storage.rs:14:gamma
crates/harness/src/agent/session/store.rs:3:one
crates/harness/src/agent/session/store.rs:4:two
crates/harness/src/agent/tool/grep.rs:7:delta
crates/harness/src/agent/tool/grep.rs:9:epsilon
crates/harness/src/agent/tool/grep.rs:11:zeta";
        let out = compact_grep_like(input, &cfg());
        assert!(out.compacted, "{}", out.text);
        assert_eq!(out.hoisted_files, 3);
        assert_eq!(out.hoisted_lines, 8);
        assert_eq!(
            out.text,
            "\
[paths grouped; N:<text> = line N of the path above]
crates/harness/src/agent/session/storage.rs
10:alpha
12:beta
14:gamma
crates/harness/src/agent/session/store.rs
3:one
4:two
crates/harness/src/agent/tool/grep.rs
7:delta
9:epsilon
11:zeta"
        );
    }

    #[test]
    fn a_path_seen_once_stays_inline() {
        let input = "\
crates/harness/src/agent/session/storage.rs:10:alpha
crates/harness/src/agent/session/storage.rs:12:beta
crates/harness/src/agent/session/storage.rs:14:gamma
crates/harness/src/agent/session/sqlite.rs:25:pub struct SqliteSessionStorage {
crates/harness/src/agent/session/sqlite.rs:75:impl AtomicSessionStorage for SqliteSessionStorage {
crates/harness/src/agent/tool/grep.rs:7:delta";
        let out = compact_grep_like(input, &cfg());
        assert!(out.compacted, "{}", out.text);
        assert_eq!(out.hoisted_files, 2);
        assert_eq!(out.hoisted_lines, 5);
        assert!(out.text.ends_with(
            "crates/harness/src/agent/session/storage.rs\n10:alpha\n12:beta\n14:gamma\n\
crates/harness/src/agent/session/sqlite.rs\n25:pub struct SqliteSessionStorage {\n\
75:impl AtomicSessionStorage for SqliteSessionStorage {\n\
crates/harness/src/agent/tool/grep.rs:7:delta"
        ));
    }

    #[test]
    fn short_paths_do_not_beat_the_header_cost() {
        // Hoisting 8-char paths saves less than the one-time note; the guard keeps the
        // original so a "saving" can never make the context longer.
        let input = "\
a/x.rs:10:alpha
a/x.rs:12:beta
a/x.rs:14:gamma
c/x.rs:7:delta
c/x.rs:9:epsilon
c/x.rs:11:zeta";
        let out = compact_grep_like(input, &cfg());
        assert!(!out.compacted, "{}", out.text);
        assert_eq!(out.text, input);
    }

    #[test]
    fn real_workspace_paths_with_mixed_separators() {
        let input = "\
crates/agentscope-harness/src/agent/session\\store.rs:25:pub struct StoreSessionStorage {
crates/agentscope-harness/src/agent/session\\storage.rs:25:pub struct WorkspaceSessionStorage {
crates/agentscope-harness/src/agent/session\\storage.rs:85:        let temp = format!(\"{path}.tmp-{}\", uuid::Uuid::new_v4());
crates/agentscope-harness/src/agent/session\\storage.rs:86:        self.fs.write(ctx, &temp, content).await?;
crates/agentscope-harness/src/agent/session\\storage.rs:122:impl AtomicSessionStorage for WorkspaceSessionStorage {
crates/agentscope-harness/src/agent/session\\storage.rs:298:    fn temp_dir(tag: &str) -> std::path::PathBuf {";
        let out = compact_grep_like(input, &cfg());
        assert!(out.compacted);
        assert!(out.text.contains(
            "\ncrates/agentscope-harness/src/agent/session\\storage.rs\n25:pub struct WorkspaceSessionStorage {"
        ));
        assert!(out
            .text
            .contains("\n122:impl AtomicSessionStorage for WorkspaceSessionStorage {"));
        // The run of one stays inline.
        assert!(out.text.contains(
            "\ncrates/agentscope-harness/src/agent/session\\store.rs:25:pub struct StoreSessionStorage {"
        ));
    }

    #[test]
    fn non_search_output_is_returned_untouched() {
        let input = "\
error[E0308]: mismatched types
   --> src/main.rs:10:5
    |
10  |     let x: u8 = \"a\";
    |            --   ^^^ expected `u8`, found `&str`

warning: unused variable: `y`
  = note: `#[warn(unused_variables)]` on by default";
        let out = compact_grep_like(input, &cfg());
        assert!(!out.compacted);
        assert_eq!(out.text, input);
    }

    #[test]
    fn log_lines_and_bare_words_are_not_paths() {
        let input = "\
docker: command not found
note: see https://example.com/docs
12:30:01 INFO starting
12:30:02 INFO ready
12:30:03 INFO done";
        let out = compact_grep_like(input, &cfg());
        assert!(!out.compacted, "{}", out.text);
    }

    #[test]
    fn vimgrep_tail_is_preserved_verbatim() {
        let input = "\
crates/harness/src/agent/session/storage.rs:10:5:let x = 1;
crates/harness/src/agent/session/storage.rs:12:9:let y = 2;
crates/harness/src/agent/session/storage.rs:14:1:let z = 3;";
        let out = compact_grep_like(input, &cfg());
        assert!(out.compacted);
        assert!(out.text.ends_with(
            "crates/harness/src/agent/session/storage.rs\n10:5:let x = 1;\n12:9:let y = 2;\n14:1:let z = 3;"
        ));
    }

    #[test]
    fn text_that_starts_with_a_number_survives() {
        let input = "\
crates/harness/src/agent/session/storage.rs:7:1:2 ratio
crates/harness/src/agent/session/storage.rs:8:1:2 ratio
crates/harness/src/agent/session/storage.rs:9:1:2 ratio";
        let out = compact_grep_like(input, &cfg());
        assert!(out.compacted);
        assert!(out.text.ends_with(
            "crates/harness/src/agent/session/storage.rs\n7:1:2 ratio\n8:1:2 ratio\n9:1:2 ratio"
        ));
    }

    #[test]
    fn powershell_select_string_gutter_is_hoisted() {
        // `Select-String -Context` (the Windows `grep -C`) prints matches as
        // `> path:line:text` in reverse video and context lines as `  path:line:text`.
        let path = r"crates\agentscope-extensions-a2a-client\src\model.rs";
        let input = [
            format!("\u{1b}[7m\u{1b}[0m  {path}:253:"),
            format!("  {path}:254:/// A2A message."),
            format!("> {path}:256:\u{1b}[7mpub struct Message\u{1b}[0m {{\u{1b}[0m"),
            format!("\u{1b}[7m\u{1b}[0m  {path}:257:    #[serde(rename = \"messageId\")]\u{1b}[0m"),
            format!("\u{1b}[7m\u{1b}[0m  {path}:258:    pub message_id: String,\u{1b}[0m"),
        ]
        .join("\n");
        let out = compact_grep_like(&input, &cfg());
        assert!(out.compacted, "{}", out.text);
        assert_eq!(out.hoisted_files, 1);
        assert_eq!(out.hoisted_lines, 5);
        assert_eq!(
            out.text,
            [
                "[paths grouped; N:<text> = line N of the path above]",
                path,
                "  253:",
                "  254:/// A2A message.",
                "> 256:pub struct Message {",
                "  257:    #[serde(rename = \"messageId\")]",
                "  258:    pub message_id: String,",
            ]
            .join("\n")
        );
    }

    #[test]
    fn wrapped_select_string_output_is_rejoined_before_hoisting() {
        // Real PowerShell shape: the output formatter cuts every `Select-String` result
        // line at the host width (119 chars = `  ` + 117) with no marker, and match lines
        // carry ANSI reverse-video escapes.
        let path = r"C:\Users\Endless\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\agent-client-protocol-schema-1.15.1\src\v1\client.rs";
        let cut = 117;
        let (head, rest) = path.split_at(cut);
        let input = [
            format!("  {head}"),
            format!("{rest}:87:    /// See protocol docs"),
            format!("  {head}"),
            format!("{rest}:88:    #[must_use]"),
            format!("> {head}"),
            format!("{rest}:89:    \u{1b}[7mpub fn meta\u{1b}[0m(mut self) -> Self {{"),
            format!("\u{1b}[7m\u{1b}[0m  {head}"),
            format!("\u{1b}[7m\u{1b}[0m{rest}:90:        self.meta\u{1b}[0m"),
        ]
        .join("\n");
        let out = compact_grep_like(&input, &cfg());
        assert!(out.compacted, "{}", out.text);
        assert_eq!(out.hoisted_files, 1);
        assert_eq!(out.hoisted_lines, 4);
        assert_eq!(
            out.text,
            [
                "[paths grouped; N:<text> = line N of the path above]",
                path,
                "  87:    /// See protocol docs",
                "  88:    #[must_use]",
                "> 89:    pub fn meta(mut self) -> Self {",
                "  90:        self.meta",
            ]
            .join("\n")
        );
        // The wrapped tail (`t.rs`) must never become a heading of its own.
        assert!(!out.text.contains("\nt.rs\n"));
    }

    #[test]
    fn a_long_gutterless_line_is_not_joined_to_the_next_result() {
        // Only gutter-led segments are candidates; `rg` output has no gutter and is never
        // wrapped, so an unrelated long line must not be glued to the next result.
        let prose = "x".repeat(80);
        let path = "crates/agentscope-harness/src/agent/session/storage.rs";
        let input =
            format!("{prose}\n{path}:1:alpha\n{path}:2:beta\n{path}:3:gamma\n{path}:4:delta");
        let out = compact_grep_like(&input, &cfg());
        assert!(out.compacted, "{}", out.text);
        assert!(out.text.contains(&format!(
            "{prose}\n{path}\n1:alpha\n2:beta\n3:gamma\n4:delta"
        )));
    }

    #[test]
    fn a_short_gutter_line_is_not_joined() {
        let path = "crates/agentscope-harness/src/agent/session/storage.rs";
        let input = format!("> a.rs\n{path}:1:alpha\n{path}:2:beta\n{path}:3:gamma");
        let out = compact_grep_like(&input, &cfg());
        assert!(out.compacted, "{}", out.text);
        assert!(out.text.contains("> a.rs"));
        assert!(out
            .text
            .contains(&format!("\n{path}\n1:alpha\n2:beta\n3:gamma")));
    }

    #[test]
    fn quoted_prose_with_a_gutter_is_not_a_path() {
        let input = "> note: run cargo test\n> todo: fix later";
        let out = compact_grep_like(input, &cfg());
        assert!(!out.compacted, "{}", out.text);
        assert_eq!(out.text, input);
    }

    #[test]
    fn is_idempotent() {
        let input = "\
crates/harness/src/agent/session/storage.rs:10:alpha
crates/harness/src/agent/session/storage.rs:12:beta
crates/harness/src/agent/session/storage.rs:14:gamma
crates/harness/src/agent/tool/grep.rs:7:delta
crates/harness/src/agent/tool/grep.rs:9:epsilon
crates/harness/src/agent/tool/grep.rs:11:zeta";
        let once = compact_grep_like(input, &cfg());
        assert!(once.compacted);
        let twice = compact_grep_like(&once.text, &cfg());
        assert!(!twice.compacted);
        assert_eq!(twice.text, once.text);
    }

    #[test]
    fn crlf_input_is_normalised_when_compacted() {
        let path = "crates/harness/src/agent/session/storage.rs";
        let input = format!("{path}:1:a\r\n{path}:2:b\r\n{path}:3:c\r\n{path}:4:d");
        let out = compact_grep_like(&input, &cfg());
        assert!(out.compacted);
        assert!(!out.text.contains('\r'));
        assert_eq!(
            out.text,
            format!(
                "[paths grouped; N:<text> = line N of the path above]\n{path}\n1:a\n2:b\n3:c\n4:d"
            )
        );
    }

    #[test]
    fn ansi_color_codes_are_stripped() {
        let path = "crates/harness/src/agent/session/storage.rs";
        let input = format!(
            "\u{1b}[35m{path}\u{1b}[0m\u{1b}[36m:1\u{1b}[0m:one\n\
\u{1b}[35m{path}\u{1b}[0m\u{1b}[36m:2\u{1b}[0m:two\n\
\u{1b}[35m{path}\u{1b}[0m\u{1b}[36m:3\u{1b}[0m:three"
        );
        let out = compact_grep_like(&input, &cfg());
        assert!(out.compacted, "{}", out.text);
        assert!(!out.text.contains('\u{1b}'));
        assert!(out
            .text
            .ends_with(&format!("{path}\n1:one\n2:two\n3:three")));
    }

    #[test]
    fn windows_absolute_paths_are_hoisted() {
        let input = "\
C:\\work\\proj\\crates\\harness\\src\\main.rs:4:fn main() {
C:\\work\\proj\\crates\\harness\\src\\main.rs:9:    run();
C:\\work\\proj\\crates\\harness\\src\\main.rs:12:}";
        let out = compact_grep_like(input, &cfg());
        assert!(out.compacted);
        assert!(out.text.ends_with(
            "C:\\work\\proj\\crates\\harness\\src\\main.rs\n4:fn main() {\n9:    run();\n12:}"
        ));
    }

    #[test]
    fn short_output_below_min_lines_is_never_touched() {
        let path = "crates/harness/src/agent/session/storage.rs";
        let input = format!("{path}:1:a");
        let out = compact_grep_like(&input, &CompactConfig::default());
        assert!(!out.compacted);
        assert_eq!(out.text, input);
    }

    #[test]
    fn a_rewrite_that_would_grow_the_output_is_rejected() {
        // Two short lines: hoisting them cannot pay for the one-time header, so the
        // strict-shrink guard keeps the original.
        let input = "a/x.rs:10:alpha\na/x.rs:12:beta";
        let out = compact_grep_like(input, &cfg());
        assert!(!out.compacted, "{}", out.text);
        assert_eq!(out.text, input);
    }

    #[test]
    fn a_raised_savings_floor_disables_the_rewrite() {
        let path = "crates/harness/src/agent/session/storage.rs";
        let input = format!("{path}:1:a\n{path}:2:b\n{path}:3:c");
        let config = CompactConfig {
            min_savings_chars: 200,
            ..cfg()
        };
        let out = compact_grep_like(&input, &config);
        assert!(!out.compacted, "{}", out.text);
        assert_eq!(out.text, input);
    }

    #[test]
    fn a_two_line_run_is_hoisted() {
        // The default threshold is two consecutive matches of the same path.
        let input = "\
crates/harness/src/agent/session/storage.rs:10:alpha
crates/harness/src/agent/session/storage.rs:12:beta
crates/harness/src/agent/tool/grep.rs:7:delta
crates/harness/src/agent/tool/grep.rs:9:epsilon";
        let out = compact_grep_like(input, &cfg());
        assert!(out.compacted, "{}", out.text);
        assert_eq!(out.hoisted_files, 2);
        assert_eq!(
            out.text,
            "\
[paths grouped; N:<text> = line N of the path above]
crates/harness/src/agent/session/storage.rs
10:alpha
12:beta
crates/harness/src/agent/tool/grep.rs
7:delta
9:epsilon"
        );
    }

    #[test]
    fn a_raised_occurrence_floor_keeps_short_runs_inline() {
        let input = "\
crates/harness/src/agent/session/storage.rs:10:alpha
crates/harness/src/agent/session/storage.rs:12:beta
crates/harness/src/agent/tool/grep.rs:7:delta
crates/harness/src/agent/tool/grep.rs:9:epsilon
crates/harness/src/agent/tool/grep.rs:11:zeta
crates/harness/src/agent/tool/grep.rs:13:eta";
        let config = CompactConfig {
            min_occurrences: 3,
            ..cfg()
        };
        let out = compact_grep_like(input, &config);
        assert!(out.compacted);
        // The 2-line run stays inline; the 4-line run is hoisted.
        assert!(out
            .text
            .contains("crates/harness/src/agent/session/storage.rs:10:alpha"));
        assert!(out
            .text
            .contains("crates/harness/src/agent/session/storage.rs:12:beta"));
        assert!(out
            .text
            .contains("\ncrates/harness/src/agent/tool/grep.rs\n7:delta"));
    }

    #[test]
    fn urls_are_not_paths() {
        let input = "\
https://example.com/docs:1:a
https://example.com/docs:2:b
https://example.com/docs:3:c";
        let out = compact_grep_like(input, &cfg());
        assert!(!out.compacted, "{}", out.text);
    }

    #[test]
    fn disabled_config_is_a_no_op() {
        let input = "crates/harness/src/agent/session/storage.rs:1:a\ncrates/harness/src/agent/session/storage.rs:2:b\ncrates/harness/src/agent/session/storage.rs:3:c\ncrates/harness/src/agent/session/storage.rs:4:d";
        let out = compact_grep_like(input, &CompactConfig::disabled());
        assert!(!out.compacted);
        assert_eq!(out.text, input);
    }
}
