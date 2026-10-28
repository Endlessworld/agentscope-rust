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
}
