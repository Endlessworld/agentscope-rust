//! What only a server can answer.
//!
//! The mapping and the whole `RecordStore` contract are tested offline. What stays untested there
//! is whether the SQL *exists* as PostgreSQL understands it: that the DDL applies twice, that the
//! key page is an index range and not a scan, that `COLLATE "C"` orders keys the way Rust does,
//! and that the revision compare-and-set moves exactly one row.
//!
//! These run when `DATABASE_URL` names a database and `psql` is on `PATH` (see
//! `docs/DURABLE_POSTGRES.md`); otherwise each test prints what it skipped and passes, so
//! `cargo test` stays offline-clean. Nothing here talks to a driver: the statements are the ones
//! the store sends, prepared verbatim.

use std::process::Command;

use agentscope_durable_postgres::schema;
use agentscope_durable_postgres::statements as sql;

/// The database to talk to, or `None` when the test should skip.
fn database() -> Option<String> {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipped: DATABASE_URL is not set (docs/DURABLE_POSTGRES.md explains the setup)");
        return None;
    };
    match Command::new("psql").arg("--version").output() {
        Ok(_) => Some(url),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("skipped: DATABASE_URL is set but psql is not on PATH");
            None
        }
        Err(error) => panic!("psql could not be started: {error}"),
    }
}

