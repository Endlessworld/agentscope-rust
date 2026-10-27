//! JDBC-backed [`AgentSkillRepository`], mirroring Java
//! `JdbcAgentSkillRepository`.
//!
//! Database-agnostic: every statement comes from [`SkillDialect`] /
//! [`SkillResourcesDialect`] via [`BoundSql`]; this module contains no inline business
//! SQL. The repository is bound to one namespace at construction and every no-argument
//! method addresses exactly that scope. Namespace-aware variants ([`Self::get_skill_in`],
//! [`Self::save_in`], ...) let one instance address another namespace.
//!
//! The write switch is an [`AtomicBool`] so [`AgentSkillRepository::set_writeable`] works
//! through `&self`, matching the Java `volatile boolean writeable` field.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use agentscope_extensions_skills::{
    AgentSkillRepository, AgentSkillRepositoryInfo, SkillRepositoryError,
};
use agentscope_harness::agent::skill::model::{Metadata, SkillResources};
use agentscope_harness::agent::skill::AgentSkill;
use serde_json::Value;

use crate::backend::{ColumnValue, JdbcError, JdbcResult, JdbcSession, SharedExecutor};
use crate::dialect::{validate_table_schema, AbstractJdbcDialect, BoundSql};

/// Repository type reported by [`Self::get_repository_info`].
const REPOSITORY_TYPE: &str = "jdbc";

/// The namespace the namespace-less constructors bind; matches the DDL column default.
pub const DEFAULT_NAMESPACE: &str = "default";

/// Maximum length for a skill name, matching the legacy repositories.
pub const MAX_SKILL_NAME_LENGTH: usize = 255;
/// Maximum length for a resource path, matching the legacy repositories.
pub const MAX_RESOURCE_PATH_LENGTH: usize = 500;

/// JDBC-backed skill repository.
pub struct JdbcAgentSkillRepository {
    executor: SharedExecutor,
    dialect: Arc<dyn AbstractJdbcDialect>,
    namespace: String,
    writeable: AtomicBool,
}

impl std::fmt::Debug for JdbcAgentSkillRepository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JdbcAgentSkillRepository")
            .field("kind", &self.dialect.kind())
            .field("skill_table", &self.dialect.resolved_skill_table_name())
            .field("namespace", &self.namespace)
            .field("writeable", &self.writeable.load(Ordering::SeqCst))
            .finish()
    }
}

/// One loaded skills row, before resources are stitched on.
struct SkillRow {
    id: i64,
    name: String,
    description: String,
    /// The skill body loaded from the `SKILL.md` resource, kept for Java schema parity;
    /// the harness `AgentSkill` exposes it as a resource, not as a struct field.
    #[allow(dead_code)]
    skill_content: String,
    source: String,
    metadata_json: Option<String>,
}

impl SkillRow {
    /// Maps the shared projection `(id, name, description, skill_content, source,
    /// metadata_json)` onto a record.
    fn from_row(row: &[ColumnValue]) -> JdbcResult<Self> {
        Ok(Self {
            id: column_int(row, 0, "id")?,
            name: column_text(row, 1, "name")?.unwrap_or_default(),
            description: column_text(row, 2, "description")?.unwrap_or_default(),
            skill_content: column_text(row, 3, "skill_content")?.unwrap_or_default(),
            source: column_text(row, 4, "source")?.unwrap_or_default(),
            metadata_json: column_text(row, 5, "metadata_json")?,
        })
    }
}

fn column_text(row: &[ColumnValue], index: usize, name: &str) -> JdbcResult<Option<String>> {
    match row.get(index) {
        Some(ColumnValue::Text(text)) => Ok(Some(text.clone())),
        Some(ColumnValue::Null) | None => Ok(None),
        Some(other) => Err(JdbcError::Database(format!(
            "column '{name}' has unexpected non-text value: {other:?}"
        ))),
    }
}

fn column_int(row: &[ColumnValue], index: usize, name: &str) -> JdbcResult<i64> {
    match row.get(index) {
        Some(ColumnValue::Int(value)) => Ok(*value),
        Some(ColumnValue::Text(text)) => text.parse::<i64>().map_err(|e| {
            JdbcError::Database(format!("column '{name}' is not an integer: {text}: {e}"))
        }),
        Some(other) => Err(JdbcError::Database(format!(
            "column '{name}' has unexpected non-integer value: {other:?}"
        ))),
        None => Err(JdbcError::Database(format!("column '{name}' is missing"))),
    }
}

