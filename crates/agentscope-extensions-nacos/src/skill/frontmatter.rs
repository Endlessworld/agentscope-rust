//! Skill ZIP parsing and Nacos YAML-frontmatter normalization, mirroring
//! `SkillUtil#createFromZip` + `NacosSkillRepository`'s private helpers.
//!
//! Nacos-exported `SKILL.md` files often use indented YAML continuation lines
//! (no `key:` prefix). AgentScope's frontmatter parser only accepts flat
//! `key: value` lines, so this module folds continuations into the previous key
//! and rewrites the ZIP before it is turned into an [`AgentSkill`].

use std::io::{Cursor, Read, Write};

use agentscope_harness::agent::skill::model::AgentSkill;
use zip::read::ZipArchive;
use zip::write::FileOptions;
use zip::{CompressionMethod, ZipWriter};

const SKILL_MD: &str = "SKILL.md";

/// Creates an [`AgentSkill`] from a skill ZIP, mirroring
/// `SkillUtil.createFromZip(bytes, source)`.
///
/// The package must contain a `SKILL.md` entry (at the root or one directory
/// deep). When `fallback_name` is given, it is used if the parsed frontmatter
/// omits `name`.
pub fn create_from_zip(
    zip_bytes: &[u8],
    source: &str,
    fallback_name: &str,
) -> Result<AgentSkill, String> {
    let mut archive =
        ZipArchive::new(Cursor::new(zip_bytes)).map_err(|e| format!("invalid skill zip: {e}"))?;

    let mut resources: Vec<(String, Vec<u8>)> = Vec::new();
    let mut skill_md: Option<String> = None;

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("failed to read zip entry: {e}"))?;
        if entry.is_dir() {
            continue;
        }
        let name = normalize_entry_name(entry.name());
        let mut data = Vec::new();
        entry
            .read_to_end(&mut data)
            .map_err(|e| format!("failed to read zip entry data: {e}"))?;
        if is_root_skill_md(&name) && skill_md.is_none() {
            skill_md = String::from_utf8(data.clone()).ok();
        }
        resources.push((name, data));
    }

    let skill_md =
        skill_md.ok_or_else(|| format!("skill package must contain a {SKILL_MD} entry"))?;

    let (metadata, name, description) = parse_frontmatter(&skill_md, fallback_name)?;

    let mut builder = AgentSkill::builder()
        .name(name)
        .source(source)
        .description(description)
        .metadata(metadata);

    for (rel, data) in resources {
        match String::from_utf8(data.clone()) {
            Ok(text) => builder = builder.add_resource(rel, text),
            Err(_) => {
                use base64::Engine as _;
                let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
                builder = builder.add_resource(rel, format!("base64:{b64}"));
            }
        }
    }

    Ok(builder.build())
}

/// Parses `name` / `description` / metadata from `SKILL.md` frontmatter.
///
/// Returns `(metadata, name, description)`. `fallback_name` supplies `name`
/// when the frontmatter omits it.
fn parse_frontmatter(
    markdown: &str,
    fallback_name: &str,
) -> Result<(serde_json::Map<String, serde_json::Value>, String, String), String> {
    let text = markdown.strip_prefix('\u{feff}').unwrap_or(markdown);
    let mut metadata = serde_json::Map::new();

    if let Some(rest) = text.strip_prefix("---") {
        let rest = rest.strip_prefix('\n').unwrap_or(rest);
        if let Some(end) = rest.find("\n---") {
            for line in rest[..end].lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some((k, v)) = line.split_once(':') {
                    let key = k.trim().to_string();
                    let value = unquote(v.trim());
                    metadata.insert(key, serde_json::Value::String(value));
                }
            }
        }
    }

    let name = metadata
        .get("name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fallback_name.to_string());
    let description = metadata
        .get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("Skill: {name}"));

    Ok((metadata, name, description))
}

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

    for i in 0..archive.len() {
        let Ok(mut entry) = archive.by_index(i) else {
            return zip_bytes.to_vec();
        };
        if entry.is_dir() {
            continue;
        }
        let name = normalize_entry_name(entry.name());
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

fn normalize_entry_name(name: &str) -> String {
    name.replace('\\', "/")
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

    #[test]
    fn parses_skill_from_zip() {
        let md = "---\nname: demo_skill\ndescription: A demo\n---\n# Body\nUse it.";
        let zip = make_zip(&[("demo_skill/SKILL.md", md), ("demo_skill/extra.txt", "hi")]);
        let skill = create_from_zip(&zip, "nacos@public", "demo_skill").unwrap();
        assert_eq!(skill.name(), "demo_skill");
        assert_eq!(skill.description(), "A demo");
        assert_eq!(skill.source(), "nacos@public");
        assert!(skill.resources().contains_key("demo_skill/extra.txt"));
    }

    #[test]
    fn missing_skill_md_errors() {
        let zip = make_zip(&[("demo/readme.txt", "x")]);
        assert!(create_from_zip(&zip, "s", "demo").is_err());
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
    fn parse_frontmatter_uses_fallback_name() {
        let md = "---\ndescription: only desc\n---\nbody";
        let zip = make_zip(&[("f/SKILL.md", md)]);
        let skill = create_from_zip(&zip, "nacos@public", "fallback").unwrap();
        assert_eq!(skill.name(), "fallback");
        assert_eq!(skill.description(), "only desc");
    }

    #[test]
    fn resource_paths_are_normalized_to_forward_slashes() {
        let md = "---\nname: r\n---\nbody";
        let zip = make_zip(&[("r/SKILL.md", md), ("r/scripts/run.sh", "echo hi")]);
        let skill = create_from_zip(&zip, "s", "r").unwrap();
        assert!(skill.resources().contains_key("r/scripts/run.sh"));
    }
}
