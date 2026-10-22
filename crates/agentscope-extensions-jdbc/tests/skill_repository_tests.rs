//! Behavior tests for [`JdbcAgentSkillRepository`], ported from the Java
//! `JdbcAgentSkillRepositoryH2Test` / `JdbcAgentSkillRepositorySqliteTest` suites.
//!
//! The in-memory SQLite backend ([`RusqliteExecutor`]) plays the role of the Java H2
//! backend: one fresh database per test, real SQL, no mocks.

use std::sync::Arc;

use agentscope_extensions_jdbc::backend::in_memory_executor;
use agentscope_extensions_jdbc::dialect::{
    parse_create_table_columns, AbstractJdbcDialect, AnsiDialect, BoundSql, JdbcDialectBuilder,
    MysqlDialect, PostgresDialect, SqlParam, SqliteDialect,
};
use agentscope_extensions_jdbc::skill::{
    JdbcAgentSkillRepository, SkillRepositoryError,
};
use agentscope_extensions_jdbc::{JdbcExecutor, SharedExecutor};
use agentscope_extensions_skills::AgentSkillRepository;
use agentscope_harness::agent::skill::AgentSkill;
use serde_json::json;

type Repo = JdbcAgentSkillRepository;

fn setup() -> (SharedExecutor, Arc<dyn AbstractJdbcDialect>, Repo) {
    let executor: SharedExecutor = in_memory_executor().expect("in-memory executor");
    let dialect: Arc<dyn AbstractJdbcDialect> = Arc::new(SqliteDialect::new());
    let repo = Repo::with_schema(executor.clone(), dialect.clone()).expect("schema");
    (executor, dialect, repo)
}

/// A skill with extended metadata and two resources, mirroring the Java `sampleSkill`.
fn sample_skill(name: &str) -> AgentSkill {
    AgentSkill::builder()
        .name(name)
        .description(format!("Description of {name}"))
        .source("test")
        .add_metadata("homepage", json!(format!("https://example.com/{name}")))
        .add_resource("docs/readme.md", "hello")
        .add_resource("docs/guide.md", "world")
        .build()
}

fn code_of(repo: &Repo) -> Result<(), SkillRepositoryError> {
    repo.get_skill("__probe__").map(|_| ())
}

fn count_rows(executor: &SharedExecutor, table: &str) -> i64 {
    let rows = executor
        .query(&BoundSql::no_params(format!(
            "SELECT COUNT(*) FROM {table}"
        )))
        .expect("count");
    rows[0][0].as_int().expect("int")
}

fn exists_by_name(executor: &SharedExecutor, table: &str, name: &str) -> bool {
    let rows = executor
        .query(&BoundSql::new(
            format!("SELECT 1 FROM {table} WHERE name = ?"),
            vec![SqlParam::Text(name.to_string())],
        ))
        .expect("probe");
    !rows.is_empty()
}

fn is_conflict(err: &SkillRepositoryError) -> bool {
    matches!(err, SkillRepositoryError::Conflict(_))
}

fn is_not_found(err: &SkillRepositoryError) -> bool {
    matches!(err, SkillRepositoryError::NotFound(_))
}

// ---------------------------------------------------------------------------
//  CRUD and listing — Java: JdbcAgentSkillRepositoryH2Test.CrudTests
// ---------------------------------------------------------------------------

#[test]
fn save_then_get_round_trips_metadata_and_resources() {
    // Java: saveAndGetRoundTrip
    let (_exec, _dialect, repo) = setup();
    assert!(repo.save(&[sample_skill("demo")], false).unwrap());

    let loaded = repo.get_skill("demo").unwrap();
    assert_eq!("demo", loaded.name());
    assert_eq!("Description of demo", loaded.description());
    assert_eq!("test", loaded.source());
    assert_eq!(
        Some(&json!("https://example.com/demo")),
        loaded.metadata().get("homepage")
    );
    assert_eq!(Some("hello"), loaded.resources().get("docs/readme.md").map(String::as_str));
    assert_eq!(Some("world"), loaded.resources().get("docs/guide.md").map(String::as_str));
}