impl JdbcAgentSkillRepository {
    /// Creates a repository over tables that already exist, validating their schema.
    ///
    /// Java's constructor also never touches the schema (the builder does); this
    /// constructor mirrors that contract for `create_if_not_exist == false`.
    pub fn new(
        executor: SharedExecutor,
        dialect: Arc<dyn AbstractJdbcDialect>,
        namespace: &str,
        writeable: bool,
        create_if_not_exist: bool,
    ) -> Result<Self, SkillRepositoryError> {
        validate_namespace(namespace)?;
        let repository = Self {
            executor,
            dialect,
            namespace: namespace.to_string(),
            writeable: AtomicBool::new(writeable),
        };
        if create_if_not_exist {
            repository.create_schema()?;
        } else {
            repository.verify_schema()?;
        }
        Ok(repository)
    }

    /// Creates a repository in the [`DEFAULT_NAMESPACE`], writable, auto-creating the
    /// skill tables.
    pub fn with_schema(
        executor: SharedExecutor,
        dialect: Arc<dyn AbstractJdbcDialect>,
    ) -> Result<Self, SkillRepositoryError> {
        Self::new(executor, dialect, DEFAULT_NAMESPACE, true, true)
    }

    /// The bound namespace.
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Runs the dialect's idempotent skill DDL, then validates both tables.
    pub fn create_schema(&self) -> Result<(), SkillRepositoryError> {
        for ddl in self.dialect.skill_create_table_ddls() {
            self.executor
                .execute(&BoundSql::no_params(ddl))
                .map_err(db_error)?;
        }
        for ddl in self.dialect.skill_resources_create_table_ddls() {
            self.executor
                .execute(&BoundSql::no_params(ddl))
                .map_err(db_error)?;
        }
        self.verify_schema()
    }

    /// Validates the skill tables against the declared DDL (missing table or missing
    /// column — notably `metadata_json` — is reported with the reference DDL).
    pub fn verify_schema(&self) -> Result<(), SkillRepositoryError> {
        let skill_ddls = self.dialect.skill_create_table_ddls();
        validate_table_schema(
            self.executor.as_ref(),
            self.dialect.as_ref(),
            &self.dialect.resolved_skill_table_name(),
            &skill_ddls,
        )
        .map_err(db_error)?;
        let resource_ddls = self.dialect.skill_resources_create_table_ddls();
        validate_table_schema(
            self.executor.as_ref(),
            self.dialect.as_ref(),
            &self.dialect.resolved_skill_resources_table_name(),
            &resource_ddls,
        )
        .map_err(db_error)
    }

    // ------------------------------------------------------------------
    //  Namespace-bound read API
    // ------------------------------------------------------------------

    /// Loads one skill of the given namespace with its resources attached.
    pub fn get_skill_in(
        &self,
        namespace: &str,
        name: &str,
    ) -> Result<AgentSkill, SkillRepositoryError> {
        validate_namespace(namespace)?;
        validate_skill_name(name)?;
        let rows = self
            .executor
            .query(&self.dialect.skill_select_by_name(namespace, name))
            .map_err(db_error)?;
        let Some(row) = rows.first() else {
            return Err(SkillRepositoryError::NotFound(name.to_string()));
        };
        let record = SkillRow::from_row(row).map_err(db_error)?;
        let resources = self.load_resources_by_skill_id(record.id)?;
        build_skill(&record, resources)
    }

    /// Lists the stored names of one namespace, skipping names that could never be
    /// loaded or deleted (same read-side validation as loading a skill).
    pub fn get_all_skill_names_in(
        &self,
        namespace: &str,
    ) -> Result<Vec<String>, SkillRepositoryError> {
        validate_namespace(namespace)?;
        let rows = self
            .executor
            .query(&self.dialect.skill_select_all_names(namespace))
            .map_err(db_error)?;
        let mut names = Vec::with_capacity(rows.len());
        let mut omitted = 0usize;
        for row in &rows {
            let Some(name) = column_text(row, 0, "name").map_err(db_error)? else {
                continue;
            };
            if let Err(error) = validate_skill_name(&name) {
                tracing::warn!("[jdbc-skill] skipping unloadable skill name '{name}': {error}");
                omitted += 1;
                continue;
            }
            names.push(name);
        }
        if omitted > 0 {
            tracing::warn!("[jdbc-skill] skill name listing is partial: {omitted} name(s) omitted");
        }
        Ok(names)
    }

