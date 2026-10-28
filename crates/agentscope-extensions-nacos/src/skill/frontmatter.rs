//! Skill ZIP parsing and Nacos YAML-frontmatter normalization, mirroring
//! `SkillUtil#createFromZip` + `NacosSkillRepository`'s private helpers.
//!
//! Nacos-exported `SKILL.md` files often use indented YAML continuation lines
//! (no `key:` prefix). AgentScope's frontmatter parser only accepts flat
//! `key: value` lines, so this module folds continuations into the previous key
//! and rewrites the ZIP before it is turned into an [`AgentSkill`].
//!
//! Package layout follows Java `SkillUtil#createFromZip` exactly:
//! * every entry must live under one shared root directory;
//! * `SKILL.md` must sit directly under that root;
//! * resource keys are the **paths relative to the root** (`scripts/run.sh`), and
//!   `SKILL.md` itself is not a resource;
//! * `name` / `description` / the markdown body after the frontmatter are required.

use std::io::{Cursor, Read, Write};

use agentscope_harness::agent::skill::model::AgentSkill;
use serde_json::{Map, Value};
use zip::read::ZipArchive;
use zip::write::FileOptions;
use zip::{CompressionMethod, ZipWriter};

const SKILL_MD: &str = "SKILL.md";

/// Java `SkillUtil` required-metadata message.
const MISSING_METADATA_MESSAGE: &str =
    "The SKILL.md must have a YAML Front Matter including `name` and `description` fields.";
/// Java `SkillUtil` empty-body message.
const EMPTY_CONTENT_MESSAGE: &str =
    "The SKILL.md must have content except for the YAML Front Matter.";

/// Creates an [`AgentSkill`] from a skill ZIP, mirroring
/// `SkillUtil.createFromZip(bytes, source)`.
///
/// The package must contain a `SKILL.md` entry directly under a single root
/// directory, a non-blank `name` / `description` in its YAML frontmatter and a
/// non-empty body. Any other entry becomes a resource keyed by its path relative
/// to that root directory.
pub fn create_from_zip(zip_bytes: &[u8], source: &str) -> Result<AgentSkill, String> {
    if zip_bytes.is_empty() {
        return Err("Zip content cannot be null or empty.".to_string());
    }
    let mut archive = ZipArchive::new(Cursor::new(zip_bytes))
        .map_err(|e| format!("Failed to read skill zip content: {e}"))?;

    let mut root_dir: Option<String> = None;
    let mut skill_md: Option<String> = None;
    let mut resources: Vec<(String, Vec<u8>)> = Vec::new();

    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|e| format!("Failed to read skill zip content: {e}"))?;
        if entry.is_dir() {
            continue;
        }
        let entry_name = normalize_entry_name(entry.name())?;
        let mut data = Vec::new();
        entry
            .read_to_end(&mut data)
            .map_err(|e| format!("Failed to read skill zip content: {e}"))?;

        // Java: `int separatorIndex = entryName.indexOf('/')`, `<= 0` is invalid.
        let Some(separator_index) = entry_name.find('/').filter(|index| *index > 0) else {
            return Err("Zip entries must be under a single root directory.".to_string());
        };
        let entry_root = &entry_name[..separator_index];
        match root_dir.as_deref() {
            None => root_dir = Some(entry_root.to_string()),
            Some(root) if root != entry_root => {
                return Err("Zip entries must share the same root directory.".to_string());
            }
            Some(_) => {}
        }

        let expected_skill_entry = format!("{entry_root}/{SKILL_MD}");
        if entry_name.ends_with(&format!("/{SKILL_MD}")) && entry_name != expected_skill_entry {
            return Err("SKILL.md must be located directly under the root directory.".to_string());
        }
        if entry_name == expected_skill_entry {
            if skill_md.is_some() {
                return Err("Multiple SKILL.md entries found in zip content.".to_string());
            }
            // Java decodes every entry with the zip charset (lossy); mirror that.
            skill_md = Some(String::from_utf8_lossy(&data).into_owned());
            continue;
        }

        // Resource keys are relative to the root directory.
        let resource_name = entry_name[entry_root.len() + 1..].to_string();
        resources.push((resource_name, data));
    }

    let skill_md = skill_md.ok_or_else(|| "SKILL.md not found in zip content.".to_string())?;
    build_skill(&skill_md, resources, source)
}