#[test]
fn list_operations_are_ordered_and_empty_when_no_rows() {
    // Java: listOperations
    let (_exec, _dialect, repo) = setup();
    assert!(repo.get_all_skill_names().unwrap().is_empty());
    assert!(repo.get_all_skills().unwrap().is_empty());

    repo.save(&[sample_skill("beta"), sample_skill("alpha")], false)
        .unwrap();

    assert_eq!(vec!["alpha".to_string(), "beta".to_string()], repo.get_all_skill_names().unwrap());
    let skills = repo.get_all_skills().unwrap();
    assert_eq!(2, skills.len());
    assert_eq!("alpha", skills[0].name());
    assert_eq!(
        Some("hello"),
        skills[1].resources().get("docs/readme.md").map(String::as_str)
    );
}

#[test]
fn unknown_skill_is_not_found() {
    // Java: unknownSkillThrows (IllegalArgumentException -> NotFound)
    let (_exec, _dialect, repo) = setup();
    assert!(is_not_found(&repo.get_skill("missing").unwrap_err()));
}

#[test]
fn force_false_conflicts_and_force_true_replaces_row_and_resources() {
    // Java: forceSemantics
    let (exec, _dialect, repo) = setup();
    repo.save(&[sample_skill("conflict")], false).unwrap();

    let err = repo.save(&[sample_skill("conflict")], false).unwrap_err();
    assert!(is_conflict(&err), "{err}");
    assert!(err.to_string().contains("force=false"), "{err}");

    let replacement = AgentSkill::builder()
        .name("conflict")
        .description("replaced")
        .source("test")
        .add_resource("docs/new.md", "new")
        .build();
    assert!(repo.save(&[replacement], true).unwrap());

    let loaded = repo.get_skill("conflict").unwrap();
    assert_eq!("replaced", loaded.description());
    assert_eq!(Some("new"), loaded.resources().get("docs/new.md").map(String::as_str));
    assert!(
        loaded.resources().get("docs/readme.md").is_none(),
        "old resources must be replaced"
    );
    assert_eq!(1, count_rows(&exec, "agentscope_skill_resources"));
}

#[test]
fn delete_removes_resources_and_unknown_delete_returns_false() {
    // Java: deleteSemantics
    let (exec, _dialect, repo) = setup();
    assert!(!repo.delete("missing").unwrap());

    repo.save(&[sample_skill("doomed")], false).unwrap();
    assert!(repo.delete("doomed").unwrap());
    assert!(!repo.skill_exists("doomed").unwrap());
    assert!(is_not_found(&repo.get_skill("doomed").unwrap_err()));
    assert_eq!(0, count_rows(&exec, "agentscope_skill_resources"));
}

#[test]
fn skill_exists_covers_present_absent_and_empty_names() {
    // Java: skillExistsCases
    let (_exec, _dialect, repo) = setup();
    repo.save(&[sample_skill("present")], false).unwrap();

    assert!(repo.skill_exists("present").unwrap());
    assert!(!repo.skill_exists("absent").unwrap());
    assert!(!repo.skill_exists("").unwrap());
    // `code_of` exercises the namespace-less read entry point as well.
    assert!(code_of(&repo).is_err());
}

#[test]
fn duplicate_inside_one_save_is_a_conflict_and_rolls_back() {
    // Java: duplicateWithinOneSaveFailsOnUniqueConstraint
    let (_exec, _dialect, repo) = setup();
    let err = repo
        .save(&[sample_skill("dup"), sample_skill("dup")], false)
        .unwrap_err();
    assert!(is_conflict(&err), "{err}");
    assert!(!repo.skill_exists("dup").unwrap(), "the first row must be rolled back");
}

