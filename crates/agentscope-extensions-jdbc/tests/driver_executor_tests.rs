//! Offline tests for the driver-backed executor (`backend::driver`).
//!
//! No database and no driver crate is needed: [`RecordingClient`] stands in for a real
//! `SqlClient` and records every statement, so the threading, the transaction gate and the
//! native advisory-lock SQL identities are all exercised for real (the fake is injected
//! **through** [`DriverExecutor::connect`], i.e. the production assembly path, not by poking
//! private fields).
//!
//! Java counterparts: `MysqlDialect.tryEnter` / `PostgresDialect.tryEnter` (lock SQL) and the
//! JDBC transaction usage of `JdbcAgentSkillRepository` (commit/rollback).

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentscope_extensions_jdbc::backend::driver::{
    to_dollar_placeholders, DriverExecutor, SqlClient,
};
use agentscope_extensions_jdbc::backend::{
    ColumnValue, JdbcError, JdbcExecutor, JdbcResult, JdbcSession,
};
use agentscope_extensions_jdbc::dialect::{BoundSql, SqlParam};
use agentscope_extensions_jdbc::sandbox::NativeLockFlavor;

// ---------------------------------------------------------------------------
//  Fake driver: records statements, answers the advisory-lock probes
// ---------------------------------------------------------------------------

/// Bookkeeping shared between `RecordingClient` instances that speak the same config.
#[derive(Default)]
struct Recorder {
    statements: Mutex<Vec<(String, Vec<SqlParam>)>>,
    threads: Mutex<Vec<std::thread::ThreadId>>,
    connections: AtomicUsize,
    closed: AtomicUsize,
}

impl Recorder {
    fn record(&self, sql: &str, params: &[SqlParam]) {
        self.threads
            .lock()
            .unwrap()
            .push(std::thread::current().id());
        self.statements
            .lock()
            .unwrap()
            .push((sql.to_string(), params.to_vec()));
    }

    fn sql(&self) -> Vec<String> {
        self.statements
            .lock()
            .unwrap()
            .iter()
            .map(|(sql, _)| sql.clone())
            .collect()
    }

    fn last_params(&self) -> Vec<SqlParam> {
        self.statements
            .lock()
            .unwrap()
            .last()
            .map(|(_, params)| params.clone())
            .unwrap_or_default()
    }

    fn threads(&self) -> Vec<std::thread::ThreadId> {
        self.threads.lock().unwrap().clone()
    }

    fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    fn closed(&self) -> usize {
        self.closed.load(Ordering::SeqCst)
    }
}

/// Token → recorder. Tokens keep parallel tests from sharing a fake connection.
static REGISTRY: Mutex<Vec<(u32, Arc<Recorder>)>> = Mutex::new(Vec::new());
static NEXT_TOKEN: AtomicU32 = AtomicU32::new(1);

/// Registers a fresh recorder and returns the configuration string that selects it.
fn register() -> (String, Arc<Recorder>) {
    let token = NEXT_TOKEN.fetch_add(1, Ordering::SeqCst);
    let recorder = Arc::new(Recorder::default());
    REGISTRY.lock().unwrap().push((token, recorder.clone()));
    (format!("fake:{token}"), recorder)
}

fn recorder_for(config: &str) -> Option<Arc<Recorder>> {
    let token: u32 = config.strip_prefix("fake:")?.parse().ok()?;
    REGISTRY
        .lock()
        .unwrap()
        .iter()
        .find(|(candidate, _)| *candidate == token)
        .map(|(_, recorder)| recorder.clone())
}

/// An in-memory [`SqlClient`]; never touches a database.
struct RecordingClient {
    recorder: Arc<Recorder>,
}