/// Builds the [`AgentSkill`], mirroring `SkillUtil.createFrom(skillMd, resources, source)`.
fn build_skill(
    skill_md: &str,
    resources: Vec<(String, Vec<u8>)>,
    source: &str,
) -> Result<AgentSkill, String> {
    let parsed = parse_skill_md(skill_md);

    // Java `stringifyRequiredMetadata`: the value must be a non-blank string.
    let name = required_metadata(&parsed.metadata, "name")?;
    let description = required_metadata(&parsed.metadata, "description")?;
    if parsed.content.is_empty() {
        return Err(EMPTY_CONTENT_MESSAGE.to_string());
    }

    let mut metadata = parsed.metadata;
    metadata.insert("name".to_string(), Value::String(name.clone()));
    metadata.insert(
        "description".to_string(),
        Value::String(description.clone()),
    );

    let mut builder = AgentSkill::builder()
        .name(name)
        .source(source)
        .description(description)
        .metadata(metadata);

    for (path, data) in resources {
        match String::from_utf8(data) {
            Ok(text) => builder = builder.add_resource(path, text),
            Err(error) => {
                // Binary resource: Java decodes it lossily; keeping the bytes
                // base64-encoded is lossless and understood by the harness.
                use base64::Engine as _;
                let encoded = base64::engine::general_purpose::STANDARD.encode(error.into_bytes());
                builder = builder.add_resource(path, format!("base64:{encoded}"));
            }
        }
    }

    Ok(builder.build())
}

/// Java `stringifyRequiredMetadata`: a missing, non-string or blank value fails.
fn required_metadata(metadata: &Map<String, Value>, key: &str) -> Result<String, String> {
    metadata
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| MISSING_METADATA_MESSAGE.to_string())
}

/// `MarkdownSkillParser.parse` result: frontmatter metadata plus the body.
struct ParsedSkillMd {
    metadata: Map<String, Value>,
    content: String,
}

/// Parses `name` / `description` / metadata from `SKILL.md` frontmatter.
fn parse_skill_md(markdown: &str) -> ParsedSkillMd {
    let text = markdown.strip_prefix('\u{feff}').unwrap_or(markdown);

    let Some(after_open) = text.strip_prefix("---") else {
        return ParsedSkillMd {
            metadata: Map::new(),
            content: text.to_string(),
        };
    };
    // The opening delimiter must be alone on its line.
    let Some(after_open) = strip_first_line_break(after_open) else {
        return ParsedSkillMd {
            metadata: Map::new(),
            content: text.to_string(),
        };
    };
    let Some(yaml_end) = find_frontmatter_end(after_open) else {
        return ParsedSkillMd {
            metadata: Map::new(),
            content: text.to_string(),
        };
    };

    let yaml = &after_open[..yaml_end];
    let mut metadata = Map::new();
    if !yaml.trim().is_empty() {
        for line in yaml.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once(':') {
                metadata.insert(key.trim().to_string(), Value::String(unquote(value.trim())));
            }
        }
    }

    let body_start = yaml_end + "---".len();
    ParsedSkillMd {
        metadata,
        content: strip_leading_blank_lines(&after_open[body_start..]).to_string(),
    }
}

/// Strips the rest of the opening delimiter's line, returning everything after the
/// line break (`None` when the delimiter is not followed by a line break).
fn strip_first_line_break(text: &str) -> Option<&str> {
    let trimmed = text.trim_start_matches([' ', '\t']);
    let rest = trimmed
        .strip_prefix("\r\n")
        .or_else(|| trimmed.strip_prefix('\n'))?;
    Some(rest)
}

/// Byte offset of the closing `---` line of the frontmatter.
fn find_frontmatter_end(text: &str) -> Option<usize> {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if line.trim() == "---" {
            return Some(offset);
        }
        offset += line.len();
    }
    None
}