#[test]
fn clear_all_skills_deletes_skills_and_resources() {
    // Java: clearAllSkills
    let (exec, _dialect, repo) = setup();
    repo.save(&[sample_skill("a"), sample_skill("b")], false)
        .unwrap();

    assert_eq!(2, repo.clear_all_skills().unwrap());
    assert!(repo.get_all_skill_names().unwrap().is_empty());
    assert_eq!(0, count_rows(&exec, "agentscope_skill_resources"));
}

#[test]
fn skill_without_resources_round_trips_empty() {
    // Java: skillWithoutResources
    let (_exec, _dialect, repo) = setup();
    let bare = AgentSkill::builder()
        .name("bare")
        .description("d")
        .source("test")
        .build();
    assert!(repo.save(&[bare], false).unwrap());
    assert!(repo.get_skill("bare").unwrap().resources().is_empty());
    assert!(repo.get_all_skills().unwrap()[0].resources().is_empty());
}

// ---------------------------------------------------------------------------
//  Namespace isolation — Java: JdbcAgentSkillRepositoryH2Test.NamespaceTests
// ---------------------------------------------------------------------------

#[test]
fn same_name_coexists_across_namespaces() {
    // Java: sameNameCoexistsAcrossNamespaces
    let (_exec, _dialect, repo) = setup();
    let team_a = sample_skill("code-review");
    let team_b = AgentSkill::builder()
        .name("code-review")
        .description("team-b flavor")
        .source("test")
        .add_resource("docs/team-b.md", "b")
        .build();

    assert!(repo.save_in("team-a", &[team_a], false).unwrap());
    assert!(repo.save_in("team-b", &[team_b], false).unwrap());

    assert_eq!(vec!["code-review".to_string()], repo.get_all_skill_names_in("team-a").unwrap());
    assert_eq!(vec!["code-review".to_string()], repo.get_all_skill_names_in("team-b").unwrap());
    assert_eq!(
        "Description of code-review",
        repo.get_skill_in("team-a", "code-review").unwrap().description()
    );
    assert_eq!(
        "team-b flavor",
        repo.get_skill_in("team-b", "code-review").unwrap().description()
    );
    assert_eq!(
        Some("b"),
        repo.get_skill_in("team-b", "code-review")
            .unwrap()
            .resources()
            .get("docs/team-b.md")
            .map(String::as_str),
        "resources must be stitched to the owning namespace's row"
    );

    assert!(repo.skill_exists_in("team-a", "code-review").unwrap());
    assert!(!repo.skill_exists_in("team-a", "never-saved").unwrap());
    assert!(repo.get_all_skill_names().unwrap().is_empty(), "default namespace stays empty");
}

#[test]
fn get_all_skills_is_namespaced_and_resources_are_stitched() {
    // Java: getAllSkillsIsNamespaced
    let (_exec, _dialect, repo) = setup();
    repo.save_in("team-a", &[sample_skill("alpha")], false)
        .unwrap();
    repo.save_in(
        "team-b",
        &[AgentSkill::builder()
            .name("beta")
            .description("team-b skill")
            .source("test")
            .add_resource("docs/team-b.md", "b")
            .build()],
        false,
    )
    .unwrap();

    let team_a = repo.get_all_skills_in("team-a").unwrap();
    assert_eq!(vec!["alpha".to_string()], team_a.iter().map(|s| s.name().to_string()).collect::<Vec<_>>());
    assert_eq!(Some("hello"), team_a[0].resources().get("docs/readme.md").map(String::as_str));
    assert!(repo.get_all_skills_in("ghost").unwrap().is_empty());
}

#[test]
fn force_is_namespaced() {
    // Java: forceIsNamespaced
    let (_exec, _dialect, repo) = setup();
    repo.save_in("team-a", &[sample_skill("shared")], false)
        .unwrap();
    repo.save_in("team-b", &[sample_skill("shared")], false)
        .unwrap();

    let replacement = AgentSkill::builder()
        .name("shared")
        .description("replaced")
        .source("test")
        .add_resource("docs/new.md", "new")
        .build();
    assert!(repo.save_in("team-a", &[replacement], true).unwrap());

    let team_a = repo.get_skill_in("team-a", "shared").unwrap();
    assert_eq!("replaced", team_a.description());
    assert!(team_a.resources().get("docs/readme.md").is_none());

    let team_b = repo.get_skill_in("team-b", "shared").unwrap();
    assert_eq!("Description of shared", team_b.description());
    assert_eq!(
        Some("hello"),
        team_b.resources().get("docs/readme.md").map(String::as_str)
    );
}

