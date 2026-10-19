//! Fuzziness ladder for patch/edit operations, ported from `FuzzyTextMatcher`.
//!
//! The LLM rarely reproduces whitespace exactly, so a strict `str::find` would reject valid
//! matches. The matcher tries progressively looser comparisons and reports which level matched:
//! `Exact` -> `TrailingWsStripped` -> `WhitespaceCollapsed`. Each looser level keeps a
//! per-character map back to the original string so patches apply to the unmodified bytes.

/// Match strictness level (ordered most-strict-first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Exact,
    TrailingWsStripped,
    WhitespaceCollapsed,
}

/// A match in `existing`: original-string byte offsets, inclusive-exclusive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchRange {
    pub start: usize,
    pub end: usize,
    pub level: Level,
}

impl MatchRange {
    pub fn len(&self) -> usize {
        self.end - self.start
    }
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

/// Outcome bundle for the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub matches: Vec<MatchRange>,
    pub level: Level,
}

impl SearchResult {
    pub fn is_empty(&self) -> bool {
        self.matches.is_empty()
    }
}
/// Searches `existing` for `needle`, starting at the strictest level and walking looser
/// ladders until at least one match is found.
pub fn search(existing: &str, needle: &str) -> SearchResult {
    if existing.is_empty() || needle.is_empty() {
        return SearchResult { matches: Vec::new(), level: Level::Exact };
    }
    let exact = find_all(existing, needle, Level::Exact);
    if !exact.is_empty() {
        return SearchResult { matches: exact, level: Level::Exact };
    }
    let existing_tws = strip_trailing_whitespace(existing);
    let needle_tws = strip_trailing_whitespace(needle);
    let tws = find_all_normalized(&existing_tws, &needle_tws, Level::TrailingWsStripped);
    if !tws.is_empty() {
        return SearchResult { matches: tws, level: Level::TrailingWsStripped };
    }
    let existing_wsc = collapse_whitespace(existing);
    let needle_wsc = collapse_whitespace(needle);
    let wsc = find_all_normalized(&existing_wsc, &needle_wsc, Level::WhitespaceCollapsed);
    if !wsc.is_empty() {
        return SearchResult { matches: wsc, level: Level::WhitespaceCollapsed };
    }
    SearchResult { matches: Vec::new(), level: Level::Exact }
}

fn find_all(haystack: &str, needle: &str, level: Level) -> Vec<MatchRange> {
    let mut out = Vec::new();
    let mut idx = 0;
    while let Some(rel) = haystack[idx..].find(needle) {
        let start = idx + rel;
        let end = start + needle.len();
        out.push(MatchRange { start, end, level });
        idx = end;
    }
    out
}

struct Normalized {
    text: String,
    original_index: Vec<usize>,
    original_len: usize,
}

fn find_all_normalized(haystack: &Normalized, needle: &Normalized, level: Level) -> Vec<MatchRange> {
    let mut out = Vec::new();
    if needle.text.is_empty() {
        return out;
    }
    let mut idx = 0;
    while let Some(rel) = haystack.text[idx..].find(&needle.text) {
        let start_pos = idx + rel;
        let orig_start = haystack.original_index[start_pos];
        let needle_end_excl = start_pos + needle.text.len();
        let orig_end = if needle_end_excl < haystack.original_index.len() {
            haystack.original_index[needle_end_excl]
        } else {
            haystack.original_len
        };
        out.push(MatchRange { start: orig_start, end: orig_end, level });
        idx = needle_end_excl;
    }
    out
}

/// End (exclusive) of the line body within `[line_start, line_end)`. A `'\r'` that immediately
/// precedes the line's `'\n'` belongs to a CRLF terminator, not to the line body.
///
/// Without this, a Windows-authored file normalises to `"foo\r\n"` while an LF-only needle
/// normalises to `"foo\n"`, so *no* rung of the ladder matches — including
/// `WhitespaceCollapsed`, whose whole purpose is to absorb exactly this kind of difference. The
/// same offset doubles as the map target for the emitted newline, so a match ending at a line
/// boundary stops *before* the whole CRLF sequence instead of swallowing the `'\r'`.
#[allow(clippy::needless_range_loop)]
fn line_body_end(chars: &[char], line_start: usize, line_end: usize) -> usize {
    if line_end < chars.len() && line_end > line_start && chars[line_end - 1] == '\r' {
        line_end - 1
    } else {
        line_end
    }
}

#[allow(clippy::needless_range_loop)]
fn strip_trailing_whitespace(s: &str) -> Normalized {
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let mut out = String::new();
    let mut map = Vec::new();
    let mut i = 0;
    while i < len {
        let line_start = i;
        let mut line_end = i;
        while line_end < len && chars[line_end] != '\n' {
            line_end += 1;
        }
        // CRLF: the carriage return is part of the terminator, not the line body.
        let body_end = line_body_end(&chars, line_start, line_end);
        let mut trim_end = body_end;
        while trim_end > line_start
            && (chars[trim_end - 1] == ' ' || chars[trim_end - 1] == '\t')
        {
            trim_end -= 1;
        }
        for j in line_start..trim_end {
            out.push(chars[j]);
            map.push(j);
        }
        if line_end < len {
            out.push('\n');
            map.push(body_end);
        }
        i = line_end + 1;
    }
    Normalized { text: out, original_index: map, original_len: len }
}

