//! Shared validation and metadata helpers used by the SQL-backed repositories.
//!
//! Ports the private helpers of `MysqlSkillRepository` / `PostgresSkillRepository` so both
//! backends share one implementation instead of duplicating them.

use agentscope_harness::agent::skill::model::{
    AgentSkill, AgentSkillBuilder, Metadata, SkillResources,
};
use serde_json::Value;

use crate::error::SkillRepositoryError;

/// Maximum skill name length (`MAX_SKILL_NAME_LENGTH` in Java).
pub const MAX_SKILL_NAME_LENGTH: usize = 255;
/// Maximum resource path length (`MAX_RESOURCE_PATH_LENGTH` in Java).
pub const MAX_RESOURCE_PATH_LENGTH: usize = 500;

/// Validates a skill name, mirroring `validateSkillName`.
///
/// Rejects empty names, over-long names and path-traversal characters (`..`, `/`, `\`).
pub fn validate_skill_name(name: &str) -> Result<(), SkillRepositoryError> {
    if name.trim().is_empty() {
        return Err(SkillRepositoryError::invalid(
            "skill name",
            "cannot be null or empty",
        ));
    }
    if name.chars().count() > MAX_SKILL_NAME_LENGTH {
        return Err(SkillRepositoryError::invalid(
            "skill name",
            format!("cannot exceed {MAX_SKILL_NAME_LENGTH} characters"),
        ));
    }
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return Err(SkillRepositoryError::invalid(
            "skill name",
            "cannot contain path separators or '..'",
        ));
    }
    Ok(())
}

/// Validates a resource path, mirroring `validateResourcePath`.
pub fn validate_resource_path(path: &str) -> Result<(), SkillRepositoryError> {
    if path.trim().is_empty() {
        return Err(SkillRepositoryError::invalid(
            "resource path",
            "cannot be null or empty",
        ));
    }
    if path.chars().count() > MAX_RESOURCE_PATH_LENGTH {
        return Err(SkillRepositoryError::invalid(
            "resource path",
            format!("cannot exceed {MAX_RESOURCE_PATH_LENGTH} characters"),
        ));
    }
    Ok(())
}

/// Resolves a user-supplied database / schema / table name exactly like the Java constructor:
///
/// ```java
/// this.databaseName = (databaseName == null || databaseName.trim().isEmpty())
///         ? DEFAULT_DATABASE_NAME
///         : databaseName.trim();
/// ```
///
/// A blank (or all-whitespace) name falls back to `default`; otherwise the **trimmed** value is
/// returned. Callers still run [`validate_identifier`] on the result, so `"  skills "` becomes
/// `skills` while `"bad-name"` keeps failing (Java does the same two-step).
pub fn resolve_identifier(raw: &str, default: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        default.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Validates a database/table/schema identifier, rejecting SQL-injection vectors.
///
/// Identifiers cannot be parameterized in prepared statements, so they must match
/// `^[A-Za-z_][A-Za-z0-9_]*$` and respect the backend length limit (64 for MySQL, 63 for
/// PostgreSQL — the caller passes `max_len`).
pub fn validate_identifier(
    identifier: &str,
    kind: &str,
    max_len: usize,
) -> Result<(), SkillRepositoryError> {
    if identifier.is_empty() {
        return Err(SkillRepositoryError::invalid(
            kind,
            "cannot be null or empty",
        ));
    }
    if identifier.chars().count() > max_len {
        return Err(SkillRepositoryError::invalid(
            kind,
            format!("cannot exceed {max_len} characters"),
        ));
    }
    let mut chars = identifier.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_alphabetic() || first == '_')
        || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(SkillRepositoryError::invalid(
            kind,
            format!(
                "contains invalid characters (only alphanumeric and underscore, must start \
                 with a letter or underscore): {identifier}"
            ),
        ));
    }
    Ok(())
}