/// Drops leading blank lines from the markdown body, mirroring the Java regex's
/// `(?:\s*[\r\n]+)?` after the closing delimiter.
fn strip_leading_blank_lines(text: &str) -> &str {
    let mut rest = text;
    while let Some((line, tail)) = split_first_line(rest) {
        if !line.trim().is_empty() {
            return rest;
        }
        rest = tail;
    }
    rest
}

fn split_first_line(text: &str) -> Option<(&str, &str)> {
    let index = text.find('\n')?;
    Some((&text[..index], &text[index + 1..]))
}

fn unquote(value: &str) -> String {
    let v = value.trim();
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        v[1..v.len() - 1].replace("\\\"", "\"")
    } else if v.len() >= 2 && v.starts_with('\'') && v.ends_with('\'') {
        v[1..v.len() - 1].to_string()
    } else {
        v.to_string()
    }
}

// ---------------------------------------------------------------------------
// Nacos YAML frontmatter adaptation
// ---------------------------------------------------------------------------

/// Normalizes Nacos-exported skill ZIPs whose `SKILL.md` uses folded YAML
/// frontmatter, mirroring `adaptNacosSkillZipForYamlFrontmatter`.
///
/// Returns the original bytes unchanged when no adaptation is needed or when the
/// ZIP cannot be inspected/re-packed.
pub fn adapt_nacos_skill_zip(zip_bytes: &[u8]) -> Vec<u8> {
    let Ok(mut archive) = ZipArchive::new(Cursor::new(zip_bytes)) else {
        return zip_bytes.to_vec();
    };

    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    let mut skill_path: Option<String> = None;
    let mut skill_md: Option<String> = None;

    for index in 0..archive.len() {
        let Ok(mut entry) = archive.by_index(index) else {
            return zip_bytes.to_vec();
        };
        if entry.is_dir() {
            continue;
        }
        let Ok(name) = normalize_entry_name(entry.name()) else {
            return zip_bytes.to_vec();
        };
        let mut data = Vec::new();
        if entry.read_to_end(&mut data).is_err() {
            return zip_bytes.to_vec();
        }
        if is_root_skill_md(&name) {
            if let Some(existing) = &skill_path {
                if existing != &name {
                    // Ambiguous: multiple root SKILL.md entries; leave as-is.
                    return zip_bytes.to_vec();
                }
            }
            skill_path = Some(name.clone());
            skill_md = String::from_utf8(data.clone()).ok();
        }
        entries.push((name, data));
    }

    let (Some(skill_path), Some(skill_md)) = (skill_path, skill_md) else {
        return zip_bytes.to_vec();
    };

    let adapted_md = adapt_skill_md_frontmatter(&skill_md);
    if adapted_md == skill_md {
        return zip_bytes.to_vec();
    }

    for (name, data) in entries.iter_mut() {
        if name == &skill_path {
            *data = adapted_md.clone().into_bytes();
        }
    }

    repack_zip(&entries).unwrap_or_else(|| zip_bytes.to_vec())
}

fn repack_zip(entries: &[(String, Vec<u8>)]) -> Option<Vec<u8>> {
    let mut buffer = Vec::new();
    {
        let mut writer = ZipWriter::new(Cursor::new(&mut buffer));
        let options = FileOptions::default().compression_method(CompressionMethod::Deflated);
        for (name, data) in entries {
            writer.start_file(name, options).ok()?;
            writer.write_all(data).ok()?;
        }
        writer.finish().ok()?;
    }
    Some(buffer)
}

/// Normalizes only the frontmatter; the body is left unchanged, mirroring
/// `adaptSkillMdFrontmatter`.
fn adapt_skill_md_frontmatter(markdown: &str) -> String {
    if markdown.is_empty() {
        return markdown.to_string();
    }
    let text = markdown.strip_prefix('\u{feff}').unwrap_or(markdown);
    if !text.starts_with("---") {
        return markdown.to_string();
    }

    let lines: Vec<&str> = split_lines_preserve_trailing(text);
    if lines.first().map(|l| l.trim()) != Some("---") {
        return markdown.to_string();
    }
    let Some(yaml_end) = (1..lines.len()).find(|&i| lines[i].trim() == "---") else {
        return markdown.to_string();
    };

    let yaml_lines = &lines[1..yaml_end];
    let normalized_yaml = normalize_folded_flat_yaml(yaml_lines);

    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&normalized_yaml);
    out.push_str("---");
    for line in &lines[yaml_end + 1..] {
        out.push('\n');
        out.push_str(line);
    }
    out
}

