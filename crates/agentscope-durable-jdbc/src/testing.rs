//! An in-process [`SqlSession`] that keeps the same facts a server would.
//!
//! It exists so the store's whole `RecordStore` contract runs offline: every statement the store
//! issues is applied to a real map, inside a transaction whose writes are discarded when the
//! closure fails. Statements are recognised by identity — `==` against [`crate::statements`] —
//! not by pattern: a statement whose text changed fails this double loudly instead of being
//! silently reinterpreted, which is the failure mode a fuzzy double has.
//!
//! What it does **not** model: concurrent connections, locking, collation, `jsonb`'s type rules
//! and the planner. Those are the server's, and `tests/schema_live.rs` is where they are checked —
//! with a real database, when `DATABASE_URL` names one.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::Mutex;

use agentscope_durable::{DurableError, Result};
use serde_json::Value;

use crate::mapping::{decode, Param};
use crate::session::{Row, SqlSession, SqlTransaction};
use crate::statements as sql;

#[derive(Clone, Default)]
struct Scope {
    revision: i64,
    parts: BTreeMap<String, Value>,
}

#[derive(Clone)]
struct Entry {
    ready_at: i64,
    payload: Option<Value>,
}

#[derive(Clone, Default)]
struct Database {
    scopes: BTreeMap<String, Scope>,
    work: BTreeMap<(String, String), Entry>,
    ddl: Vec<String>,
    clock: i64,
}

type BumpHook = Box<dyn Fn(&mut i64) + Send + Sync>;

/// A session that answers the store's statements from a map.
pub struct MemoryPg {
    db: Mutex<Database>,
    statements: Mutex<Vec<String>>,
    before_bump: Mutex<Option<BumpHook>>,
}

impl Default for MemoryPg {
    fn default() -> Self {
        Self::new()
    }
}

/// The instant the double dates its transactions by.
///
/// A fixed instant is a time bomb: `backend::turn::frame` checks a lease against the host's
/// clock, so a lease claimed against a 2023 timestamp reads as expired and every ownership check
/// answers `Fenced`. Starting at the host's clock keeps the double honest; each transaction then
/// advances it by a millisecond, so ordering inside a test is still deterministic.
fn host_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

impl MemoryPg {
    pub fn new() -> Self {
        Self {
            db: Mutex::new(Database {
                clock: host_millis(),
                ..Database::default()
            }),
            statements: Mutex::new(Vec::new()),
            before_bump: Mutex::new(None),
        }
    }

    /// Runs `hook` with the stored revision before the compare-and-set is answered.
    ///
    /// That is how a test plays a second host committing in between: the hook moves the revision,
    /// the update finds it moved, and the store has to answer a conflict instead of writing.
    pub fn before_bump(mut self, hook: impl Fn(&mut i64) + Send + Sync + 'static) -> Self {
        self.before_bump = Mutex::new(Some(Box::new(hook)));
        self
    }

    /// Every statement this double was asked to run, in order — including `BEGIN`/`COMMIT`.
    pub fn statements(&self) -> Vec<String> {
        self.statements.lock().expect("statements").clone()
    }

    pub fn clear_statements(&self) {
        self.statements.lock().expect("statements").clear();
    }

    /// The revision a scope holds; `None` before its first write.
    pub fn revision(&self, scope: &str) -> Option<i64> {
        self.db
            .lock()
            .expect("database")
            .scopes
            .get(scope)
            .map(|scope| scope.revision)
    }

    /// The DDL this session was asked to apply.
    pub fn ddl(&self) -> Vec<String> {
        self.db.lock().expect("database").ddl.clone()
    }
}

impl SqlSession for MemoryPg {
    fn transaction(
        &self,
        work: &mut dyn FnMut(&mut dyn SqlTransaction) -> Result<()>,
    ) -> Result<()> {
        let mut db = self.db.lock().expect("database");
        // Every transaction works on a copy and publishes it only on commit: that is the rollback
        // a server performs, and the reason a failed closure can leave nothing behind.
        let mut working = db.clone();
        let now = working.clock;
        working.clock += 1;
        let mut ops: Vec<String> = vec!["BEGIN".to_string()];
        let outcome = {
            let mut txn = WorkingTxn {
                db: &mut working,
                now,
                ops: &mut ops,
                hook: &self.before_bump,
            };
            work(&mut txn)
        };
        let outcome = match outcome {
            Ok(()) => {
                *db = working;
                Ok(())
            }
            Err(error) => Err(error),
        };
        // The statement log is taken after the database is released: two locks, one order.
        drop(db);
        let mut log = self.statements.lock().expect("statements");
        log.extend(ops);
        log.push(match &outcome {
            Ok(()) => "COMMIT".to_string(),
            Err(_) => "ROLLBACK".to_string(),
        });
        outcome
    }