/// Splits `---`-fenced YAML frontmatter into `(frontmatter, body)`.
///
/// Lives here rather than in [`crate::git`] because the SQL backends need the same parse to
/// persist the Java-shaped `skill_content` column (body only; SKILLS-08), and a second copy of
/// the fence rules would drift from the git loader.
pub fn split_frontmatter(markdown: &str) -> (Option<&str>, &str) {
    let text = markdown.strip_prefix('\u{feff}').unwrap_or(markdown);
    let rest = match text.strip_prefix("---") {
        // Java `MarkdownSkillParser.FRONTMATTER_PATTERN` is `^\uFEFF?---\s*[\r\n]+...`, so blank
        // space between the opening fence and the line break is allowed (SKILLS-05); a suffix
        // (`---x`) is not a fence.
        Some(rest) => {
            let after_blanks = rest.trim_start_matches([' ', '\t']);
            match after_blanks
                .strip_prefix("\r\n")
                .or_else(|| after_blanks.strip_prefix('\n'))
            {
                None => return (None, text),
                Some(after) => after.trim_start_matches(['\r', '\n']),
            }
        }
        None => return (None, text),
    };
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        // Java `MarkdownSkillParser.FRONTMATTER_PATTERN` requires the closing `---` / `...` to
        // occupy the whole line on its own: it must directly follow a `\r` / `\n` (lookbehind),
        // and only blank space may follow it before the next line break. So no leading
        // indentation is allowed (an indented `  ---` is `|` / `>` block content), while
        // trailing spaces / tabs stay permitted.
        let raw = line.trim_end_matches(['\r', '\n']);
        let candidate = raw.trim_end_matches([' ', '\t']);
        if candidate == "---" || candidate == "..." {
            return (Some(&rest[..offset]), &rest[offset + line.len()..]);
        }
        offset += line.len();
    }
    (None, text)
}

/// The skill body as Java persists it in `skill_content`: the `SKILL.md` markdown **without** its
/// YAML frontmatter (`SkillUtil.createFrom` → `parsed.getContent()`).
pub fn skill_body(markdown: &str) -> &str {
    split_frontmatter(markdown).1
}

/// Whether a resource path is the skill document itself.
///
/// Java `SkillFileSystemHelper.isValidResource` keeps `SKILL.md` out of the resources map: the
/// document is the skill *body* (`skill_content` column), everything else is a resource row.
pub fn is_skill_document(path: &str) -> bool {
    path.ends_with("SKILL.md")
}

/// Serializes a skill's metadata tree for the `metadata_json` column.
pub fn serialize_metadata(metadata: &Metadata) -> String {
    serde_json::to_string(metadata).unwrap_or_else(|_| "{}".to_string())
}

/// Deserializes `metadata_json`, then overlays the authoritative `name` / `description`
/// columns, mirroring `deserializeMetadata`.
pub fn deserialize_metadata(
    metadata_json: Option<&str>,
    name: &str,
    description: &str,
) -> Metadata {
    let mut metadata = Metadata::new();
    if let Some(json) = metadata_json.filter(|j| !j.trim().is_empty()) {
        if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(json) {
            metadata = map;
        } else {
            tracing::warn!(
                skill = name,
                "failed to deserialize metadata_json, falling back to core metadata"
            );
        }
    }
    metadata.insert("name".to_string(), Value::String(name.to_string()));
    metadata.insert(
        "description".to_string(),
        Value::String(description.to_string()),
    );
    metadata
}

/// Whether metadata carries fields beyond the legacy core columns (`hasExtendedMetadata`).
/// Used only to warn when writing to a schema without `metadata_json`.
pub fn has_extended_metadata(metadata: &Metadata) -> bool {
    metadata.len() > 2 || metadata.keys().any(|k| k != "name" && k != "description")
}

