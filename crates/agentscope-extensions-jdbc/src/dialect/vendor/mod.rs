//! Vendor dialects.
//!
//! One module per database; each extends the ANSI baseline in
//! [`super::AnsiDialect`] and overrides only the divergent DDL/UPSERT syntax.

pub mod ansi;
pub mod mysql;
pub mod postgres;
pub mod sqlite;

pub use ansi::AnsiDialect;
pub use mysql::MysqlDialect;
pub use postgres::PostgresDialect;
pub use sqlite::SqliteDialect;