impl Drop for RecordingClient {
    fn drop(&mut self) {
        self.recorder.closed.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl SqlClient for RecordingClient {
    async fn connect(config: &str) -> JdbcResult<Self> {
        let recorder = recorder_for(config).ok_or_else(|| {
            JdbcError::Configuration(format!("unknown fake configuration '{config}'"))
        })?;
        recorder.connections.fetch_add(1, Ordering::SeqCst);
        Ok(Self { recorder })
    }

    async fn execute(&mut self, sql: &str, params: &[SqlParam]) -> JdbcResult<usize> {
        self.recorder.record(sql, params);
        if sql.contains("boom") {
            return Err(JdbcError::database(format!("fake failure for {sql}")));
        }
        Ok(1)
    }

    async fn query(
        &mut self,
        sql: &str,
        params: &[SqlParam],
    ) -> JdbcResult<Vec<Vec<ColumnValue>>> {
        self.recorder.record(sql, params);
        let row = |value: i64| vec![vec![ColumnValue::Int(value)]];
        let rows = if sql.contains("IS_USED_LOCK") {
            row(42)
        } else if sql.contains("pg_locks") {
            row(0)
        } else if sql.contains("pg_try_advisory_lock")
            || sql.contains("pg_advisory_unlock")
            || sql.contains("GET_LOCK")
            || sql.contains("RELEASE_LOCK")
        {
            row(1)
        } else if sql.contains("SELECT answer") {
            vec![vec![ColumnValue::Int(7)], vec![ColumnValue::Null]]
        } else {
            Vec::new()
        };
        Ok(rows)
    }
}

fn connect(
    native_lock_flavor: Option<NativeLockFlavor>,
) -> (DriverExecutor<RecordingClient>, Arc<Recorder>) {
    let (config, recorder) = register();
    let executor = DriverExecutor::<RecordingClient>::connect(config, native_lock_flavor)
        .expect("the fake driver connects");
    (executor, recorder)
}

// ---------------------------------------------------------------------------
//  Placeholder rewriting (PostgreSQL `$n`)
// ---------------------------------------------------------------------------

#[test]
fn dollar_placeholders_are_numbered_in_positional_order() {
    assert_eq!(
        to_dollar_placeholders("INSERT INTO t (a, b) VALUES (?, ?) ON CONFLICT (a) DO UPDATE SET b = ?"),
        "INSERT INTO t (a, b) VALUES ($1, $2) ON CONFLICT (a) DO UPDATE SET b = $3"
    );
    assert_eq!(to_dollar_placeholders("SELECT 1"), "SELECT 1");
    assert_eq!(to_dollar_placeholders(""), "");
}

// ---------------------------------------------------------------------------
//  Threading: statements run on the driver thread, not on the caller's
// ---------------------------------------------------------------------------

#[test]
fn statements_run_on_the_dedicated_driver_thread() {
    let (executor, recorder) = connect(None);
    executor
        .execute(&BoundSql::text("INSERT one", vec!["a".to_string()]))
        .unwrap();
    executor
        .execute(&BoundSql::text("INSERT two", vec!["b".to_string()]))
        .unwrap();

    let threads = recorder.threads();
    assert_eq!(threads.len(), 2);
    assert_ne!(threads[0], std::thread::current().id());
    // One connection ⇒ one thread for every statement.
    assert_eq!(threads[0], threads[1]);
    // Parameters survive the hop onto the driver thread.
    assert_eq!(recorder.last_params(), vec![SqlParam::Text("b".to_string())]);
}

#[test]
fn query_returns_the_driver_rows() {
    let (executor, _recorder) = connect(None);
    let rows = executor
        .query(&BoundSql::no_params("SELECT answer"))
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![ColumnValue::Int(7)],
            vec![ColumnValue::Null],
        ]
    );
}