/// Rebuilds an [`AgentSkill`] from the persisted columns, mirroring `buildSkill`.
///
/// The skill body lives under the `SKILL.md` resource key so the harness loader / prompt
/// provider can read it back; `name`/`description` are authoritative from the columns.
pub fn build_skill(
    name: &str,
    description: &str,
    resources: SkillResources,
    source: &str,
    metadata_json: Option<&str>,
) -> AgentSkill {
    let metadata = deserialize_metadata(metadata_json, name, description);
    let mut builder: AgentSkillBuilder = AgentSkill::builder()
        .name(name)
        .source(source)
        .description(description)
        .metadata(metadata);
    for (path, content) in resources {
        builder = builder.add_resource(path, content);
    }
    builder.build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_skill_names() {
        assert!(validate_skill_name("ok").is_ok());
        assert!(validate_skill_name("").is_err());
        assert!(validate_skill_name("a/b").is_err());
        assert!(validate_skill_name("..").is_err());
    }

    // Java parity (P2-02): null / blank falls back to the default, everything else is trimmed.
    #[test]
    fn resolves_blank_and_padded_identifiers() {
        assert_eq!(resolve_identifier("", "fallback"), "fallback");
        assert_eq!(resolve_identifier("   \t\n", "fallback"), "fallback");
        assert_eq!(
            resolve_identifier("  custom_skills  ", "fallback"),
            "custom_skills"
        );
        // The trimmed value is what gets validated, so padding no longer trips the pattern.
        assert!(validate_identifier(&resolve_identifier(" skills ", "d"), "table", 64).is_ok());
        assert!(validate_identifier(&resolve_identifier(" 9bad ", "d"), "table", 64).is_err());
    }

    #[test]
    fn validates_identifiers() {
        assert!(validate_identifier("agentscope_skills", "table", 64).is_ok());
        assert!(validate_identifier("_x9", "table", 64).is_ok());
        assert!(validate_identifier("9bad", "table", 64).is_err());
        assert!(validate_identifier("bad-name", "table", 64).is_err());
        assert!(validate_identifier("drop;table", "table", 64).is_err());
    }

    // SKILLS-08: the SQL backends persist this body in `skill_content`.
    #[test]
    fn skill_body_drops_only_the_frontmatter() {
        assert_eq!(
            skill_body("---\nname: x\ndescription: d\n---\n# Body\ntext\n"),
            "# Body\ntext\n"
        );
        // No frontmatter: the whole document is the body.
        assert_eq!(skill_body("# Body\ntext\n"), "# Body\ntext\n");
        assert!(is_skill_document("SKILL.md"));
        assert!(is_skill_document("sub/SKILL.md"));
        assert!(!is_skill_document("sub/a.md"));
    }

    #[test]
    fn metadata_overlays_name_and_description() {
        let m = deserialize_metadata(Some(r#"{"name":"old","extra":1}"#), "new", "desc");
        assert_eq!(m.get("name"), Some(&Value::String("new".into())));
        assert_eq!(m.get("description"), Some(&Value::String("desc".into())));
        assert_eq!(m.get("extra"), Some(&Value::from(1)));
    }

    #[test]
    fn detects_extended_metadata() {
        let mut m = Metadata::new();
        m.insert("name".into(), Value::String("n".into()));
        m.insert("description".into(), Value::String("d".into()));
        assert!(!has_extended_metadata(&m));
        m.insert("extra".into(), Value::Bool(true));
        assert!(has_extended_metadata(&m));
    }

    /// Java parity: `MAX_SKILL_NAME_LENGTH = 255`, counted in characters (not bytes), so a
    /// 255-character name passes and 256 fails - regardless of multi-byte content.
    #[test]
    fn skill_name_length_limit_is_counted_in_characters() {
        assert!(validate_skill_name(&"a".repeat(MAX_SKILL_NAME_LENGTH)).is_ok());
        assert!(validate_skill_name(&"a".repeat(MAX_SKILL_NAME_LENGTH + 1)).is_err());
        // 255 CJK characters are 765 bytes: a byte-based limit would reject them.
        assert!(validate_skill_name(&"\u{6280}".repeat(MAX_SKILL_NAME_LENGTH)).is_ok());
    }

    /// Java parity: `validateResourcePath` bounds the path to 500 characters and only rejects
    /// blanks - sub-directories and `.` segments are allowed (only `..` is blocked elsewhere).
    #[test]
    fn resource_path_validation_bounds() {
        assert!(validate_resource_path("refs/table.csv").is_ok());
        assert!(validate_resource_path(&"r".repeat(MAX_RESOURCE_PATH_LENGTH)).is_ok());
        assert!(validate_resource_path(&"r".repeat(MAX_RESOURCE_PATH_LENGTH + 1)).is_err());
        assert!(validate_resource_path("   ").is_err());
    }

    /// P2-02: the identifier limit is backend-specific (MySQL 64 / PostgreSQL 63); the same
    /// 64-character name is legal for MySQL and illegal for PostgreSQL.
    #[test]
    fn identifier_length_limits_are_backend_specific() {
        let name = "a".repeat(64);
        assert!(validate_identifier(&name, "table", 64).is_ok());
        assert!(validate_identifier(&name, "table", 63).is_err());
        assert!(validate_identifier("", "table", 64).is_err());
    }

    /// `deserialize_metadata` falls back to the legacy columns when `metadata_json` is absent,
    /// blank or malformed (Java `JsonUtils` swallow-and-warn).
    #[test]
    fn malformed_metadata_json_falls_back_to_columns() {
        for json in [None, Some("   "), Some("{not json")] {
            let m = deserialize_metadata(json, "n", "d");
            assert_eq!(m.get("name"), Some(&Value::String("n".into())));
            assert_eq!(m.get("description"), Some(&Value::String("d".into())));
            assert_eq!(m.len(), 2, "only the authoritative columns remain");
        }
    }
}