#[allow(clippy::needless_range_loop)]
fn collapse_whitespace(s: &str) -> Normalized {
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let mut out = String::new();
    let mut map = Vec::new();
    let mut i = 0;
    while i < len {
        let line_start = i;
        let mut line_end = i;
        while line_end < len && chars[line_end] != '\n' {
            line_end += 1;
        }
        // CRLF: the carriage return is part of the terminator, not line content.
        let body_end = line_body_end(&chars, line_start, line_end);
        let mut j = line_start;
        while j < body_end && (chars[j] == ' ' || chars[j] == '\t') {
            j += 1;
        }
        let mut in_run = false;
        let mut run_orig_start = 0usize;
        let content_emitted_at = map.len();
        while j < body_end {
            let c = chars[j];
            if c == ' ' || c == '\t' {
                if !in_run {
                    in_run = true;
                    run_orig_start = j;
                }
            } else {
                if in_run {
                    out.push(' ');
                    map.push(run_orig_start);
                    in_run = false;
                }
                out.push(c);
                map.push(j);
            }
            j += 1;
        }
        if line_end < len && map.len() > content_emitted_at {
            out.push('\n');
            map.push(body_end);
        }
        i = line_end + 1;
    }
    Normalized { text: out, original_index: map, original_len: len }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match() {
        let r = search("fn main() {}\n", "main");
        assert_eq!(r.level, Level::Exact);
        assert_eq!(r.matches.len(), 1);
        assert_eq!(&r.matches[0].start, &3usize);
    }

    #[test]
    fn trailing_ws_level() {
        let existing = "hello world  \nfoo";
        let needle = "hello world\nfoo";
        let r = search(existing, needle);
        assert_eq!(r.level, Level::TrailingWsStripped);
        assert_eq!(r.matches.len(), 1);
    }

    #[test]
    fn collapsed_level() {
        let existing = "    hello    world\n";
        let needle = "hello world";
        let r = search(existing, needle);
        assert_eq!(r.level, Level::WhitespaceCollapsed);
        assert!(!r.is_empty());
    }

    #[test]
    fn no_match() {
        let r = search("abcdef", "xyz");
        assert!(r.is_empty());
    }

    #[test]
    fn multiple_exact() {
        let r = search("aa bb aa", "aa");
        assert_eq!(r.level, Level::Exact);
        assert_eq!(r.matches.len(), 2);
    }

    #[test]
    fn crlf_existing_matches_lf_needle() {
        // Core repro: a Windows-authored file normalises to "foo\r\n" while an LF-only needle
        // normalises to "foo\n", so no rung of the ladder used to match.
        let existing = "PREFIX_KEEP\r\nalpha\r\nbeta\r\nSUFFIX_KEEP\r\n";
        let needle = "alpha\nbeta";
        let r = search(existing, needle);
        assert_eq!(r.level, Level::TrailingWsStripped);
        assert_eq!(r.matches.len(), 1);
    }

    #[test]
    fn crlf_match_range_preserves_terminators() {
        // The mapped range must stop *before* the whole CRLF sequence instead of swallowing
        // the '\r' and silently downgrading that line's terminator to LF.
        let existing = "PREFIX_KEEP\r\nalpha \r\nbeta\r\nSUFFIX_KEEP\r\n";
        let needle = "alpha\nbeta";
        let r = search(existing, needle);
        assert_eq!(r.matches.len(), 1);
        let m = &r.matches[0];
        assert_eq!(&existing[m.start..m.end], "alpha \r\nbeta");

        // Splicing the replacement over that range leaves surrounding bytes verbatim.
        let patched = format!("{}ALPHA{}", &existing[..m.start], &existing[m.end..]);
        assert_eq!(patched, "PREFIX_KEEP\r\nALPHA\r\nSUFFIX_KEEP\r\n");
    }

    #[test]
    fn crlf_collapse_level_handles_indent_drift() {
        let existing = "\talpha\t\tbeta\r\n";
        let needle = "alpha beta";
        let r = search(existing, needle);
        assert_eq!(r.level, Level::WhitespaceCollapsed);
        assert_eq!(r.matches.len(), 1);
    }

    #[test]
    fn crlf_behaves_like_lf_twin() {
        let crlf = "PREFIX_KEEP\r\nalpha \r\nbeta\r\nSUFFIX_KEEP\r\n";
        let lf = "PREFIX_KEEP\nalpha \nbeta\nSUFFIX_KEEP\n";
        for needle in ["alpha\nbeta", "SUFFIX_KEEP"] {
            let a = search(crlf, needle);
            let b = search(lf, needle);
            assert_eq!(a.is_empty(), b.is_empty(), "needle {needle}");
            assert_eq!(a.level, b.level, "needle {needle}");
            assert_eq!(a.matches.len(), b.matches.len(), "needle {needle}");
            if let Some(m) = a.matches.first() {
                // Same matched text modulo the CRLF terminator folding.
                assert_eq!(
                    crlf[m.start..m.end].replace('\r', ""),
                    lf[b.matches[0].start..b.matches[0].end],
                    "needle {needle}"
                );
            }
        }
    }
}