    /// Loads every skill of one namespace, ordered by name, resources stitched in.
    ///
    /// Rows the model rejects (invalid name or resource path) are skipped with a warning,
    /// matching Java's `buildAll`; orphaned resource rows are ignored.
    pub fn get_all_skills_in(
        &self,
        namespace: &str,
    ) -> Result<Vec<AgentSkill>, SkillRepositoryError> {
        validate_namespace(namespace)?;
        let rows = self
            .executor
            .query(&self.dialect.skill_select_all(namespace))
            .map_err(db_error)?;
        let mut records = Vec::with_capacity(rows.len());
        let mut index_by_id: HashMap<i64, usize> = HashMap::with_capacity(rows.len());
        for row in &rows {
            let record = SkillRow::from_row(row).map_err(db_error)?;
            index_by_id.insert(record.id, records.len());
            records.push((record, SkillResources::new()));
        }

        let resource_rows = self
            .executor
            .query(&self.dialect.skill_resources_select_all(namespace))
            .map_err(db_error)?;
        for row in &resource_rows {
            let skill_id = column_int(row, 0, "id").map_err(db_error)?;
            let path = column_text(row, 1, "resource_path")
                .map_err(db_error)?
                .unwrap_or_default();
            let content = column_text(row, 2, "resource_content")
                .map_err(db_error)?
                .unwrap_or_default();
            match index_by_id.get(&skill_id) {
                Some(&index) => {
                    records[index].1.insert(path, content);
                }
                None => {
                    tracing::warn!(
                        "[jdbc-skill] orphaned resource for non-existent skill id: {skill_id}"
                    );
                }
            }
        }

        let mut skills = Vec::with_capacity(records.len());
        let mut omitted = 0usize;
        for (record, resources) in records {
            match build_skill(&record, resources) {
                Ok(skill) => skills.push(skill),
                Err(error) => {
                    omitted += 1;
                    tracing::warn!(
                        "[jdbc-skill] failed to build skill '{}': {error}",
                        record.name
                    );
                }
            }
        }
        if omitted > 0 {
            tracing::warn!(
                "[jdbc-skill] skill catalog listing is partial: {omitted} of {} skill(s) omitted",
                skills.len() + omitted
            );
        }
        Ok(skills)
    }

    /// Whether one skill exists in the given namespace.
    ///
    /// Mirrors Java: only null/blank names are short-circuited; the name is otherwise not
    /// validated (an over-long name simply does not exist).
    pub fn skill_exists_in(
        &self,
        namespace: &str,
        skill_name: &str,
    ) -> Result<bool, SkillRepositoryError> {
        validate_namespace(namespace)?;
        if skill_name.is_empty() {
            return Ok(false);
        }
        let rows = self
            .executor
            .query(&self.dialect.skill_exists(namespace, skill_name))
            .map_err(db_error)?;
        Ok(!rows.is_empty())
    }

    // ------------------------------------------------------------------
    //  Namespace-bound write API
    // ------------------------------------------------------------------

