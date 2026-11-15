//! The DDL: the file a deployment applies, and the statements a test can assert on.

/// The whole schema, comments included. This is what `docs/DURABLE_JDBC.md` describes.
pub const SCHEMA_SQL: &str = include_str!("../schema.sql");

/// The schema as the statements a client executes in order.
///
/// Comment lines are removed before splitting: a comment that mentions a semicolon would
/// otherwise cut a statement in half. Every statement is idempotent, so applying the schema on
/// each connection is safe and there is no migration table to keep in step for this version.
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

    /// The parts rows are the store's access path: equality on the scope, ordered range on the
    /// key. Both the primary key and the `"C"` collation are load-bearing, so they are asserted
    /// where a reader of the DDL can see why.
    #[test]
    fn the_part_table_is_keyed_for_the_pages_the_store_issues() {
        let part = statements()
            .into_iter()
            .find(|statement| {
                statement.contains("agentscope_part (")
                    || statement.contains("CREATE TABLE IF NOT EXISTS agentscope_part")
            })
            .expect("the part table");
        assert!(part.contains("PRIMARY KEY (scope_key, k)"), "{part}");
        assert!(part.contains("COLLATE \"C\""), "{part}");
        assert!(part.contains("jsonb"), "{part}");
        assert!(
            part.contains("REFERENCES agentscope_scope (scope_key) ON DELETE CASCADE"),
            "{part}"
        );
    }

    /// The revision is the compare-and-set token, and a scope row that could be written without
    /// one would make a commit unable to tell a stale read from a current one.
    #[test]
    fn the_scope_table_carries_the_revision() {
        let scope = statements()
            .into_iter()
            .find(|statement| statement.contains("agentscope_scope ("))
            .expect("the scope table");
        assert!(scope.contains("revision"), "{scope}");
        assert!(scope.contains("bigint"), "{scope}");
        assert!(scope.contains("PRIMARY KEY"), "{scope}");
    }
}