#[test]
fn duplicate_within_one_namespace_is_rejected() {
    // Java: duplicateWithinOneNamespaceRejected
    let (_exec, _dialect, repo) = setup();
    repo.save_in("team-a", &[sample_skill("dup")], false)
        .unwrap();
    let err = repo
        .save_in("team-a", &[sample_skill("dup")], false)
        .unwrap_err();
    assert!(is_conflict(&err), "{err}");
}

#[test]
fn delete_and_clear_stay_within_the_namespace() {
    // Java: deleteAndClearAreNamespaced
    let (exec, _dialect, repo) = setup();
    repo.save_in("team-a", &[sample_skill("gone"), sample_skill("kept")], false)
        .unwrap();
    repo.save_in("team-b", &[sample_skill("gone")], false)
        .unwrap();

    assert!(repo.delete_in("team-a", "gone").unwrap());
    assert!(!repo.skill_exists_in("team-a", "gone").unwrap());
    assert!(repo.skill_exists_in("team-b", "gone").unwrap());
    assert_eq!(vec!["kept".to_string()], repo.get_all_skill_names_in("team-a").unwrap());

    assert_eq!(1, repo.clear_all_skills_in("team-a").unwrap());
    assert!(repo.get_all_skill_names_in("team-a").unwrap().is_empty());
    assert!(repo.skill_exists_in("team-b", "gone").unwrap());
    assert_eq!(
        2,
        count_rows(&exec, "agentscope_skill_resources"),
        "only team-b's resources must remain"
    );
}

#[test]
fn invalid_namespaces_are_rejected_at_entry_points() {
    // Java: invalidNamespacesRejected
    let (exec, dialect, repo) = setup();
    for bad in ["", "   ", "team a", "team/a", "..", ".", &"a".repeat(65)] {
        assert!(
            repo.skill_exists_in(bad, "any").is_err(),
            "namespace must be rejected: {bad}"
        );
        assert!(
            Repo::new(exec.clone(), dialect.clone(), bad, true, false).is_err(),
            "constructor must reject namespace: {bad}"
        );
    }
}

#[test]
fn bound_namespace_constructor_binds_the_scope() {
    // Java: boundNamespaceConstructor
    let (exec, dialect, _repo) = setup();
    let repo = Repo::new(exec, dialect, "team-a", true, false).unwrap();

    assert_eq!("team-a", repo.namespace());
    assert!(repo.save(&[sample_skill("bound")], false).unwrap());
    assert!(repo.skill_exists("bound").unwrap());
    assert!(repo.skill_exists_in("team-a", "bound").unwrap());
    assert!(!repo.skill_exists_in("team-b", "bound").unwrap());
    assert_eq!(vec!["bound".to_string()], repo.get_all_skill_names().unwrap());
}

// ---------------------------------------------------------------------------
//  Validation and read-only — Java: ValidationTests
// ---------------------------------------------------------------------------

#[test]
fn get_skill_rejects_invalid_names() {
    // Java: skillNameValidation
    let (_exec, _dialect, repo) = setup();
    for bad in ["", "   ", "../etc/passwd", &"a".repeat(256)] {
        assert!(repo.get_skill(bad).is_err(), "name must be rejected: {bad}");
    }
}