    /// Saves skills into the given namespace.
    ///
    /// * empty input → `Ok(false)`;
    /// * read-only repository → `Ok(false)` (no error, matching Java);
    /// * `force == false` and a name exists → [`SkillRepositoryError::Conflict`];
    /// * `force == true` replaces the row and its resources within the namespace.
    pub fn save_in(
        &self,
        namespace: &str,
        skills: &[AgentSkill],
        force: bool,
    ) -> Result<bool, SkillRepositoryError> {
        validate_namespace(namespace)?;
        if skills.is_empty() {
            return Ok(false);
        }
        if !self.is_writeable_internal() {
            tracing::warn!("[jdbc-skill] cannot save skills: repository is read-only");
            return Ok(false);
        }
        validate_for_save(skills)?;

        // Advisory pre-check (Java `requireNoConflicts`): reports every conflict before
        // any write. The authoritative guarantee remains the UNIQUE(namespace, name)
        // constraint, which closes the race between this check and the insert.
        if !force {
            let mut existing = Vec::new();
            for skill in skills {
                if self.skill_exists_in(namespace, skill.name())? {
                    existing.push(skill.name().to_string());
                }
            }
            if !existing.is_empty() {
                return Err(SkillRepositoryError::Conflict(format!(
                    "the following skills already exist and force=false: {}",
                    existing.join(", ")
                )));
            }
        }

        let result = self.executor.in_transaction(&mut |session| {
            for skill in skills {
                if force {
                    if let Some(id) =
                        select_skill_id(session, &self.dialect, namespace, skill.name())?
                    {
                        session.execute(&self.dialect.skill_resources_delete_by_skill_id(id))?;
                        session
                            .execute(&self.dialect.skill_delete_by_name(namespace, skill.name()))?;
                    }
                }
                let metadata_json = serialize_metadata(skill.metadata());
                session.execute(&self.dialect.skill_insert(
                    namespace,
                    skill.name(),
                    skill.description(),
                    &skill_content(skill),
                    skill.source(),
                    &metadata_json,
                ))?;
                let Some(skill_id) =
                    select_skill_id(session, &self.dialect, namespace, skill.name())?
                else {
                    return Err(JdbcError::Database(format!(
                        "failed to read generated id for skill: {}",
                        skill.name()
                    )));
                };
                for (path, content) in skill.resources() {
                    session.execute(
                        &self
                            .dialect
                            .skill_resources_insert(namespace, skill_id, path, content),
                    )?;
                }
            }
            Ok(())
        });

        match result {
            Ok(()) => Ok(true),
            Err(error) if !force && is_skill_unique_violation(&error) => {
                Err(SkillRepositoryError::Conflict(format!(
                    "the following skills already exist and force=false: {}",
                    skills
                        .iter()
                        .map(|skill| skill.name())
                        .collect::<Vec<_>>()
                        .join(", ")
                )))
            }
            Err(error) => Err(db_error(error)),
        }
    }

    /// Deletes one skill and its resources within the given namespace.
    pub fn delete_in(
        &self,
        namespace: &str,
        skill_name: &str,
    ) -> Result<bool, SkillRepositoryError> {
        validate_namespace(namespace)?;
        if !self.is_writeable_internal() {
            tracing::warn!("[jdbc-skill] cannot delete skill: repository is read-only");
            return Ok(false);
        }
        validate_skill_name(skill_name)?;
        if !self.skill_exists_in(namespace, skill_name)? {
            tracing::warn!("[jdbc-skill] skill does not exist: {skill_name}");
            return Ok(false);
        }
        self.executor
            .in_transaction(&mut |session| {
                if let Some(id) = select_skill_id(session, &self.dialect, namespace, skill_name)? {
                    session.execute(&self.dialect.skill_resources_delete_by_skill_id(id))?;
                }
                session.execute(&self.dialect.skill_delete_by_name(namespace, skill_name))?;
                Ok(())
            })
            .map_err(db_error)?;
        Ok(true)
    }

    /// Deletes every skill and resource row of the given namespace, returning the number
    /// of deleted skill rows.
    pub fn clear_all_skills_in(&self, namespace: &str) -> Result<usize, SkillRepositoryError> {
        validate_namespace(namespace)?;
        if !self.is_writeable_internal() {
            tracing::warn!("[jdbc-skill] cannot clear skills: repository is read-only");
            return Ok(0);
        }
        let mut deleted = 0usize;
        self.executor
            .in_transaction(&mut |session| {
                session.execute(&self.dialect.skill_resources_delete_all(namespace))?;
                deleted = session.execute(&self.dialect.skill_delete_all(namespace))?;
                Ok(())
            })
            .map_err(db_error)?;
        Ok(deleted)
    }

    /// Clears the namespace bound at construction.
    pub fn clear_all_skills(&self) -> Result<usize, SkillRepositoryError> {
        self.clear_all_skills_in(&self.namespace)
    }

    fn is_writeable_internal(&self) -> bool {
        self.writeable.load(Ordering::SeqCst)
    }

    fn load_resources_by_skill_id(
        &self,
        skill_id: i64,
    ) -> Result<SkillResources, SkillRepositoryError> {
        let rows = self
            .executor
            .query(&self.dialect.skill_resources_select_by_skill_id(skill_id))
            .map_err(db_error)?;
        let mut resources = SkillResources::new();
        for row in &rows {
            let path = column_text(row, 0, "resource_path")
                .map_err(db_error)?
                .unwrap_or_default();
            let content = column_text(row, 1, "resource_content")
                .map_err(db_error)?
                .unwrap_or_default();
            resources.insert(path, content);
        }
        Ok(resources)
    }
}

impl AgentSkillRepository for JdbcAgentSkillRepository {
    fn get_skill(&self, name: &str) -> Result<AgentSkill, SkillRepositoryError> {
        self.get_skill_in(&self.namespace, name)
    }