/// Splits on any line break, preserving trailing empties (mirrors Java's
/// `split("\\R", -1)`).
fn split_lines_preserve_trailing(text: &str) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                out.push(&text[start..i]);
                i += 1;
                start = i;
            }
            b'\r' => {
                out.push(&text[start..i]);
                i += if bytes.get(i + 1) == Some(&b'\n') {
                    2
                } else {
                    1
                };
                start = i;
            }
            _ => i += 1,
        }
    }
    out.push(&text[start..]);
    out
}

/// Folds lines that are not `key: value` into the previous key's value,
/// mirroring `normalizeFoldedFlatYaml`.
fn normalize_folded_flat_yaml(yaml_lines: &[&str]) -> String {
    let mut emit = String::new();
    let mut pending_key: Option<String> = None;
    let mut pending_val: Option<String> = None;

    for raw in yaml_lines {
        let t = raw.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        match parse_yaml_kv(t) {
            Some((key, value)) => {
                flush_yaml_kv_line(&mut emit, pending_key.take(), pending_val.take());
                pending_key = Some(key);
                pending_val = Some(value.trim().to_string());
            }
            None => {
                if pending_key.is_some() {
                    pending_val = match pending_val {
                        None => Some(t.to_string()),
                        Some(v) if v.is_empty() => Some(t.to_string()),
                        Some(v) => Some(format!("{v} {t}")),
                    };
                }
            }
        }
    }
    flush_yaml_kv_line(&mut emit, pending_key, pending_val);
    emit
}

/// Parses a flat `key: value` line, mirroring the Java `YAML_KV` pattern
/// `^([a-zA-Z_][a-zA-Z0-9_-]*)\s*:\s*(.*)$`.
fn parse_yaml_kv(line: &str) -> Option<(String, String)> {
    let (key_part, value_part) = line.split_once(':')?;
    let key = key_part.trim_end();
    if key.is_empty() {
        return None;
    }
    let mut chars = key.chars();
    let first = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return None;
    }
    Some((key.to_string(), value_part.trim_start().to_string()))
}

fn flush_yaml_kv_line(sb: &mut String, key: Option<String>, value: Option<String>) {
    let Some(key) = key else {
        return;
    };
    sb.push_str(&key);
    sb.push_str(": ");
    match value {
        None => sb.push('\n'),
        Some(v) if v.is_empty() => sb.push('\n'),
        Some(v) => {
            if yaml_value_needs_quoting(&v) {
                sb.push('"');
                append_yaml_double_quoted_escaped(sb, &v);
                sb.push('"');
            } else {
                sb.push_str(&v);
            }
            sb.push('\n');
        }
    }
}

/// Mirrors `MarkdownSkillParser.SimpleYamlParser#needsQuoting`.
fn yaml_value_needs_quoting(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    if value.contains(':')
        || value.contains('#')
        || value.contains('\n')
        || value.contains('\r')
        || value.contains('\t')
    {
        return true;
    }
    let first = value.chars().next().unwrap();
    let last = value.chars().last().unwrap();
    if first.is_whitespace() || last.is_whitespace() {
        return true;
    }
    matches!(
        first,
        '"' | '\'' | '[' | ']' | '{' | '}' | '>' | '|' | '*' | '&' | '!' | '%' | '@' | '`'
    )
}

fn append_yaml_double_quoted_escaped(sb: &mut String, value: &str) {
    for c in value.chars() {
        match c {
            '\\' => sb.push_str("\\\\"),
            '"' => sb.push_str("\\\""),
            '\n' => sb.push_str("\\n"),
            '\r' => sb.push_str("\\r"),
            '\t' => sb.push_str("\\t"),
            other => sb.push(other),
        }
    }
}

fn is_root_skill_md(name: &str) -> bool {
    // Exactly one path segment then SKILL.md, mirroring `ROOT_SKILL_MD`.
    match name.split_once('/') {
        Some((parent, rest)) => !parent.is_empty() && !parent.contains('/') && rest == SKILL_MD,
        None => false,
    }
}