#[test]
fn save_input_validation_rejects_bad_paths_before_writing() {
    // Java: saveInputValidation
    let (_exec, _dialect, repo) = setup();
    assert!(!repo.save(&[], false).unwrap());

    for bad_path in ["", "   ", "/etc/passwd", "..\\secrets.txt", "../x", "a/../b", "C:/temp/x"] {
        let skill = AgentSkill::builder()
            .name("paths")
            .description("d")
            .source("test")
            .add_resource(bad_path, "x")
            .build();
        assert!(
            repo.save(&[skill], false).is_err(),
            "resource path must be rejected: {bad_path}"
        );
    }
    let too_long = AgentSkill::builder()
        .name("paths")
        .description("d")
        .source("test")
        .add_resource("a".repeat(501), "x")
        .build();
    assert!(repo.save(&[too_long], false).is_err());
    assert!(!repo.skill_exists("paths").unwrap(), "no rejected save may persist anything");
}

#[test]
fn read_only_refuses_writes_and_writeable_toggles() {
    // Java: readOnlyAndToggle
    let (_exec, _dialect, repo) = setup();
    assert!(repo.is_writeable());
    repo.set_writeable(false);
    assert!(!repo.save(&[sample_skill("frozen")], false).unwrap());
    assert!(!repo.delete("frozen").unwrap());
    assert_eq!(0, repo.clear_all_skills().unwrap());
    assert!(!repo.is_writeable());

    repo.set_writeable(true);
    assert!(repo.save(&[sample_skill("frozen")], false).unwrap());
    assert!(repo.is_writeable());
}

// ---------------------------------------------------------------------------
//  Repository info and construction — Java: InfoAndConstructionTests
// ---------------------------------------------------------------------------

#[test]
fn info_and_source_carry_the_bound_namespace() {
    // Java: infoAndSource
    let (_exec, _dialect, repo) = setup();
    let info = repo.get_repository_info();
    assert_eq!("jdbc", info.repo_type);
    assert_eq!("agentscope_skills@default", info.location);
    assert!(info.writable);
    assert_eq!("jdbc_agentscope_skills@default", repo.get_source());
    assert!(repo.close().is_ok());

    let (exec, dialect, _repo) = setup();
    let team_a = Repo::new(exec, dialect, "team-a", true, false).unwrap();
    assert_eq!("jdbc_agentscope_skills@team-a", team_a.get_source());
    assert_eq!("agentscope_skills@team-a", team_a.get_repository_info().location);
}

#[test]
fn path_shaped_namespaces_are_rejected_at_construction() {
    // Java: pathShapedNamespacesRejectedAtConstruction
    let (exec, dialect, _repo) = setup();
    for bad in [".", "..", "team/a", "team\\a"] {
        assert!(
            Repo::new(exec.clone(), dialect.clone(), bad, true, false).is_err(),
            "path-shaped namespace must be rejected: {bad}"
        );
    }
}

#[test]
fn default_namespace_constant_is_bound() {
    // Java: defaultNamespaceConstant
    let (_exec, _dialect, repo) = setup();
    assert_eq!("default", repo.namespace());
}

// ---------------------------------------------------------------------------
//  Schema / degraded rows — Java: SkillTableDialectH2Test + degradedRowsAreTolerated
// ---------------------------------------------------------------------------

#[test]
fn create_if_not_exist_false_rejects_a_missing_schema() {
    // Not in Java's repository test (schema is owned by the builder there): proves the
    // Rust `create_if_not_exist=false` contract fails fast instead of silently creating.
    let executor = in_memory_executor().unwrap();
    let dialect: Arc<dyn AbstractJdbcDialect> = Arc::new(SqliteDialect::new());

    let err = Repo::new(executor.clone(), dialect.clone(), "default", true, false).unwrap_err();
    assert!(err.to_string().contains("missing") || err.to_string().contains("does not exist"), "{err}");

    let repo = Repo::with_schema(executor.clone(), dialect.clone()).unwrap();
    assert!(repo.get_all_skill_names().unwrap().is_empty());
    // Now the same schema validates.
    Repo::new(executor, dialect, "default", true, false).expect("existing schema must validate");
}

