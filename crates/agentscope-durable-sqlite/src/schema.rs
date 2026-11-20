//! The DDL: the file a deployment applies, and the statements a test can assert on.

/// The whole schema, comments included. This is what `docs/DURABLE_SQLITE.md` describes.
pub const SCHEMA_SQL: &str = include_str!("schema.sql");

/// The schema as the statements a client executes in order.
///
/// Comment lines are removed before splitting: a comment that mentions a semicolon would
/// otherwise cut a statement in half. Every statement is idempotent, so applying the schema on
/// each connection is safe and `PRAGMA user_version` is what records that a migration ran.
pub fn statements() -> Vec<String> {
    let body = SCHEMA_SQL
        .lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n");
    body.split(';')
        .map(str::trim)
        .filter(|statement| !statement.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_schema_is_three_tables_and_the_readiness_index() {
        let statements = statements();
        assert_eq!(statements.len(), 4, "{statements:#?}");
        let tables: Vec<&String> = statements
            .iter()
            .filter(|statement| statement.starts_with("CREATE TABLE"))
            .collect();
        assert_eq!(tables.len(), 3, "{tables:#?}");
        for table in ["agentscope_scope", "agentscope_part", "agentscope_work"] {
            assert!(
                statements.iter().any(|statement| statement.contains(table)),
                "the schema does not define {table}"
            );
        }
        assert!(statements
            .iter()
            .any(|statement| statement.contains("CREATE INDEX")));
    }

    /// A statement that is not idempotent cannot be applied on every connection, and a
    /// deployment that runs the schema from a startup hook would fail on its second start.
    #[test]
    fn every_statement_can_be_applied_twice() {
        for statement in statements() {
            assert!(statement.starts_with("CREATE "), "{statement}");
            assert!(
                statement.contains("IF NOT EXISTS"),
                "not idempotent: {statement}"
            );
            assert!(!statement.contains(';'), "{statement}");
            assert!(!statement.contains("--"), "{statement}");
        }
    }

    /// The parts table carries the same integrity the PostgreSQL schema has: a primary key that
    /// is the access path, a foreign key with a cascade, and a collation that matches Rust's byte
    /// order. A reader of the DDL should not have to guess which of those is load-bearing.
    #[test]
    fn the_part_table_is_keyed_and_constrained_like_the_postgres_one() {
        let part = statements()
            .into_iter()
            .find(|statement| statement.contains("CREATE TABLE IF NOT EXISTS agentscope_part"))
            .expect("the part table");
        assert!(part.contains("PRIMARY KEY (scope_key, k)"), "{part}");
        assert!(part.contains("COLLATE BINARY"), "{part}");
        assert!(
            part.contains("REFERENCES agentscope_scope (scope_key) ON DELETE CASCADE"),
            "{part}"
        );
    }

    /// The revision is the compare-and-set token, and a scope key outside the bound is refused by
    /// the PostgreSQL store; both have to answer the same way for a caller that switches stores.
    #[test]
    fn the_scope_table_carries_the_revision_and_the_key_bound() {
        let scope = statements()
            .into_iter()
            .find(|statement| statement.contains("CREATE TABLE IF NOT EXISTS agentscope_scope"))
            .expect("the scope table");
        assert!(scope.contains("revision"), "{scope}");
        assert!(scope.contains("INTEGER NOT NULL DEFAULT 0"), "{scope}");
        assert!(
            scope.contains("CHECK (length(scope_key) BETWEEN 1 AND 256)"),
            "{scope}"
        );
        assert!(scope.contains("PRIMARY KEY"), "{scope}");
    }

    /// The readiness page is ordered by `ready_at, work_id`; without the trailing column in the
    /// index SQLite sorts every page after selecting it.
    #[test]
    fn the_readiness_index_covers_the_order_by() {
        let index = statements()
            .into_iter()
            .find(|statement| statement.contains("CREATE INDEX"))
            .expect("the readiness index");
        assert!(
            index.contains("ON agentscope_work (queue, ready_at, work_id)"),
            "{index}"
        );
    }
}
