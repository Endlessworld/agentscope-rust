//! Shared filesystem utilities, mirroring
//! `io.agentscope.harness.agent.filesystem.util.FilesystemUtils`.

/// Extensions treated as binary by [`get_file_type`].
const BINARY_EXTENSIONS: &[&str] = &[
    ".png", ".jpg", ".jpeg", ".gif", ".webp", ".heic", ".heif", ".bmp", ".ico", ".svg", ".mp4",
    ".mpeg", ".mov", ".avi", ".flv", ".mpg", ".webm", ".wmv", ".3gpp", ".wav", ".mp3", ".aiff",
    ".aac", ".ogg", ".flac", ".pdf", ".ppt", ".pptx", ".doc", ".docx", ".xls", ".xlsx", ".zip",
    ".tar", ".gz", ".bz2", ".7z", ".rar", ".class", ".jar", ".war", ".ear", ".so", ".dll",
    ".dylib", ".exe",
];

/// Classifies a file as `"text"` or `"binary"` based on its extension.
pub fn get_file_type(path: &str) -> &'static str {
    let dot = match path.rfind('.') {
        Some(i) => i,
        None => return "text",
    };
    let ext = path[dot..].to_ascii_lowercase();
    if BINARY_EXTENSIONS.contains(&ext.as_str()) {
        "binary"
    } else {
        "text"
    }
}

/// Counts non-overlapping occurrences of `sub` in `text`.
pub fn count_occurrences(text: &str, sub: &str) -> usize {
    if sub.is_empty() {
        return 0;
    }
    let mut count = 0;
    let mut idx = 0;
    while let Some(pos) = text[idx..].find(sub) {
        count += 1;
        idx += pos + sub.len();
    }
    count
}

/// Performs a validated string replacement.
///
/// Returns `Ok((new_content, occurrences))` on success, or `Err(message)` when the
/// old string is absent or ambiguous (multiple matches without `replace_all`).
pub fn perform_string_replacement(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Result<(String, usize), String> {
    let occurrences = count_occurrences(content, old_string);
    if occurrences == 0 {
        return Err(format!("Error: String not found in file: '{old_string}'"));
    }
    if occurrences > 1 && !replace_all {
        return Err(format!(
            "Error: String '{old_string}' appears {occurrences} times in file. \
             Use replaceAll=true to replace all instances, or provide a more specific \
             string with surrounding context."
        ));
    }
    let new_content = if replace_all {
        content.replace(old_string, new_string)
    } else {
        match content.find(old_string) {
            Some(idx) => {
                let mut out = String::with_capacity(content.len() + new_string.len());
                out.push_str(&content[..idx]);
                out.push_str(new_string);
                out.push_str(&content[idx + old_string.len()..]);
                out
            }
            None => return Err(format!("Error: String not found in file: '{old_string}'")),
        }
    };
    Ok((new_content, occurrences))
}

/// Shell-quotes a string for safe interpolation into a POSIX shell command.
pub fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_type_classifies_by_extension() {
        assert_eq!(get_file_type("/a/b.txt"), "text");
        assert_eq!(get_file_type("/a/b.PNG"), "binary");
        assert_eq!(get_file_type("/a/noext"), "text");
        assert_eq!(get_file_type("archive.tar"), "binary");
    }

    #[test]
    fn count_occurrences_is_non_overlapping() {
        assert_eq!(count_occurrences("aaaa", "aa"), 2);
        assert_eq!(count_occurrences("abcabc", "abc"), 2);
        assert_eq!(count_occurrences("abc", "z"), 0);
        assert_eq!(count_occurrences("abc", ""), 0);
    }

    #[test]
    fn replacement_errors_when_not_found() {
        let err = perform_string_replacement("abc", "z", "y", false).unwrap_err();
        assert!(err.contains("not found"));
    }

    #[test]
    fn replacement_rejects_ambiguity_without_replace_all() {
        let err = perform_string_replacement("a a", "a", "b", false).unwrap_err();
        assert!(err.contains("appears 2 times"));
    }

    #[test]
    fn replacement_replaces_single_occurrence() {
        let (out, n) = perform_string_replacement("a b c", "b", "Z", false).unwrap();
        assert_eq!(n, 1);
        assert_eq!(out, "a Z c");
    }

    #[test]
    fn replacement_replaces_all() {
        let (out, n) = perform_string_replacement("a a", "a", "b", true).unwrap();
        assert_eq!(n, 2);
        assert_eq!(out, "b b");
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
    }
}