#[test]
fn legacy_table_without_metadata_json_fails_schema_validation() {
    // Java: missingColumnBlocked (SkillTableDialectH2Test)
    let executor = in_memory_executor().unwrap();
    executor
        .execute(&BoundSql::no_params(
            "CREATE TABLE agentscope_skills (\
             id INTEGER PRIMARY KEY AUTOINCREMENT, \
             namespace VARCHAR(64) NOT NULL DEFAULT 'default', \
             name TEXT NOT NULL, description TEXT NOT NULL, \
             skill_content TEXT NOT NULL, source TEXT NOT NULL, \
             created_at TIMESTAMP, updated_at TIMESTAMP, \
             UNIQUE (namespace, name))",
        ))
        .unwrap();
    let dialect: Arc<dyn AbstractJdbcDialect> = Arc::new(SqliteDialect::new());

    let err = Repo::with_schema(executor, dialect).unwrap_err();
    assert!(err.to_string().contains("metadata_json"), "{err}");
    assert!(err.to_string().to_uppercase().contains("CREATE TABLE"), "{err}");
}

#[test]
fn malformed_metadata_json_degrades_and_columns_are_authoritative() {
    // Java: degradedRowsAreTolerated (malformed JSON + JSON name/description disagreement)
    let (exec, _dialect, repo) = setup();
    exec.execute(&BoundSql::new(
        "INSERT INTO agentscope_skills (namespace, name, description, skill_content, source, metadata_json) \
         VALUES ('default', 'valid', 'd', 'c', 'test', ?)",
        vec![SqlParam::Text(
            r#"{"name":"json-name","description":"json-desc","homepage":"h"}"#.to_string(),
        )],
    ))
    .unwrap();
    exec.execute(&BoundSql::new(
        "INSERT INTO agentscope_skills (namespace, name, description, skill_content, source, metadata_json) \
         VALUES ('default', 'badjson', 'd', 'c', 'test', 'not-json')",
        vec![],
    ))
    .unwrap();

    let skills = repo.get_all_skills().unwrap();
    assert_eq!(vec!["badjson", "valid"], skills.iter().map(|s| s.name()).collect::<Vec<_>>());
    let mut degraded_keys: Vec<String> = skills[0].metadata().keys().cloned().collect();
    degraded_keys.sort();
    assert_eq!(
        vec!["description".to_string(), "name".to_string()],
        degraded_keys,
        "malformed metadata_json must degrade to core metadata"
    );
    let valid = &skills[1];
    assert_eq!("valid", valid.name(), "SQL columns must win over metadata_json");
    assert_eq!("d", valid.description());
    assert_eq!(Some(&json!("h")), valid.metadata().get("homepage"));
}

#[test]
fn orphaned_resources_are_tolerated_on_catalog_reads() {
    // Java: degradedRowsAreTolerated (orphan row)
    let (exec, _dialect, repo) = setup();
    repo.save(&[sample_skill("kept")], false).unwrap();
    // H2's test disables referential integrity for this staging; SQLite's analogue is
    // turning foreign-key enforcement off for the two raw statements.
    exec.execute(&BoundSql::no_params("PRAGMA foreign_keys = OFF"))
        .unwrap();
    exec.execute(&BoundSql::new(
        "INSERT INTO agentscope_skill_resources (namespace, id, resource_path, resource_content) \
         VALUES ('default', 99999, 'x.md', 'x')",
        vec![],
    ))
    .unwrap();
    exec.execute(&BoundSql::no_params("PRAGMA foreign_keys = ON"))
        .unwrap();

    let skills = repo.get_all_skills().unwrap();
    assert_eq!(vec!["kept"], skills.iter().map(|s| s.name()).collect::<Vec<_>>());
}