#[test]
fn driver_errors_are_surfaced_with_their_message() {
    let (executor, _recorder) = connect(None);
    match executor.execute(&BoundSql::no_params("INSERT boom")) {
        Err(JdbcError::Database(db)) => assert!(db.message().contains("boom"), "{db}"),
        other => panic!("expected a database error, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
//  Transactions (the reason `in_transaction` must be overridden)
// ---------------------------------------------------------------------------

#[test]
fn in_transaction_wraps_the_work_in_begin_and_commit() {
    let (executor, recorder) = connect(None);
    executor
        .in_transaction(&mut |session: &mut dyn JdbcSession| {
            session.execute(&BoundSql::no_params("INSERT skill"))?;
            session.execute(&BoundSql::no_params("INSERT resource"))?;
            Ok(())
        })
        .unwrap();

    assert_eq!(
        recorder.sql(),
        vec![
            "BEGIN".to_string(),
            "INSERT skill".to_string(),
            "INSERT resource".to_string(),
            "COMMIT".to_string(),
        ]
    );
}

#[test]
fn in_transaction_rolls_back_when_the_work_fails() {
    let (executor, recorder) = connect(None);
    let result = executor.in_transaction(&mut |session: &mut dyn JdbcSession| {
        session.execute(&BoundSql::no_params("INSERT skill"))?;
        Err(JdbcError::database("resource insert failed".to_string()))
    });

    match result {
        Err(JdbcError::Database(db)) => assert!(db.message().contains("resource insert failed")),
        other => panic!("expected the closure error, got {other:?}"),
    }
    assert_eq!(
        recorder.sql(),
        vec![
            "BEGIN".to_string(),
            "INSERT skill".to_string(),
            "ROLLBACK".to_string(),
        ]
    );
}

#[test]
fn a_statement_from_another_thread_waits_for_the_open_transaction() {
    let (executor, recorder) = connect(None);
    let executor = Arc::new(executor);
    let outside_finished = Arc::new(AtomicBool::new(false));
    let mut outside: Option<std::thread::JoinHandle<JdbcResult<usize>>> = None;

    {
        let executor_for_thread = executor.clone();
        let flag = outside_finished.clone();
        let mut work = |session: &mut dyn JdbcSession| -> JdbcResult<()> {
            session.execute(&BoundSql::no_params("INSERT inside"))?;
            let executor = executor_for_thread.clone();
            let flag = flag.clone();
            outside = Some(std::thread::spawn(move || {
                let result = executor.execute(&BoundSql::no_params("UPDATE outside"));
                flag.store(true, Ordering::SeqCst);
                result
            }));
            std::thread::sleep(Duration::from_millis(100));
            assert!(
                !outside_finished.load(Ordering::SeqCst),
                "a statement outside the transaction must not run concurrently with it"
            );
            assert!(!recorder.sql().iter().any(|sql| sql.contains("outside")));
            Ok(())
        };
        executor.in_transaction(&mut work).unwrap();
    }

    outside.take().unwrap().join().unwrap().unwrap();
    let statements = recorder.sql();
    let commit = statements
        .iter()
        .position(|sql| sql == "COMMIT")
        .expect("commit recorded");
    let outside_index = statements
        .iter()
        .position(|sql| sql.contains("outside"))
        .expect("the other thread's statement ran");
    assert!(
        outside_index > commit,
        "the statement must only run after the commit: {statements:?}"
    );
}

#[test]
fn execute_batch_is_atomic() {
    let (executor, recorder) = connect(None);
    executor
        .execute_batch(&[
            BoundSql::no_params("INSERT a"),
            BoundSql::no_params("INSERT b"),
        ])
        .unwrap();
    assert_eq!(
        recorder.sql(),
        vec![
            "BEGIN".to_string(),
            "INSERT a".to_string(),
            "INSERT b".to_string(),
            "COMMIT".to_string(),
        ]
    );

    let (executor, recorder) = connect(None);
    assert!(executor
        .execute_batch(&[
            BoundSql::no_params("INSERT a"),
            BoundSql::no_params("INSERT boom"),
        ])
        .is_err());
    assert_eq!(
        recorder.sql(),
        vec![
            "BEGIN".to_string(),
            "INSERT a".to_string(),
            "INSERT boom".to_string(),
            "ROLLBACK".to_string(),
        ]
    );
}

// ---------------------------------------------------------------------------
//  Fail-closed configuration
// ---------------------------------------------------------------------------

#[test]
fn an_empty_configuration_is_rejected() {
    match DriverExecutor::<RecordingClient>::connect("   ", None) {
        Err(JdbcError::Configuration(message)) => assert!(message.contains("empty"), "{message}"),
        other => panic!("expected a configuration error, got {other:?}"),
    }
}

#[test]
fn from_env_reports_the_missing_variable() {
    const VARIABLE: &str = "AGENTSCOPE_TEST_UNSET_JDBC_DRIVER_URL";
    std::env::remove_var(VARIABLE);
    match DriverExecutor::<RecordingClient>::from_env(VARIABLE, None) {
        Err(JdbcError::Configuration(message)) => {
            assert!(message.contains(VARIABLE), "{message}");
            assert!(message.contains("not set"), "{message}");
        }
        other => panic!("expected a configuration error, got {other:?}"),
    }
}

#[test]
fn a_driver_without_a_flavor_cannot_open_native_lock_sessions() {
    let (executor, _recorder) = connect(None);
    match executor.open_native_lock_session() {
        Err(JdbcError::Unsupported(message)) => {
            assert!(message.contains("flavor"), "{message}");
            assert!(message.contains("advisory"), "{message}");
        }
        other => match other {
            Err(other) => panic!("expected Unsupported, got {other:?}"),
            Ok(_) => panic!("expected Unsupported, got an open native-lock session"),
        },
    }
}

// ---------------------------------------------------------------------------
//  Native advisory-lock sessions (one dedicated connection each)
// ---------------------------------------------------------------------------

#[test]
fn the_postgres_session_speaks_the_advisory_lock_sql() {
    let (executor, recorder) = connect(Some(NativeLockFlavor::Postgres));
    assert_eq!(recorder.connections(), 1);

    let session = executor.open_native_lock_session().unwrap();
    // A session is a *second* connection: advisory locks are session-scoped.
    assert_eq!(recorder.connections(), 2);

    assert!(session.try_acquire("1234567890", Duration::ZERO).unwrap());
    assert!(recorder.sql().last().unwrap().contains("pg_try_advisory_lock"));
    assert_eq!(recorder.last_params(), vec![SqlParam::Int(1_234_567_890)]);

    session.release("1234567890").unwrap();
    assert!(recorder.sql().last().unwrap().contains("pg_advisory_unlock"));

    assert!(!session.is_held("1234567890").unwrap());
    assert!(recorder.sql().last().unwrap().contains("pg_locks"));
}

#[test]
fn the_mysql_session_speaks_get_lock_sql() {
    let (executor, recorder) = connect(Some(NativeLockFlavor::Mysql));
    let session = executor.open_native_lock_session().unwrap();

    assert!(session
        .try_acquire("agentscope:sandbox:lock:agent:1", Duration::from_secs(5))
        .unwrap());
    assert!(recorder.sql().last().unwrap().contains("GET_LOCK"));
    assert_eq!(
        recorder.last_params(),
        vec![
            SqlParam::Text("agentscope:sandbox:lock:agent:1".to_string()),
            SqlParam::Int(5),
        ]
    );

    session
        .release("agentscope:sandbox:lock:agent:1")
        .unwrap();
    assert!(recorder.sql().last().unwrap().contains("RELEASE_LOCK"));

    // IS_USED_LOCK reports the owning session id (the fake answers 42) ⇒ held.
    assert!(session.is_held("agentscope:sandbox:lock:agent:1").unwrap());
    assert!(recorder.sql().last().unwrap().contains("IS_USED_LOCK"));
}

#[test]
fn a_non_numeric_postgres_lock_key_is_rejected() {
    let (executor, recorder) = connect(Some(NativeLockFlavor::Postgres));
    let session = executor.open_native_lock_session().unwrap();
    match session.try_acquire("not-a-key", Duration::ZERO) {
        Err(agentscope_extensions_jdbc::dialect::NativeLockError::Failed(message)) => {
            assert!(message.contains("decimal"), "{message}");
        }
        other => panic!("expected a failed session, got {other:?}"),
    }
    // The bad key never reaches the server.
    assert!(recorder.sql().is_empty());
}

#[test]
fn dropping_the_session_closes_its_connection() {
    let (executor, recorder) = connect(Some(NativeLockFlavor::Postgres));
    let session = executor.open_native_lock_session().unwrap();
    assert_eq!(recorder.connections(), 2);
    assert_eq!(recorder.closed(), 0);

    drop(session);
    // The driver thread ends when its job channel closes; wait (bounded) for the connection
    // to be dropped there.
    let deadline = Instant::now() + Duration::from_secs(5);
    while recorder.closed() < 1 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(recorder.closed(), 1, "dropping the session must close it");
}