    fn get_all_skill_names(&self) -> Result<Vec<String>, SkillRepositoryError> {
        self.get_all_skill_names_in(&self.namespace)
    }

    fn get_all_skills(&self) -> Result<Vec<AgentSkill>, SkillRepositoryError> {
        self.get_all_skills_in(&self.namespace)
    }

    fn save(&self, skills: &[AgentSkill], force: bool) -> Result<bool, SkillRepositoryError> {
        self.save_in(&self.namespace, skills, force)
    }

    fn delete(&self, skill_name: &str) -> Result<bool, SkillRepositoryError> {
        self.delete_in(&self.namespace, skill_name)
    }

    fn skill_exists(&self, skill_name: &str) -> Result<bool, SkillRepositoryError> {
        self.skill_exists_in(&self.namespace, skill_name)
    }

    fn get_repository_info(&self) -> AgentSkillRepositoryInfo {
        AgentSkillRepositoryInfo::new(
            REPOSITORY_TYPE,
            format!(
                "{}@{}",
                self.dialect.resolved_skill_table_name(),
                self.namespace
            ),
            self.is_writeable_internal(),
        )
    }

    fn get_source(&self) -> String {
        format!(
            "{}_{}@{}",
            REPOSITORY_TYPE,
            self.dialect.resolved_skill_table_name(),
            self.namespace
        )
    }

    fn is_writeable(&self) -> bool {
        self.is_writeable_internal()
    }

    fn set_writeable(&self, writeable: bool) {
        self.writeable.store(writeable, Ordering::SeqCst);
    }

    fn close(&self) -> Result<(), SkillRepositoryError> {
        // The executor is managed externally; nothing to release.
        Ok(())
    }
}

// ----------------------------------------------------------------------
//  Save-path helpers
// ----------------------------------------------------------------------

/// The skill body persisted in the `skill_content` column.
///
/// The harness `AgentSkill` folds the body into the `SKILL.md` resource; the column keeps
/// that body for Java schema interoperability. A skill without a `SKILL.md` resource
/// stores an empty string, matching `agentscope-extensions-skills`.
fn skill_content(skill: &AgentSkill) -> String {
    skill
        .resources()
        .iter()
        .find(|(path, _)| path.ends_with("SKILL.md"))
        .map(|(_, content)| content.clone())
        .unwrap_or_default()
}

fn serialize_metadata(metadata: &Metadata) -> String {
    serde_json::to_string(metadata).unwrap_or_else(|_| "{}".to_string())
}

fn select_skill_id(
    session: &mut dyn JdbcSession,
    dialect: &Arc<dyn AbstractJdbcDialect>,
    namespace: &str,
    name: &str,
) -> JdbcResult<Option<i64>> {
    let rows = session.query(&dialect.skill_select_id_by_name(namespace, name))?;
    match rows.first() {
        Some(row) => Ok(Some(column_int(row, 0, "id")?)),
        None => Ok(None),
    }
}

fn validate_for_save(skills: &[AgentSkill]) -> Result<(), SkillRepositoryError> {
    for skill in skills {
        validate_skill_name(skill.name())?;
        for (path, _content) in skill.resources() {
            validate_resource_path(path)?;
        }
    }
    Ok(())
}

/// Builds an [`AgentSkill`], re-validating name and resource paths on read. `metadata_json`
/// is restored when present (malformed JSON degrades to the core columns with a warning);
/// the `name` / `description` SQL columns are authoritative and overwrite any JSON values.
fn build_skill(
    record: &SkillRow,
    resources: SkillResources,
) -> Result<AgentSkill, SkillRepositoryError> {
    validate_skill_name(&record.name)?;
    for path in resources.keys() {
        validate_resource_path(path)?;
    }
    let mut metadata = deserialize_metadata(record.metadata_json.as_deref(), &record.name);
    metadata.insert("name".to_string(), Value::String(record.name.clone()));
    metadata.insert(
        "description".to_string(),
        Value::String(record.description.clone()),
    );
    Ok(AgentSkill::builder()
        .name(record.name.clone())
        .description(record.description.clone())
        .source(record.source.clone())
        .metadata(metadata)
        .resources(resources)
        .build())
}