#[test]
fn legacy_traversal_rows_are_refused_on_read() {
    // Java: legacyTraversalRowsRefusedOnRead
    let (exec, _dialect, repo) = setup();
    exec.execute(&BoundSql::new(
        "INSERT INTO agentscope_skills (namespace, name, description, skill_content, source) \
         VALUES ('default', 'legacy', 'd', 'c', 'test')",
        vec![],
    ))
    .unwrap();
    exec.execute(&BoundSql::new(
        "INSERT INTO agentscope_skill_resources (namespace, id, resource_path, resource_content) \
         VALUES ('default', (SELECT id FROM agentscope_skills WHERE name = 'legacy'), \
                 '../../escape.txt', 'x')",
        vec![],
    ))
    .unwrap();
    exec.execute(&BoundSql::new(
        "INSERT INTO agentscope_skills (namespace, name, description, skill_content, source) \
         VALUES ('default', '../evil', 'd', 'c', 'test')",
        vec![],
    ))
    .unwrap();

    let err = repo.get_skill("legacy").unwrap_err();
    assert!(err.to_string().contains("../../escape.txt"), "{err}");

    assert!(repo.get_all_skills().unwrap().is_empty(), "the guard must extend to reads");
    let names = repo.get_all_skill_names().unwrap();
    assert!(names.contains(&"legacy".to_string()));
    assert!(!names.contains(&"../evil".to_string()));
}

#[test]
fn not_null_failure_is_not_reported_as_a_conflict() {    // Java: batch-failure test / narrow classifier: a non-duplicate constraint failure
    // must surface as a database error, never as the "use force=true" conflict.
    let (exec, _dialect, repo) = setup();
    // Force a non-unique constraint failure on the resources table.
    exec.execute(&BoundSql::no_params(
        "CREATE TRIGGER poison_resource BEFORE INSERT ON agentscope_skill_resources \
         WHEN NEW.resource_content = 'poison' \
         BEGIN SELECT RAISE(ABORT, 'CHECK constraint failed: poison'); END",
    ))
    .unwrap();
    let skill = AgentSkill::builder()
        .name("batch")
        .description("d")
        .source("test")
        .add_resource("ok.md", "content")
        .add_resource("broken.md", "poison")
        .build();

    let err = repo.save(&[skill], false).unwrap_err();
    assert!(!is_conflict(&err), "a CHECK violation is not a duplicate conflict: {err}");
    assert!(!repo.skill_exists("batch").unwrap(), "the skill row must be rolled back");
    assert!(!exists_by_name(&exec, "agentscope_skills", "batch"));
}

#[test]
fn skill_content_column_holds_skill_md_body_not_description() {
    // Java `skill_content` is the skill body; the harness folds that body into the
    // `SKILL.md` resource (same contract as agentscope-extensions-skills' mysql backend).
    let (exec, _dialect, repo) = setup();
    let with_body = AgentSkill::builder()
        .name("body")
        .description("short description")
        .source("test")
        .add_resource("SKILL.md", "# Body\n\nInstructions.")
        .add_resource("docs/readme.md", "hello")
        .build();
    let without_body = AgentSkill::builder()
        .name("plain")
        .description("plain description")
        .source("test")
        .build();
    repo.save(&[with_body, without_body], false).unwrap();

    let rows = exec
        .query(&BoundSql::new(
            "SELECT skill_content, description FROM agentscope_skills \
             WHERE namespace = ? AND name = ?",
            vec![
                SqlParam::Text("default".to_string()),
                SqlParam::Text("body".to_string()),
            ],
        ))
        .unwrap();
    assert_eq!(Some("# Body\n\nInstructions."), rows[0][0].as_text());
    assert_eq!(Some("short description"), rows[0][1].as_text());
    assert_ne!(
        rows[0][0].as_text(),
        rows[0][1].as_text(),
        "skill_content must not be the description"
    );

    let plain = exec
        .query(&BoundSql::new(
            "SELECT skill_content FROM agentscope_skills \
             WHERE namespace = ? AND name = ?",
            vec![
                SqlParam::Text("default".to_string()),
                SqlParam::Text("plain".to_string()),
            ],
        ))
        .unwrap();
    assert_eq!(
        Some(""),
        plain[0][0].as_text(),
        "missing SKILL.md must store an empty body"
    );
}

