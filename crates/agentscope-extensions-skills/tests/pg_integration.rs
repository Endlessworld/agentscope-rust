//! SKILLS-09: real-server integration tests for the PostgreSQL backend.
//!
//! Everything here was previously exercised only against [`InMemorySqlDatabase`]: the DDL, the
//! `?` -> `$n` placeholder rewriting, `INSERT ... RETURNING id` and the `RESOURCE_BATCH_SIZE`
//! chunking. These cases need a live server, so they are gated on `AGENTSCOPE_TEST_PG_URL` and
//! **return early** when it is unset (skip semantics, no `#[ignore]`), keeping the default run
//! green on machines without a database.
//!
//! ```powershell
//! $env:AGENTSCOPE_TEST_PG_URL = "host=localhost user=postgres password=postgres dbname=postgres"
//! cargo test -p agentscope-extensions-skills --features postgres-driver --test pg_integration
//! ```
#![cfg(feature = "postgres-driver")]

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use agentscope_extensions_skills::{
    AgentSkillRepository, PgSqlDatabase, PostgresSkillRepository, PostgresSkillRepositoryConfig,
    SkillRepositoryError, SqlConnection, SqlDatabase, SqlParam,
};
use agentscope_harness::agent::skill::model::AgentSkill;

/// Number of extra resources: above `RESOURCE_BATCH_SIZE` so the chunking path runs for real.
const RESOURCE_COUNT: usize = 1500;

fn pg_url() -> Option<String> {
    match std::env::var("AGENTSCOPE_TEST_PG_URL") {
        Ok(url) if !url.trim().is_empty() => Some(url),
        _ => {
            eprintln!(
                "skipping: set AGENTSCOPE_TEST_PG_URL to run the PostgreSQL integration test"
            );
            None
        }
    }
}

fn unique_table(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{prefix}_{}_{nanos}", std::process::id())
}

fn full_table(schema: &str, table: &str) -> String {
    format!("\"{schema}\".\"{table}\"")
}

#[test]
fn crud_and_batched_resources_round_trip() {
    let Some(url) = pg_url() else {
        return;
    };
    let db = Arc::new(PgSqlDatabase::connect(&url).expect("connect to AGENTSCOPE_TEST_PG_URL"));
    let skills_table = unique_table("skills_it");
    let resources_table = unique_table("resources_it");
    let repo = PostgresSkillRepository::with_config(
        Arc::clone(&db),
        PostgresSkillRepositoryConfig::new()
            .skills_table_name(skills_table.clone())
            .resources_table_name(resources_table.clone()),
    )
    .expect("create the schema and the two tables");
    let schema = repo.schema_name().to_string();

    let mut builder = AgentSkill::builder()
        .name("calc")
        .source("integration")
        .description("A calculator")
        .add_resource(
            "SKILL.md",
            "---\nname: calc\ndescription: A calculator\n---\n# Calc\nrun it\n",
        );
    for i in 0..RESOURCE_COUNT {
        builder = builder.add_resource(format!("r/{i}.txt"), "x");
    }
    let skill = builder.build();

    assert!(
        repo.save(std::slice::from_ref(&skill), false)
            .expect("save"),
        "save must report a write"
    );

    // Raw column check: `skill_content` holds the body (SKILLS-08) and `SKILL.md` is not a
    // resource row, which is exactly what a Java reader sharing the table expects.
    let mut conn = db.connection().expect("second session");
    let sql = format!(
        "SELECT skill_content FROM {} WHERE name = ?",
        full_table(&schema, &skills_table)
    );
    let rows = conn
        .query(&sql, &[SqlParam::Text("calc".to_string())])
        .expect("skill_content query");
    assert_eq!(rows.len(), 1, "one skills row");
    let content = rows[0].get_text("skill_content").unwrap_or_default();
    assert!(!content.contains("---"), "frontmatter leaked: {content}");
    assert!(content.contains("# Calc"), "body lost: {content}");

    let resources_sql = format!(
        "SELECT resource_path FROM {}",
        full_table(&schema, &resources_table)
    );
    let resource_rows = conn.query(&resources_sql, &[]).expect("resource query");
    assert_eq!(resource_rows.len(), RESOURCE_COUNT, "all chunks landed");
    assert!(
        resource_rows
            .iter()
            .all(|row| row.get_text("resource_path").as_deref() != Some("SKILL.md")),
        "SKILL.md must not be a resource row"
    );
    drop(conn);

    let loaded = repo.get_skill("calc").expect("get_skill");
    assert_eq!(loaded.name(), "calc");
    assert_eq!(loaded.description(), "A calculator");
    assert_eq!(
        loaded.resources().len(),
        RESOURCE_COUNT + 1,
        "the body comes back as SKILL.md on top of every resource"
    );
    assert_eq!(
        repo.get_all_skill_names().expect("names"),
        vec!["calc".to_string()]
    );
    assert!(repo.skill_exists("calc").expect("exists"));

    // `force = true` replaces the skill: the resources must not be duplicated.
    assert!(repo.save(&[skill], true).expect("force save"));
    let mut conn = db.connection().expect("third session");
    let count = conn
        .query(&resources_sql, &[])
        .expect("resource recount")
        .len();
    assert_eq!(count, RESOURCE_COUNT, "force save must not duplicate rows");
    drop(conn);
    assert_eq!(repo.get_all_skills().expect("all skills").len(), 1);

    assert!(repo.delete("calc").expect("delete"));
    assert!(!repo.skill_exists("calc").expect("absent"));
    assert!(matches!(
        repo.get_skill("calc"),
        Err(SkillRepositoryError::NotFound(_))
    ));

    // Cleanup so repeated runs against the same server leave nothing behind.
    let mut conn = db.connection().expect("cleanup session");
    conn.execute(
        &format!("DROP TABLE {}", full_table(&schema, &resources_table)),
        &[],
    )
    .expect("drop resources table");
    conn.execute(
        &format!("DROP TABLE {}", full_table(&schema, &skills_table)),
        &[],
    )
    .expect("drop skills table");
}