fn deserialize_metadata(metadata_json: Option<&str>, name: &str) -> Metadata {
    let Some(json) = metadata_json.filter(|json| !json.trim().is_empty()) else {
        return Metadata::new();
    };
    match serde_json::from_str::<Metadata>(json) {
        Ok(parsed) => parsed,
        Err(error) => {
            tracing::warn!(
                "[jdbc-skill] failed to deserialize metadata_json for skill '{name}', \
                 falling back to core metadata: {error}"
            );
            Metadata::new()
        }
    }
}

/// Classifies duplicate-key failures narrowly: only positive duplicate signals match, so
/// a NOT NULL / CHECK / FK failure surfaces as a database error instead of the misleading
/// "use force=true" conflict (mirrors Java `isUniqueViolation`).
fn is_skill_unique_violation(error: &JdbcError) -> bool {
    let JdbcError::Database(message) = error else {
        return false;
    };
    let lower = message.to_ascii_lowercase();
    lower.contains("unique") || lower.contains("duplicate") || lower.contains("23505")
}

fn db_error(error: JdbcError) -> SkillRepositoryError {
    SkillRepositoryError::Database(error.to_string())
}

// ----------------------------------------------------------------------
//  Validation
// ----------------------------------------------------------------------

/// Validates a namespace: 1–64 characters from `[A-Za-z0-9._-]`, excluding `.` and `..`.
pub fn validate_namespace(namespace: &str) -> Result<(), SkillRepositoryError> {
    if namespace.is_empty() {
        return Err(SkillRepositoryError::invalid(
            "namespace",
            "namespace cannot be null or empty",
        ));
    }
    if namespace == "." || namespace == ".." {
        return Err(SkillRepositoryError::invalid(
            "namespace",
            format!("namespace must not be '.' or '..': {namespace}"),
        ));
    }
    if namespace.chars().count() > 64 {
        return Err(SkillRepositoryError::invalid(
            "namespace",
            "namespace cannot exceed 64 characters",
        ));
    }
    if let Some(ch) = namespace
        .chars()
        .find(|ch| !(ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-')))
    {
        return Err(SkillRepositoryError::invalid(
            "namespace",
            format!(
                "namespace contains illegal character '{ch}'; allowed charset is [A-Za-z0-9._-]: \
                 {namespace}"
            ),
        ));
    }
    Ok(())
}

/// Validates a skill name: non-blank, at most 255 characters, no path separators or `..`.
pub fn validate_skill_name(skill_name: &str) -> Result<(), SkillRepositoryError> {
    if skill_name.trim().is_empty() {
        return Err(SkillRepositoryError::invalid(
            "skill name",
            "skill name cannot be null or empty",
        ));
    }
    if skill_name.chars().count() > MAX_SKILL_NAME_LENGTH {
        return Err(SkillRepositoryError::invalid(
            "skill name",
            format!("skill name cannot exceed {MAX_SKILL_NAME_LENGTH} characters"),
        ));
    }
    if skill_name.contains("..") || skill_name.contains('/') || skill_name.contains('\\') {
        return Err(SkillRepositoryError::invalid(
            "skill name",
            "skill name cannot contain path separators or '..'",
        ));
    }
    Ok(())
}

/// Validates a resource path: non-blank, at most 500 characters, relative, no `..`
/// segment. Ordinary relative sub-paths such as `docs/readme.md` stay valid.
pub fn validate_resource_path(path: &str) -> Result<(), SkillRepositoryError> {
    if path.trim().is_empty() {
        return Err(SkillRepositoryError::invalid(
            "resource path",
            "resource path cannot be null or empty",
        ));
    }
    if path.chars().count() > MAX_RESOURCE_PATH_LENGTH {
        return Err(SkillRepositoryError::invalid(
            "resource path",
            format!("resource path cannot exceed {MAX_RESOURCE_PATH_LENGTH} characters"),
        ));
    }
    if is_absolute_or_traversing(path) {
        return Err(SkillRepositoryError::invalid(
            "resource path",
            format!("resource path must be relative and must not contain '..': {path}"),
        ));
    }
    Ok(())
}

/// Whether the path is absolute (POSIX, Windows drive, or UNC) or contains a `..`
/// segment.
fn is_absolute_or_traversing(path: &str) -> bool {
    if path.starts_with('/') || path.starts_with('\\') {
        return true;
    }
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        // Deliberately also matches drive-relative forms like `C:foo`, which resolve
        // outside the skill directory on Windows.
        return true;
    }
    path.split(['/', '\\']).any(|segment| segment == "..")
}