/// Runs a script with `ON_ERROR_STOP`, so a statement the server rejects fails the test.
fn run(database: &str, script: &str) -> String {
    let output = Command::new("psql")
        .args([
            database,
            "-X",
            "-q",
            "-v",
            "ON_ERROR_STOP=1",
            "-A",
            "-t",
            "-c",
            script,
        ])
        .output()
        .expect("psql runs");
    if !output.status.success() {
        panic!(
            "psql failed:\n{}\nscript:\n{script}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn the_schema_applies_twice() {
    let Some(database) = database() else { return };
    let ddl = schema::statements().join(";\n");
    // Twice on purpose: a deployment that runs the schema from a startup hook must be able to
    // start twice. A statement that only looked idempotent fails here.
    run(&database, &format!("{ddl};"));
    run(&database, &format!("{ddl};"));
    let tables = run(
        &database,
        "SELECT count(*) FROM information_schema.tables WHERE table_name IN \
         ('agentscope_scope', 'agentscope_part', 'agentscope_work')",
    );
    assert_eq!(tables.trim(), "3", "not every table was created");
}

#[test]
fn a_key_page_is_an_index_range_and_not_a_scan() {
    let Some(database) = database() else { return };
    run(&database, &format!("{};", schema::statements().join(";\n")));
    // One scope with a journal long enough that the planner has a real choice.
    run(
        &database,
        "DELETE FROM agentscope_scope WHERE scope_key = 'live:plan';\
         INSERT INTO agentscope_scope (scope_key, revision, updated_at) \
             VALUES ('live:plan', 0, 0);\
         INSERT INTO agentscope_part (scope_key, k, v) \
             SELECT 'live:plan', 'journal:' || lpad(g::text, 6, '0'), '{}'::jsonb \
             FROM generate_series(1, 4000) g;\
         ANALYZE agentscope_part;",
    );
    // The statement the store sends, prepared verbatim: the plan under test is the plan the
    // store will get, not a hand-written query that resembles it.
    let plan = run(
        &database,
        &format!(
            "PREPARE page(text, text, text, int) AS {};\
             EXPLAIN (COSTS OFF) EXECUTE page('live:plan', 'journal:', 'journal;', 50);",
            sql::KEYS_FROM_PREFIX_BOUNDED
        ),
    );
    assert!(
        plan.contains("Index"),
        "the key page is not an index range:\n{plan}"
    );
    assert!(
        !plan.contains("Seq Scan"),
        "the key page scans the table:\n{plan}"
    );
}

#[test]
fn the_collation_orders_keys_the_way_rust_does() {
    let Some(database) = database() else { return };
    run(&database, &format!("{};", schema::statements().join(";\n")));
    let order = run(
        &database,
        "DELETE FROM agentscope_scope WHERE scope_key = 'live:order';\
         INSERT INTO agentscope_scope (scope_key, revision, updated_at) \
             VALUES ('live:order', 0, 0);\
         INSERT INTO agentscope_part (scope_key, k, v) \
             VALUES ('live:order', 'b', '{}'), ('live:order', 'A', '{}'), \
                    ('live:order', 'a', '{}'), ('live:order', 'á', '{}'), \
                    ('live:order', 'Z', '{}');\
         SELECT string_agg(k, '' ORDER BY k) FROM agentscope_part \
             WHERE scope_key = 'live:order';",
    );
    // Byte order: uppercase before lowercase, ASCII before Latin-1. A server-configured collation
    // would answer `aAbZá` or similar, and the journal pages would silently disagree with the
    // in-memory store about where a page ends.
    assert_eq!(
        order.trim(),
        "AZabá",
        "the key column is not ordered by bytes"
    );
}

#[test]
fn the_revision_compare_and_set_moves_one_row_or_none() {
    let Some(database) = database() else { return };
    run(&database, &format!("{};", schema::statements().join(";\n")));
    run(
        &database,
        "DELETE FROM agentscope_scope WHERE scope_key = 'live:cas';\
         INSERT INTO agentscope_scope (scope_key, revision, updated_at) \
             VALUES ('live:cas', 0, 0);",
    );
    let stale = run(
        &database,
        &format!(
            "UPDATE agentscope_scope SET revision = revision + 1, updated_at = 1 \
             WHERE scope_key = 'live:cas' AND revision = 99;\n{}",
            sql::BUMP_REVISION
        ),
    );
    // Two statements: the literal one and the store's own, both with a stale expectation.
    assert!(
        stale.contains("UPDATE 0"),
        "a stale revision wrote a row: {stale}"
    );
    let fresh = run(
        &database,
        &format!(
            "UPDATE agentscope_scope SET revision = revision + 1, updated_at = 1 \
             WHERE scope_key = 'live:cas' AND revision = 0;\n{}",
            sql::BUMP_REVISION
        ),
    );
    assert_eq!(
        fresh.matches("UPDATE 1").count(),
        2,
        "the revision did not advance exactly once per statement: {fresh}"
    );
    let revision = run(
        &database,
        "SELECT revision FROM agentscope_scope WHERE scope_key = 'live:cas'",
    );
    assert_eq!(
        revision.trim(),
        "2",
        "the revision is not the number of writes"
    );
}

#[test]
fn a_work_entry_is_due_once_and_its_removal_is_a_deleted_row() {
    let Some(database) = database() else { return };
    run(&database, &format!("{};", schema::statements().join(";\n")));
    run(
        &database,
        "DELETE FROM agentscope_work WHERE queue = 'live:queue';\
         INSERT INTO agentscope_work (queue, work_id, ready_at, payload) \
             VALUES ('live:queue', 'b', 5, '{\"thread\":\"main\"}'::jsonb), \
                    ('live:queue', 'a', 10, NULL), \
                    ('live:queue', 'late', 9999, NULL);",
    );
    let due = run(
        &database,
        &format!(
            "PREPARE due(text, bigint, int) AS {};\
                  EXECUTE due('live:queue', 10, 10);",
            sql::READY
        ),
    );
    let lines: Vec<&str> = due.lines().filter(|line| !line.is_empty()).collect();
    assert_eq!(lines.len(), 2, "not the due entries: {due}");
    assert!(lines[0].starts_with("b|5|"), "not readiness order: {due}");
    assert!(lines[1].starts_with("a|10|"), "not readiness order: {due}");
    assert!(
        lines[0].contains("main"),
        "the payload of a due turn is not carried: {due}"
    );
    let removed = run(
        &database,
        &format!(
            "PREPARE drop(text, text) AS {};\nEXECUTE drop('live:queue', 'b');",
            sql::DELETE_WORK
        ),
    );
    assert!(removed.contains("DELETE 1"), "{removed}");
    let empty = run(
        &database,
        "SELECT count(*) FROM agentscope_work WHERE queue = 'live:queue' AND work_id = 'b'",
    );
    assert_eq!(empty.trim(), "0");
}