// ---------------------------------------------------------------------------
//  Dialect DDL parity — Java: SkillTableDialectH2Test.allVendorDdlsParse /
//  allVendorDdlsDeclareNamespacedUnique
// ---------------------------------------------------------------------------

fn vendors() -> Vec<Arc<dyn AbstractJdbcDialect>> {
    vec![
        Arc::new(AnsiDialect::new()),
        Arc::new(MysqlDialect::new()),
        Arc::new(PostgresDialect::new()),
        Arc::new(SqliteDialect::new()),
    ]
}

#[test]
fn every_vendor_skill_ddl_parses_to_the_declared_columns() {
    // Java: allVendorDdlsParse
    let expected_skills = vec![
        "id", "namespace", "name", "description", "skill_content", "source", "metadata_json",
        "created_at", "updated_at",
    ];
    let expected_resources = vec![
        "id", "namespace", "resource_path", "resource_content", "created_at", "updated_at",
    ];
    for dialect in vendors() {
        let skill_ddl = &dialect.skill_create_table_ddls()[0];
        assert_eq!(
            Some(expected_skills.iter().map(|s| s.to_string()).collect::<Vec<_>>()),
            parse_create_table_columns(skill_ddl),
            "{:?} skill DDL must parse",
            dialect.kind()
        );
        let resource_ddl = &dialect.skill_resources_create_table_ddls()[0];
        assert_eq!(
            Some(expected_resources.iter().map(|s| s.to_string()).collect::<Vec<_>>()),
            parse_create_table_columns(resource_ddl),
            "{:?} resource DDL must parse",
            dialect.kind()
        );
    }
}

#[test]
fn every_vendor_declares_namespaced_unique_and_default_table_names() {
    // Java: allVendorDdlsDeclareNamespacedUnique
    for dialect in vendors() {
        let ddl = dialect.skill_create_table_ddls()[0].to_ascii_uppercase();
        let compact: String = ddl.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            compact.contains("UNIQUE KEY UK_NAMESPACE_NAME (NAMESPACE, NAME)")
                || compact.contains("UNIQUE (NAMESPACE, NAME)"),
            "{:?} must declare UNIQUE(namespace, name): {compact}",
            dialect.kind()
        );
        assert!(
            !compact.contains("NAME TEXT NOT NULL UNIQUE")
                && !compact.contains("NAME VARCHAR(255) NOT NULL UNIQUE"),
            "{:?} must not keep a global UNIQUE(name)",
            dialect.kind()
        );
        assert_eq!("agentscope_skills", dialect.resolved_skill_table_name());
        assert_eq!(
            "agentscope_skill_resources",
            dialect.resolved_skill_resources_table_name()
        );
    }
}

#[test]
fn builder_prefix_and_per_table_overrides_apply_to_skill_tables() {
    // Java: DialectBuilderTableGroupsTest / AbstractJdbcDialectBuilder table-name overrides.
    let executor: SharedExecutor = in_memory_executor().unwrap();
    let dialect = JdbcDialectBuilder::from_product_name("SQLite")
        .table_prefix("app_")
        .unwrap()
        .build();
    assert_eq!("app_skills", dialect.resolved_skill_table_name());
    assert_eq!("app_skill_resources", dialect.resolved_skill_resources_table_name());
    let repo = Repo::with_schema(executor.clone(), dialect).unwrap();
    repo.save(&[sample_skill("prefixed")], false).unwrap();
    assert_eq!(1, count_rows(&executor, "app_skills"));
    assert_eq!(2, count_rows(&executor, "app_skill_resources"));

    let override_dialect = JdbcDialectBuilder::from_product_name("SQLite")
        .skill_table_name("my_skills")
        .unwrap()
        .skill_resources_table_name("my_skill_resources")
        .unwrap()
        .build();
    assert_eq!("my_skills", override_dialect.resolved_skill_table_name());
    assert_eq!(
        "my_skill_resources",
        override_dialect.resolved_skill_resources_table_name()
    );
}