    fn query(&self, statement: &str, params: &[Param]) -> Result<Vec<Row>> {
        let mut db = self.db.lock().expect("database");
        let rows = if statement.starts_with("CREATE ") {
            // The schema is applied outside any transaction, so it is recorded here and nowhere
            // else: a double that ran DDL inside a transaction would model a rollback no server
            // performs for `CREATE TABLE IF NOT EXISTS`.
            db.ddl.push(statement.to_string());
            Ok(Vec::new())
        } else {
            read(&db, statement, params)
        };
        drop(db);
        self.statements
            .lock()
            .expect("statements")
            .push(statement.to_string());
        rows
    }
}

struct WorkingTxn<'a> {
    db: &'a mut Database,
    now: i64,
    ops: &'a mut Vec<String>,
    hook: &'a Mutex<Option<BumpHook>>,
}

impl SqlTransaction for WorkingTxn<'_> {
    fn now_millis(&mut self) -> Result<i64> {
        Ok(self.now)
    }

    fn query(&mut self, statement: &str, params: &[Param]) -> Result<Vec<Row>> {
        self.ops.push(statement.to_string());
        read(self.db, statement, params)
    }

    fn execute(&mut self, statement: &str, params: &[Param]) -> Result<u64> {
        self.ops.push(statement.to_string());
        write(self.db, statement, params, Some(self.hook))
    }
}

fn write(
    db: &mut Database,
    statement: &str,
    params: &[Param],
    hook: Option<&Mutex<Option<BumpHook>>>,
) -> Result<u64> {
    if statement == sql::ENSURE_SCOPE {
        let scope = text(params, 0)?;
        if db.scopes.contains_key(scope) {
            return Ok(0);
        }
        db.scopes.insert(scope.to_string(), Scope::default());
        return Ok(1);
    }
    if statement == sql::UPSERT_PART {
        let scope = text(params, 0)?;
        let key = text(params, 1)?.to_string();
        let value = decode(text(params, 2)?)?;
        let scope = db.scopes.get_mut(scope).ok_or_else(|| {
            DurableError::unavailable("MemoryPg: agentscope_part has no parent scope")
        })?;
        scope.parts.insert(key, value);
        return Ok(1);
    }
    if statement == sql::DELETE_PART {
        let scope = text(params, 0)?;
        let key = text(params, 1)?;
        let removed = db
            .scopes
            .get_mut(scope)
            .and_then(|scope| scope.parts.remove(key))
            .is_some();
        return Ok(u64::from(removed));
    }
    if statement == sql::BUMP_REVISION {
        let scope_key = text(params, 0)?;
        let expected = int(params, 1)?;
        let scope = db.scopes.get_mut(scope_key).ok_or_else(|| {
            DurableError::unavailable("MemoryPg: agentscope_scope row is missing")
        })?;
        if let Some(hook) = hook {
            if let Some(hook) = hook.lock().expect("hook").as_ref() {
                hook(&mut scope.revision);
            }
        }
        if scope.revision != expected {
            return Ok(0);
        }
        scope.revision += 1;
        return Ok(1);
    }
    if statement == sql::UPSERT_WORK {
        let queue = text(params, 0)?.to_string();
        let work_id = text(params, 1)?.to_string();
        let ready_at = int(params, 2)?;
        let payload = payload(params, 3)?;
        db.work
            .insert((queue, work_id), Entry { ready_at, payload });
        return Ok(1);
    }
    if statement == sql::DELETE_WORK {
        let queue = text(params, 0)?.to_string();
        let work_id = text(params, 1)?.to_string();
        return Ok(u64::from(db.work.remove(&(queue, work_id)).is_some()));
    }
    Err(unimplemented(statement))
}

