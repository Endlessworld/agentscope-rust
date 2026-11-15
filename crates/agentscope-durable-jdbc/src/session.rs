//! The narrow surface a driver has to provide: four methods, no decisions.
//!
//! `RecordStore` is synchronous, so the session is too: a driver owns the blocking hop (a pool, a
//! dedicated runtime thread); everything above it is here, in statements and pure mapping.

use agentscope_durable::{DurableError, Result};
use serde_json::Value;

use crate::mapping::Param;

/// One SQL transaction.
///
/// A driver answers `now_millis` from the server's clock (`clock_timestamp()`), and the store
/// uses only that value: a durable decision may not be dated by the host's own clock.
pub trait SqlTransaction: Send {
    /// Milliseconds since the epoch, from the server.
    fn now_millis(&mut self) -> Result<i64>;
    fn query(&mut self, sql: &str, params: &[Param]) -> Result<Vec<Row>>;
    /// Rows affected.
    fn execute(&mut self, sql: &str, params: &[Param]) -> Result<u64>;
}

/// A connection, or a pool standing in for one.
pub trait SqlSession: Send + Sync {
    /// Runs `work` inside one transaction: `Ok` commits, `Err` rolls back and returns the error.
    ///
    /// The transaction is what makes a durable commit atomic — the checkpoint, the tool intent
    /// and the events are the same `COMMIT` or none of them happened.
    fn transaction(
        &self,
        work: &mut dyn FnMut(&mut dyn SqlTransaction) -> Result<()>,
    ) -> Result<()>;

    /// A statement outside any transaction, for reads that must not take the scope row lock.
    fn query(&self, sql: &str, params: &[Param]) -> Result<Vec<Row>>;
}

/// One returned row. Columns are positional, in the order the statement selects them.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    values: Vec<Value>,
}

impl Row {
    pub fn new(values: Vec<Value>) -> Self {
        Self { values }
    }

    pub fn values(&self) -> &[Value] {
        &self.values
    }

    fn value(&self, index: usize) -> Result<&Value> {
        self.values.get(index).ok_or_else(|| {
            DurableError::unavailable(format!(
                "statement selected {} columns, column {index} was read",
                self.values.len()
            ))
        })
    }

    /// A `text` column.
    pub fn text(&self, index: usize) -> Result<&str> {
        match self.value(index)? {
            Value::String(text) => Ok(text),
            other => Err(mismatch(index, "text", other)),
        }
    }

    /// A `bigint` column, whether the driver handed it back as a number or as text.
    pub fn int(&self, index: usize) -> Result<i64> {
        match self.value(index)? {
            Value::Number(number) => number
                .as_i64()
                .ok_or_else(|| mismatch(index, "bigint", &Value::Number(number.clone()))),
            Value::String(text) => text
                .parse()
                .map_err(|_| mismatch(index, "bigint", &Value::String(text.clone()))),
            other => Err(mismatch(index, "bigint", other)),
        }
    }

    /// A `jsonb` column.
    ///
    /// A JSON *string* is a value, not an encoding: a driver that only speaks text has to decode
    /// its column into the `Value` it means before building the row ([`crate::mapping::decode`]
    /// does exactly that), because guessing here would turn `"RUNNING"` into a parse error.
    pub fn json(&self, index: usize) -> Result<Value> {
        Ok(self.value(index)?.clone())
    }

    /// A nullable `jsonb` column.
    pub fn json_opt(&self, index: usize) -> Result<Option<Value>> {
        match self.value(index)? {
            Value::Null => Ok(None),
            _ => self.json(index).map(Some),
        }
    }
}

fn mismatch(index: usize, expected: &str, found: &Value) -> DurableError {
    DurableError::unavailable(format!("column {index} is not {expected}: {found}"))
}