/// Normalizes a zip entry name, mirroring `SkillUtil#normalizeZipEntryName`.
fn normalize_entry_name(name: &str) -> Result<String, String> {
    if name.is_empty() {
        return Err("Zip entry name cannot be null or empty.".to_string());
    }
    let normalized = name.replace('\\', "/");
    if normalized.starts_with('/') {
        return Err("Zip entry name must be a relative path.".to_string());
    }
    if normalized.split('/').any(|segment| segment == "..") {
        return Err("Zip entry name must not contain parent directory segments.".to_string());
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_zip(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut w = ZipWriter::new(Cursor::new(&mut buf));
            let opts = FileOptions::default().compression_method(CompressionMethod::Deflated);
            for (name, content) in entries {
                w.start_file(*name, opts).unwrap();
                w.write_all(content.as_bytes()).unwrap();
            }
            w.finish().unwrap();
        }
        buf
    }

    fn make_binary_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut w = ZipWriter::new(Cursor::new(&mut buf));
            let opts = FileOptions::default().compression_method(CompressionMethod::Deflated);
            for (name, content) in entries {
                w.start_file(*name, opts).unwrap();
                w.write_all(content).unwrap();
            }
            w.finish().unwrap();
        }
        buf
    }

    #[test]
    fn parses_skill_from_zip() {
        let md = "---\nname: demo_skill\ndescription: A demo\n---\n# Body\nUse it.";
        let zip = make_zip(&[("demo_skill/SKILL.md", md), ("demo_skill/extra.txt", "hi")]);
        let skill = create_from_zip(&zip, "nacos@public").unwrap();
        assert_eq!(skill.name(), "demo_skill");
        assert_eq!(skill.description(), "A demo");
        assert_eq!(skill.source(), "nacos@public");
        // Resource keys are relative to the root directory (Java `createFromZip`).
        assert!(skill.resources().contains_key("extra.txt"));
        // SKILL.md itself is not a resource.
        assert!(!skill.resources().contains_key("SKILL.md"));
        assert!(skill.metadata().contains_key("name"));
        assert!(skill.metadata().contains_key("description"));
    }

    #[test]
    fn missing_skill_md_errors() {
        let zip = make_zip(&[("demo/readme.txt", "x")]);
        let err = create_from_zip(&zip, "s").unwrap_err();
        assert_eq!(err, "SKILL.md not found in zip content.");
    }

    #[test]
    fn entries_without_a_single_root_are_rejected() {
        // Top-level file: Java `separatorIndex <= 0`.
        let md = "---\nname: n\ndescription: d\n---\nbody";
        let err = create_from_zip(&make_zip(&[("SKILL.md", md)]), "s").unwrap_err();
        assert_eq!(err, "Zip entries must be under a single root directory.");

        // Two different roots.
        let zip = make_zip(&[("a/SKILL.md", md), ("b/extra.txt", "x")]);
        let err = create_from_zip(&zip, "s").unwrap_err();
        assert_eq!(err, "Zip entries must share the same root directory.");
    }

    #[test]
    fn nested_skill_md_is_rejected() {
        let md = "---\nname: n\ndescription: d\n---\nbody";
        let zip = make_zip(&[("root/sub/SKILL.md", md)]);
        let err = create_from_zip(&zip, "s").unwrap_err();
        assert_eq!(
            err,
            "SKILL.md must be located directly under the root directory."
        );
    }

    #[test]
    fn parent_directory_segments_are_rejected() {
        let md = "---\nname: n\ndescription: d\n---\nbody";
        let zip = make_zip(&[("root/SKILL.md", md), ("root/../evil.txt", "x")]);
        let err = create_from_zip(&zip, "s").unwrap_err();
        assert_eq!(
            err,
            "Zip entry name must not contain parent directory segments."
        );
    }

    #[test]
    fn missing_name_is_rejected() {
        // Mirrors Java `SkillUtil.createFrom`: name/description are required.
        let md = "---\ndescription: only desc\n---\nbody";
        let zip = make_zip(&[("f/SKILL.md", md)]);
        let err = create_from_zip(&zip, "s").unwrap_err();
        assert_eq!(err, MISSING_METADATA_MESSAGE);
    }

    #[test]
    fn missing_description_is_rejected() {
        let md = "---\nname: n\ndescription: \"   \"\n---\nbody";
        let zip = make_zip(&[("f/SKILL.md", md)]);
        let err = create_from_zip(&zip, "s").unwrap_err();
        assert_eq!(err, MISSING_METADATA_MESSAGE);
    }

    #[test]
    fn empty_body_is_rejected() {
        let md = "---\nname: n\ndescription: d\n---\n\n";
        let zip = make_zip(&[("f/SKILL.md", md)]);
        let err = create_from_zip(&zip, "s").unwrap_err();
        assert_eq!(err, EMPTY_CONTENT_MESSAGE);
    }

    #[test]
    fn empty_zip_is_rejected() {
        assert_eq!(
            create_from_zip(&[], "s").unwrap_err(),
            "Zip content cannot be null or empty."
        );
    }

    #[test]
    fn folds_indented_yaml_continuations() {
        let md =
            "---\nname: multi\ndescription: first\n  second line\n  third\nversion: 1\n---\nbody";
        let zip = make_zip(&[("multi/SKILL.md", md)]);
        let adapted = adapt_nacos_skill_zip(&zip);
        let mut archive = ZipArchive::new(Cursor::new(&adapted)).unwrap();
        let mut content = String::new();
        archive
            .by_name("multi/SKILL.md")
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert!(
            content.contains("description: first second line third"),
            "ACTUAL CONTENT:\n{content}"
        );
        assert!(content.contains("name: multi"));
    }

    #[test]
    fn flat_yaml_is_left_untouched() {
        let md = "---\nname: flat\ndescription: simple\n---\nbody";
        let zip = make_zip(&[("flat/SKILL.md", md)]);
        let adapted = adapt_nacos_skill_zip(&zip);
        assert_eq!(adapted, zip);
    }

    #[test]
    fn quotes_values_containing_colons() {
        // `needsQuoting` mirrors MarkdownSkillParser's SimpleYamlParser.
        let md = "---\nname: q\ndescription: see: http://x\n---\nbody";
        let zip = make_zip(&[("q/SKILL.md", md)]);
        let adapted = adapt_nacos_skill_zip(&zip);
        let mut archive = ZipArchive::new(Cursor::new(&adapted)).unwrap();
        let mut content = String::new();
        archive
            .by_name("q/SKILL.md")
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert!(
            content.contains("description: \"see: http://x\""),
            "ACTUAL:\n{content}"
        );
    }

    #[test]
    fn missing_frontmatter_is_left_untouched() {
        let md = "# No frontmatter\njust body";
        let zip = make_zip(&[("n/SKILL.md", md)]);
        assert_eq!(adapt_nacos_skill_zip(&zip), zip);
    }

    #[test]
    fn resource_paths_are_relative_to_the_root() {
        let md = "---\nname: r\ndescription: d\n---\nbody";
        let zip = make_zip(&[("r/SKILL.md", md), ("r/scripts/run.sh", "echo hi")]);
        let skill = create_from_zip(&zip, "s").unwrap();
        assert_eq!(
            skill.resources().get("scripts/run.sh").map(String::as_str),
            Some("echo hi")
        );
    }

    #[test]
    fn backslash_entry_names_are_normalized() {
        let md = "---\nname: b\ndescription: d\n---\nbody";
        let zip = make_zip(&[("b\\SKILL.md", md), ("b\\docs\\a.txt", "x")]);
        let skill = create_from_zip(&zip, "s").unwrap();
        assert_eq!(
            skill.resources().get("docs/a.txt").map(String::as_str),
            Some("x")
        );
    }

    #[test]
    fn binary_resources_are_base64_encoded() {
        let md = "---\nname: bn\ndescription: d\n---\nbody";
        let zip = make_binary_zip(&[("bn/SKILL.md", md.as_bytes()), ("bn/logo.bin", &[0xff])]);
        let skill = create_from_zip(&zip, "s").unwrap();
        assert_eq!(
            skill.resources().get("logo.bin").map(String::as_str),
            Some("base64:/w==")
        );
    }
}