fn read(db: &Database, statement: &str, params: &[Param]) -> Result<Vec<Row>> {
    if statement.starts_with("CREATE ") {
        return Ok(Vec::new());
    }
    if statement == sql::LOCK_SCOPE {
        let scope = db.scopes.get(text(params, 0)?);
        return Ok(scope
            .map(|scope| Row::new(vec![Value::from(scope.revision)]))
            .into_iter()
            .collect());
    }
    if statement == sql::SELECT_PART {
        let scope = db.scopes.get(text(params, 0)?);
        let key = text(params, 1)?;
        return Ok(scope
            .and_then(|scope| scope.parts.get(key).cloned())
            .map(|value| vec![Row::new(vec![value])])
            .unwrap_or_default());
    }
    if statement == sql::SELECT_PARTS {
        let keys = text_array(params, 1)?;
        let mut rows = Vec::new();
        if let Some(scope) = db.scopes.get(text(params, 0)?) {
            for key in scope.parts.keys() {
                if keys.iter().any(|asked| asked == key) {
                    rows.push(Row::new(vec![
                        Value::from(key.clone()),
                        scope.parts[key].clone(),
                    ]));
                }
            }
        }
        return Ok(rows);
    }
    if let Some((bound_lower, bound_upper)) = key_page_bounds(statement, params)? {
        let scope = db.scopes.get(text(params, 0)?);
        let limit = int(params, page_limit_index(statement))? as usize;
        let mut rows = Vec::new();
        if let Some(scope) = scope {
            let range = (bound_lower, Bound::Unbounded);
            for (key, _) in scope.parts.range::<str, _>(range) {
                if bound_upper
                    .as_deref()
                    .map_or(false, |upper| key.as_str() >= upper)
                {
                    break;
                }
                if rows.len() == limit {
                    break;
                }
                rows.push(Row::new(vec![Value::from(key.clone())]));
            }
        }
        return Ok(rows);
    }
    if statement == sql::READY {
        let queue = text(params, 0)?;
        let now = int(params, 1)?;
        let limit = int(params, 2)? as usize;
        let mut due: Vec<(&String, &Entry)> = db
            .work
            .iter()
            .filter(|((entry_queue, _), _)| entry_queue == queue)
            .map(|((_, work_id), entry)| (work_id, entry))
            .filter(|(_, entry)| entry.ready_at >= 0 && entry.ready_at <= now)
            .collect();
        due.sort_by(|(left_id, left), (right_id, right)| {
            left.ready_at
                .cmp(&right.ready_at)
                .then_with(|| left_id.cmp(right_id))
        });
        return Ok(due
            .into_iter()
            .take(limit)
            .map(|(work_id, entry)| {
                Row::new(vec![
                    Value::from(work_id.clone()),
                    Value::from(entry.ready_at),
                    entry.payload.clone().unwrap_or(Value::Null),
                ])
            })
            .collect());
    }
    Err(unimplemented(statement))
}

/// The range a key page selects: the lower bound and, when the prefix has one, its upper edge.
fn key_page_bounds<'a>(
    statement: &str,
    params: &'a [Param],
) -> Result<Option<(Bound<&'a str>, Option<String>)>> {
    let bounds = match statement {
        sql::KEYS_FROM_PREFIX => (Bound::Included(text(params, 1)?), None),
        sql::KEYS_AFTER => (Bound::Excluded(text(params, 1)?), None),
        sql::KEYS_FROM_PREFIX_BOUNDED => (
            Bound::Included(text(params, 1)?),
            Some(text(params, 2)?.to_string()),
        ),
        sql::KEYS_AFTER_BOUNDED => (
            Bound::Excluded(text(params, 1)?),
            Some(text(params, 2)?.to_string()),
        ),
        _ => return Ok(None),
    };
    Ok(Some(bounds))
}

/// Where the cursor sits in the parameter list of each key page.
fn page_limit_index(statement: &str) -> usize {
    match statement {
        sql::KEYS_FROM_PREFIX_BOUNDED | sql::KEYS_AFTER_BOUNDED => 3,
        _ => 2,
    }
}

fn unimplemented(statement: &str) -> DurableError {
    DurableError::unavailable(format!(
        "MemoryPg does not implement this statement: {statement}"
    ))
}

fn text(params: &[Param], index: usize) -> Result<&str> {
    match params.get(index) {
        Some(Param::Text(text)) => Ok(text),
        other => Err(parameter(index, "text", other)),
    }
}

fn int(params: &[Param], index: usize) -> Result<i64> {
    match params.get(index) {
        Some(Param::BigInt(value)) => Ok(*value),
        other => Err(parameter(index, "bigint", other)),
    }
}

fn text_array(params: &[Param], index: usize) -> Result<&[String]> {
    match params.get(index) {
        Some(Param::TextArray(values)) => Ok(values),
        other => Err(parameter(index, "text[]", other)),
    }
}

fn payload(params: &[Param], index: usize) -> Result<Option<Value>> {
    match params.get(index) {
        Some(Param::Null) => Ok(None),
        Some(Param::Text(text)) => decode(text).map(Some),
        other => Err(parameter(index, "jsonb", other)),
    }
}

fn parameter(index: usize, expected: &str, found: Option<&Param>) -> DurableError {
    DurableError::invalid(format!(
        "MemoryPg expected a {expected} parameter at {index}, found {found:?}"
    ))
}
