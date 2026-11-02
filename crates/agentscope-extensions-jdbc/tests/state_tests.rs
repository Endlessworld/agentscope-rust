//! `JdbcAgentStateStore` offline integration tests, mirroring the Java
//! `JdbcAgentStateStoreH2Test`.
//!
//! Backend: in-memory SQLite (`RusqliteExecutor::in_memory()`) with `SqliteDialect` — the Rust
//! analogue of Java's H2 + `H2Dialect` pairing.
//!
//! Note on the versioned-API subset: core's `AgentStateStore` trait exposes no versioning
//! surface, so the Java `supportsVersioning` / `getVersioned` / `saveIfVersion` cases are
//! covered against the inherent methods of [`JdbcAgentStateStore`] instead (see §3.2).

use std::sync::Arc;

use agentscope_extensions_jdbc::backend::is_unique_violation;
use agentscope_extensions_jdbc::dialect::vendor::SqliteDialect;
use agentscope_extensions_jdbc::dialect::{
    AbstractJdbcDialect, BoundSql, DialectKind, SessionStateDialect, SkillDialect,
    SkillResourcesDialect, SnapshotDialect, StoreDialect, TableNameConfigurable, TableNames,
};
use agentscope_extensions_jdbc::{
    AgentStateStore, ColumnValue, JdbcAgentStateStore, JdbcError, JdbcExecutor, JdbcResult,
    JdbcSession, RusqliteExecutor, UNVERSIONED,
};

fn new_store() -> JdbcAgentStateStore {
    let executor = Arc::new(RusqliteExecutor::in_memory().unwrap());
    let dialect: Arc<dyn AbstractJdbcDialect> = Arc::new(SqliteDialect::new());
    JdbcAgentStateStore::with_schema(executor, dialect).unwrap()
}

/// Builds a store plus a handle to its executor, so tests can inspect raw rows.
fn new_store_with_executor() -> (JdbcAgentStateStore, Arc<RusqliteExecutor>) {
    let executor = Arc::new(RusqliteExecutor::in_memory().unwrap());
    let dialect: Arc<dyn AbstractJdbcDialect> = Arc::new(SqliteDialect::new());
    let store = JdbcAgentStateStore::with_schema(executor.clone(), dialect.clone()).unwrap();
    (store, executor)
}

/// Reads the raw `state_data` of a single state row, bypassing JSON decoding.
///
/// Needed for the `<key>:_hash` companion row, whose value is a bare hash string (mirroring
/// Java, which stores `ListHashUtil.computeHash(...)` verbatim) rather than a JSON document.
fn raw_state_data(
    executor: &RusqliteExecutor,
    dialect: &dyn SessionStateDialect,
    slot_id: &str,
    key: &str,
) -> Option<String> {
    let bound = dialect.session_state_select(slot_id, key, 0);
    executor
        .query(&bound)
        .unwrap()
        .into_iter()
        .next()
        .and_then(|r| r.into_iter().next())
        .and_then(|c| c.as_text().map(|s| s.to_string()))
}

fn text(s: &str) -> serde_json::Value {
    serde_json::json!({ "value": s })
}

fn text_of(v: &serde_json::Value) -> String {
    v.get("value")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string()
}

/// What the injected "competing writer" writes when the interpose point is reached.
#[derive(Debug, Clone, Copy)]
enum CompetingWrite {
    /// Overwrite the single key `u:s` / `k` (an `UNVERSIONED`-style UPSERT that bumps
    /// `version` and replaces the payload).
    SingleKey,
    /// Rewrite the list `u:s` / `l` to `[a1, b2]` — a concurrent writer whose append raced
    /// with ours.
    ListAppend,
}

/// Wraps a real executor to observe how many statements a call issues, and to let a test
/// inject a *competing writer* at a precise point (GAP-01 / JDBC-01 / JDBC-02).
///
/// Interposition happens **after** the triggering statement has completed but before the
/// caller regains control. That is exactly the window a non-atomic read-modify-write (or
/// "UPSERT then SELECT version") exposes: the write lands, another writer commits, and the
/// follow-up read reports the other writer's state. A single-statement or transactional
/// implementation cannot be corrupted this way, and statements that run inside
/// `in_transaction` never reach this wrapper at all.
#[derive(Debug)]
struct InstrumentedExecutor {
    inner: Arc<RusqliteExecutor>,
    /// Statements touching the session-state table issued *outside* a transaction, in order.
    session_statements: std::sync::atomic::AtomicUsize,
    /// Statements touching the session-state table issued *inside* `in_transaction`.
    transaction_statements: std::sync::atomic::AtomicUsize,
    /// Statements that aborted with a unique/primary-key violation.
    conflicts: std::sync::atomic::AtomicUsize,
    /// Fire the competing write right after the Nth outside-transaction session statement
    /// (`usize::MAX` = disarmed). Starts disarmed so schema creation is not counted.
    interpose_after: std::sync::atomic::AtomicUsize,
    interposed: std::sync::atomic::AtomicBool,
    competing: CompetingWrite,
}

impl InstrumentedExecutor {
    fn new(inner: Arc<RusqliteExecutor>, competing: CompetingWrite) -> Self {
        Self {
            inner,
            session_statements: std::sync::atomic::AtomicUsize::new(0),
            transaction_statements: std::sync::atomic::AtomicUsize::new(0),
            conflicts: std::sync::atomic::AtomicUsize::new(0),
            interpose_after: std::sync::atomic::AtomicUsize::new(usize::MAX),
            interposed: std::sync::atomic::AtomicBool::new(false),
            competing,
        }
    }

    /// Starts counting from zero and arms the competing write for after outside-transaction
    /// statement `n`.
    fn arm_after(&self, n: usize) {
        self.session_statements
            .store(0, std::sync::atomic::Ordering::SeqCst);
        self.transaction_statements
            .store(0, std::sync::atomic::Ordering::SeqCst);
        self.conflicts.store(0, std::sync::atomic::Ordering::SeqCst);
        self.interposed
            .store(false, std::sync::atomic::Ordering::SeqCst);
        self.interpose_after
            .store(n, std::sync::atomic::Ordering::SeqCst);
    }

    /// Stops injecting the competing writer; later statements are only counted.
    fn disarm(&self) {
        self.interpose_after
            .store(usize::MAX, std::sync::atomic::Ordering::SeqCst);
    }

    fn session_statement_count(&self) -> usize {
        self.session_statements
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    fn transaction_statement_count(&self) -> usize {
        self.transaction_statements
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    fn conflict_count(&self) -> usize {
        self.conflicts.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn was_interposed(&self) -> bool {
        self.interposed.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn record_error(&self, error: Option<&JdbcError>) {
        if let Some(error) = error {
            if is_unique_violation(error) {
                self.conflicts
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }

    /// A competing writer's committed write, injected exactly at the read/write boundary.
    fn interpose_competing_write(&self) {
        let dialect = SqliteDialect::new();
        match self.competing {
            CompetingWrite::SingleKey => {
                let bound = dialect.session_state_upsert(
                    "u:s",
                    "k",
                    0,
                    &serde_json::to_string(&text("competitor")).unwrap(),
                );
                self.inner
                    .execute(&bound)
                    .expect("competing write must succeed");
            }
            CompetingWrite::ListAppend => {
                self.inner
                    .execute(&dialect.session_state_delete_by_key("u:s", "l"))
                    .expect("competing rewrite must succeed");
                for (index, value) in [text("a1"), text("b2")].iter().enumerate() {
                    let bound = dialect.session_state_insert(
                        "u:s",
                        "l",
                        index as i32,
                        &serde_json::to_string(value).unwrap(),
                    );
                    self.inner
                        .execute(&bound)
                        .expect("competing append must succeed");
                }
                let hash =
                    agentscope_core::state::list_hash::compute_hash(&[text("a1"), text("b2")]);
                let bound = dialect.session_state_upsert("u:s", "l:_hash", 0, &hash);
                self.inner
                    .execute(&bound)
                    .expect("competing hash write must succeed");
            }
        }
    }

    fn observe(&self, sql: &str) {
        if !sql.to_ascii_uppercase().contains("AGENTSCOPE_SESSIONS") {
            return;
        }
        let n = self
            .session_statements
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        if self
            .interpose_after
            .load(std::sync::atomic::Ordering::SeqCst)
            == n
            && !self
                .interposed
                .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            self.interpose_competing_write();
        }
    }
}

/// Connection-scoped view over the inner transaction: counts statements and duplicate-key
/// aborts that happen **inside** it.
struct CountingSession<'a> {
    inner: &'a dyn JdbcSession,
    statements: &'a std::sync::atomic::AtomicUsize,
    conflicts: &'a std::sync::atomic::AtomicUsize,
}

impl CountingSession<'_> {
    fn observe(&self, error: Option<&JdbcError>) {
        self.statements
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some(error) = error {
            if is_unique_violation(error) {
                self.conflicts
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }
}

impl JdbcSession for CountingSession<'_> {
    fn execute(&self, bound: &BoundSql) -> JdbcResult<usize> {
        let result = self.inner.execute(bound);
        self.observe(result.as_ref().err());
        result
    }

    fn query(&self, bound: &BoundSql) -> JdbcResult<Vec<Vec<ColumnValue>>> {
        let result = self.inner.query(bound);
        self.observe(result.as_ref().err());
        result
    }
}

impl JdbcExecutor for InstrumentedExecutor {
    fn execute(&self, bound: &BoundSql) -> JdbcResult<usize> {
        let result = self.inner.execute(bound);
        self.record_error(result.as_ref().err());
        self.observe(&bound.sql);
        result
    }

    fn query(&self, bound: &BoundSql) -> JdbcResult<Vec<Vec<ColumnValue>>> {
        let result = self.inner.query(bound);
        self.record_error(result.as_ref().err());
        self.observe(&bound.sql);
        result
    }

    /// Delegates to the inner backend's transaction, wrapping the session so the test can see
    /// how many statements ran **inside** it and whether any of them aborted on a duplicate
    /// key. Statements inside the transaction never reach this wrapper's `execute`/`query`,
    /// which is exactly why a competing writer can no longer be injected mid-transaction: the
    /// transaction is the fix, and this observation is its proof.
    fn in_transaction(
        &self,
        work: &mut dyn FnMut(&mut dyn JdbcSession) -> JdbcResult<()>,
    ) -> JdbcResult<()> {
        self.inner
            .in_transaction(&mut |session: &mut dyn JdbcSession| {
                let mut counting = CountingSession {
                    inner: &*session,
                    statements: &self.transaction_statements,
                    conflicts: &self.conflicts,
                };
                work(&mut counting)
            })
    }
}

fn new_instrumented_store() -> (JdbcAgentStateStore, Arc<InstrumentedExecutor>) {
    new_instrumented_store_with(Arc::new(SqliteDialect::new()), CompetingWrite::SingleKey)
}

fn new_instrumented_store_with(
    dialect: Arc<dyn AbstractJdbcDialect>,
    competing: CompetingWrite,
) -> (JdbcAgentStateStore, Arc<InstrumentedExecutor>) {
    let inner = Arc::new(RusqliteExecutor::in_memory().unwrap());
    let instrumented = Arc::new(InstrumentedExecutor::new(inner, competing));
    let executor: Arc<dyn JdbcExecutor> = instrumented.clone();
    let store = JdbcAgentStateStore::with_schema(executor, dialect).unwrap();
    (store, instrumented)
}

// ---------------------------------------------------------------------------
//  Java: JdbcAgentStateStoreH2Test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn constructor_with_create_if_not_exist_false_throws_when_table_missing() {
    // Java: JdbcAgentStateStoreH2Test.constructorVerifiesTableExists
    let executor = Arc::new(RusqliteExecutor::in_memory().unwrap());
    let dialect: Arc<dyn AbstractJdbcDialect> = Arc::new(SqliteDialect::new());
    let result = JdbcAgentStateStore::new(executor, dialect, false);
    assert!(
        result.is_err(),
        "verifying a missing table must fail when create_if_not_exist=false"
    );
}

#[tokio::test]
async fn constructor_with_create_if_not_exist_true_creates_table() {
    // Java: JdbcAgentStateStoreH2Test.setUp (new JdbcAgentStateStore(ds, dialect, true))
    let executor = Arc::new(RusqliteExecutor::in_memory().unwrap());
    let dialect: Arc<dyn AbstractJdbcDialect> = Arc::new(SqliteDialect::new());
    let store = JdbcAgentStateStore::new(executor, dialect, true).unwrap();
    // A freshly created store has no sessions yet.
    assert!(!store.exists(Some("user1"), "s1").await);
}

#[tokio::test]
async fn save_and_get_single_state() {
    // Java: JdbcAgentStateStoreH2Test.saveAndGetSingleState
    let store = new_store();
    store
        .save(Some("user1"), "session1", "key", text("hello"))
        .await;

    let result = store.get(Some("user1"), "session1", "key").await;
    assert_eq!(result.map(|v| text_of(&v)), Some("hello".to_string()));
}

#[tokio::test]
async fn get_returns_empty_for_missing() {
    // Java: JdbcAgentStateStoreH2Test.getReturnsEmptyForMissing
    let store = new_store();
    assert!(store
        .get(Some("user1"), "session1", "missing")
        .await
        .is_none());
}

#[tokio::test]
async fn save_overwrites_single() {
    // Java: JdbcAgentStateStoreH2Test.saveOverwritesSingle
    let store = new_store();
    store.save(Some("user1"), "s1", "k", text("v1")).await;
    store.save(Some("user1"), "s1", "k", text("v2")).await;
    assert_eq!(
        store
            .get(Some("user1"), "s1", "k")
            .await
            .map(|v| text_of(&v)),
        Some("v2".to_string())
    );
}

#[tokio::test]
async fn save_and_get_list_state() {
    // Java: JdbcAgentStateStoreH2Test.saveAndGetListState
    let store = new_store();
    let messages = vec![text("msg1"), text("msg2"), text("msg3")];
    store
        .save_list(Some("user1"), "s1", "messages", &messages)
        .await;

    let result = store.get_list(Some("user1"), "s1", "messages").await;
    assert_eq!(result.len(), 3);
    assert_eq!(text_of(&result[0]), "msg1");
    assert_eq!(text_of(&result[2]), "msg3");
}

#[tokio::test]
async fn save_list_incremental_append() {
    // Java: JdbcAgentStateStoreH2Test.saveListIncrementalAppend
    let store = new_store();
    store
        .save_list(Some("user1"), "s1", "list", &[text("a"), text("b")])
        .await;
    store
        .save_list(
            Some("user1"),
            "s1",
            "list",
            &[text("a"), text("b"), text("c")],
        )
        .await;

    let result = store.get_list(Some("user1"), "s1", "list").await;
    assert_eq!(result.len(), 3);
    assert_eq!(text_of(&result[2]), "c");
}

#[tokio::test]
async fn save_list_modified_prefix_rewrites() {
    // Java: JdbcAgentStateStoreH2Test.saveListModifiedPrefixRewrites
    let store = new_store();
    store
        .save_list(Some("user1"), "s1", "list", &[text("a"), text("b")])
        .await;
    // Modify the first element — hash changes, triggering full rewrite.
    store
        .save_list(Some("user1"), "s1", "list", &[text("CHANGED"), text("b")])
        .await;

    let result = store.get_list(Some("user1"), "s1", "list").await;
    assert_eq!(result.len(), 2);
    assert_eq!(text_of(&result[0]), "CHANGED");
}

#[tokio::test]
async fn save_list_shrink_rewrites() {
    // Java: JdbcAgentStateStoreH2Test.saveListShrinkRewrites
    let store = new_store();
    store
        .save_list(
            Some("user1"),
            "s1",
            "list",
            &[text("a"), text("b"), text("c")],
        )
        .await;
    store
        .save_list(Some("user1"), "s1", "list", &[text("a")])
        .await;

    let result = store.get_list(Some("user1"), "s1", "list").await;
    assert_eq!(result.len(), 1);
}

#[tokio::test]
async fn list_hash_companion_row_is_written() {
    // Java: JdbcAgentStateStore.save(list) stores a `<key>:_hash` companion row
    // (HASH_KEY_SUFFIX = ":_hash") holding the raw hash string. The row is inspected through
    // a raw SELECT because `get()` JSON-decodes its payload (the hash is not a JSON document,
    // exactly as in Java).
    let (store, executor) = new_store_with_executor();
    let dialect = SqliteDialect::new();
    store
        .save_list(Some("user1"), "s1", "list", &[text("a"), text("b")])
        .await;

    let hash = raw_state_data(&executor, &dialect, "user1:s1", "list:_hash");
    let hash = hash.expect("the list hash companion row must exist");
    assert_eq!(
        hash,
        agentscope_core::state::list_hash::compute_hash(&[text("a"), text("b")])
    );

    // The hash row must not leak into the list projection.
    let list = store.get_list(Some("user1"), "s1", "list").await;
    assert_eq!(list.len(), 2);
}

#[tokio::test]
async fn empty_list_save_is_a_noop() {
    // Java: JdbcAgentStateStore.save(list) returns early when values.isEmpty().
    let (store, executor) = new_store_with_executor();
    let dialect = SqliteDialect::new();
    store.save_list(Some("user1"), "s1", "list", &[]).await;
    assert!(!store.exists(Some("user1"), "s1").await);
    assert!(raw_state_data(&executor, &dialect, "user1:s1", "list:_hash").is_none());
}

#[tokio::test]
async fn exists_returns_true() {
    // Java: JdbcAgentStateStoreH2Test.existsReturnsTrue
    let store = new_store();
    store.save(Some("user1"), "s1", "k", text("v")).await;
    assert!(store.exists(Some("user1"), "s1").await);
}

#[tokio::test]
async fn exists_returns_false() {
    // Java: JdbcAgentStateStoreH2Test.existsReturnsFalse
    let store = new_store();
    assert!(!store.exists(Some("user1"), "nonexistent").await);
}

#[tokio::test]
async fn delete_removes_session() {
    // Java: JdbcAgentStateStoreH2Test.deleteRemovesSession
    let store = new_store();
    store.save(Some("user1"), "s1", "k1", text("v1")).await;
    store.save(Some("user1"), "s1", "k2", text("v2")).await;
    store.delete(Some("user1"), "s1").await;

    assert!(!store.exists(Some("user1"), "s1").await);
    assert!(store.get(Some("user1"), "s1", "k1").await.is_none());
}

#[tokio::test]
async fn list_session_ids_returns_for_user() {
    // Java: JdbcAgentStateStoreH2Test.listSessionIdsReturnsForUser
    let store = new_store();
    store.save(Some("user1"), "s1", "k", text("v")).await;
    store.save(Some("user1"), "s2", "k", text("v")).await;
    store.save(Some("user2"), "s3", "k", text("v")).await;

    let sessions = store.list_session_ids(Some("user1")).await;
    assert_eq!(sessions.len(), 2);
    assert!(sessions.contains(&"s1".to_string()));
    assert!(sessions.contains(&"s2".to_string()));
}

#[tokio::test]
async fn anonymous_user_grouping() {
    // Java: JdbcAgentStateStoreH2Test.anonymousUserGrouping
    let store = new_store();
    store.save(None, "anon-session", "k", text("v")).await;

    let sessions = store.list_session_ids(None).await;
    assert!(sessions.contains(&"anon-session".to_string()));
}

#[tokio::test]
async fn list_session_ids_escapes_like_wildcards() {
    // Java: JdbcAgentStateStoreH2Test.listSessionIdsEscapesLikeWildcards
    let store = new_store();
    store.save(Some("u_anon_x"), "sess", "k", text("v")).await;
    store.save(None, "anon-session", "k", text("v")).await;

    let anon_sessions = store.list_session_ids(None).await;
    assert!(anon_sessions.contains(&"anon-session".to_string()));
    assert!(
        !anon_sessions.contains(&"sess".to_string()),
        "non-anon session must not leak into the anonymous list"
    );

    let real_sessions = store.list_session_ids(Some("u_anon_x")).await;
    assert!(real_sessions.contains(&"sess".to_string()));
}

#[tokio::test]
async fn list_session_ids_escapes_percent_and_underscore() {
    // Java: JdbcAgentStateStoreH2Test.listSessionIdsEscapesPercentAndUnderscore
    let store = new_store();
    store.save(Some("a_%_b"), "s1", "k", text("v")).await;

    let sessions = store.list_session_ids(Some("a_%_b")).await;
    assert_eq!(sessions.len(), 1);
    assert!(sessions.contains(&"s1".to_string()));
}

// ---------------------------------------------------------------------------
//  Extra coverage: user/session isolation and validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sessions_are_isolated_between_users() {
    // Java: JdbcAgentStateStore.slotId = normalizeUser(userId) + ":" + sessionId
    let store = new_store();
    store.save(Some("user1"), "s1", "k", text("one")).await;
    store.save(Some("user2"), "s1", "k", text("two")).await;

    assert_eq!(
        store
            .get(Some("user1"), "s1", "k")
            .await
            .map(|v| text_of(&v)),
        Some("one".to_string())
    );
    assert_eq!(
        store
            .get(Some("user2"), "s1", "k")
            .await
            .map(|v| text_of(&v)),
        Some("two".to_string())
    );
}

#[tokio::test]
#[allow(non_snake_case)]
async fn blank_session_or_key_is_ignored() {
    // Java: JdbcAgentStateStore.validateSlotId / validateStateKey
    let store = new_store();
    store.save(Some("user1"), "", "k", text("v")).await;
    store.save(Some("user1"), "s1", "", text("v")).await;
    assert!(!store.exists(Some("user1"), "").await);
    assert!(!store.exists(Some("user1"), "s1").await);
    assert!(store.get(Some("user1"), "s1", "k").await.is_none());
}

#[tokio::test]
async fn incremental_append_preserves_prior_items_and_hash() {
    // Java: JdbcAgentStateStore.save(list) incremental-append branch: only the tail beyond
    // the stored prefix is inserted, and the hash companion row is updated.
    let (store, executor) = new_store_with_executor();
    let dialect = SqliteDialect::new();
    store
        .save_list(Some("u"), "s", "l", &[text("a"), text("b")])
        .await;
    let hash_before = raw_state_data(&executor, &dialect, "u:s", "l:_hash").unwrap();

    store
        .save_list(
            Some("u"),
            "s",
            "l",
            &[text("a"), text("b"), text("c"), text("d")],
        )
        .await;

    let list = store.get_list(Some("u"), "s", "l").await;
    assert_eq!(
        list.iter().map(text_of).collect::<Vec<_>>(),
        vec!["a", "b", "c", "d"]
    );
    let hash_after = raw_state_data(&executor, &dialect, "u:s", "l:_hash").unwrap();
    assert_ne!(hash_before, hash_after, "the hash row must track the list");
    assert_eq!(
        hash_after,
        agentscope_core::state::list_hash::compute_hash(&[
            text("a"),
            text("b"),
            text("c"),
            text("d")
        ])
    );
}

// ---------------------------------------------------------------------------
//  Java: AgentStateStoreVersioningContractTest / JdbcAgentStateStoreH2Test
//  (versioned subset — inherent API, since core's trait has no versioning surface)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn supports_versioning_is_true() {
    assert!(new_store().supports_versioning());
}

#[tokio::test]
async fn version_of_absent_key_is_zero() {
    let store = new_store();
    assert_eq!(store.read_version(Some("u"), "s", "k").unwrap(), 0);
}

#[tokio::test]
async fn get_versioned_absent_key_is_absent_with_version_zero() {
    let store = new_store();
    let vs = store.get_versioned(Some("u"), "s", "k").unwrap();
    assert!(!vs.is_present());
    assert_eq!(vs.value, None);
    assert_eq!(vs.version, 0);
}

#[tokio::test]
async fn save_if_version_zero_creates_and_returns_one() {
    let store = new_store();
    assert_eq!(
        store
            .save_if_version(Some("u"), "s", "k", text("a"), 0)
            .unwrap(),
        1
    );
    let vs = store.get_versioned(Some("u"), "s", "k").unwrap();
    assert!(vs.is_present());
    assert_eq!(text_of(vs.value.as_ref().unwrap()), "a");
    assert_eq!(vs.version, 1);
}

#[tokio::test]
async fn save_if_version_zero_conflicts_on_existing_key() {
    let store = new_store();
    store
        .save_if_version(Some("u"), "s", "k", text("a"), 0)
        .unwrap();
    assert_eq!(
        store
            .save_if_version(Some("u"), "s", "k", text("b"), 0)
            .unwrap(),
        UNVERSIONED,
        "create-if-absent must report a conflict once the key exists"
    );
    // The conflicting write must not clobber the stored value.
    assert_eq!(text_of(&store.get(Some("u"), "s", "k").await.unwrap()), "a");
    assert_eq!(store.read_version(Some("u"), "s", "k").unwrap(), 1);
}

#[tokio::test]
async fn save_if_version_matching_bumps_version() {
    let store = new_store();
    store
        .save_if_version(Some("u"), "s", "k", text("a"), 0)
        .unwrap();
    assert_eq!(
        store
            .save_if_version(Some("u"), "s", "k", text("b"), 1)
            .unwrap(),
        2
    );
    assert_eq!(text_of(&store.get(Some("u"), "s", "k").await.unwrap()), "b");
}

#[tokio::test]
async fn save_if_version_stale_is_rejected_without_writing() {
    let store = new_store();
    store
        .save_if_version(Some("u"), "s", "k", text("a"), 0)
        .unwrap();
    assert_eq!(
        store
            .save_if_version(Some("u"), "s", "k", text("b"), 7)
            .unwrap(),
        UNVERSIONED
    );
    assert_eq!(text_of(&store.get(Some("u"), "s", "k").await.unwrap()), "a");
    assert_eq!(store.read_version(Some("u"), "s", "k").unwrap(), 1);
}

#[tokio::test]
async fn save_if_version_unversioned_overwrites_unconditionally() {
    let store = new_store();
    store
        .save_if_version(Some("u"), "s", "k", text("a"), 0)
        .unwrap();
    let returned = store
        .save_if_version(Some("u"), "s", "k", text("b"), UNVERSIONED)
        .unwrap();
    assert_ne!(
        returned, UNVERSIONED,
        "unconditional overwrite always succeeds"
    );
    assert_eq!(returned, store.read_version(Some("u"), "s", "k").unwrap());
    assert_eq!(text_of(&store.get(Some("u"), "s", "k").await.unwrap()), "b");
}

#[tokio::test]
async fn versioned_reads_are_scoped_per_user() {
    let store = new_store();
    store
        .save_if_version(Some("alice"), "s", "k", text("a"), 0)
        .unwrap();
    assert_eq!(store.read_version(Some("bob"), "s", "k").unwrap(), 0);
    assert!(!store
        .get_versioned(Some("bob"), "s", "k")
        .unwrap()
        .is_present());
}

#[tokio::test]
async fn versioned_api_rejects_blank_session_or_key() {
    let store = new_store();
    assert!(store.read_version(Some("u"), "  ", "k").is_err());
    assert!(store.get_versioned(Some("u"), "s", "").is_err());
    assert!(store
        .save_if_version(Some("u"), "", "k", text("a"), 0)
        .is_err());
    assert!(store
        .save_if_version(Some("u"), "s", " ", text("a"), 0)
        .is_err());
}

// ---------------------------------------------------------------------------
//  GAP-01: `UNVERSIONED` write-then-read must be one round trip
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unversioned_write_returns_the_version_a_following_read_sees() {
    // (a) The returned version is exactly the one `read_version` reports right after.
    let store = new_store();
    let returned = store
        .save_if_version(Some("u"), "s", "k", text("a"), UNVERSIONED)
        .unwrap();
    assert_eq!(
        returned,
        store.read_version(Some("u"), "s", "k").unwrap(),
        "the version returned by an UNVERSIONED write must match the stored version"
    );
    // A fresh row inserted by the UPSERT starts at 1 (the statement binds `version = 1`; the
    // DDL's `DEFAULT 0` only applies to `session_state_insert`, which omits the column).
    assert_eq!(returned, 1);
    assert_eq!(text_of(&store.get(Some("u"), "s", "k").await.unwrap()), "a");
}

#[tokio::test]
async fn consecutive_unversioned_writes_increase_the_version() {
    // (b) Repeated unconditional overwrites keep bumping the version monotonically, and each
    // returned version is the committed one.
    let store = new_store();
    let first = store
        .save_if_version(Some("u"), "s", "k", text("a"), UNVERSIONED)
        .unwrap();
    let second = store
        .save_if_version(Some("u"), "s", "k", text("b"), UNVERSIONED)
        .unwrap();
    let third = store
        .save_if_version(Some("u"), "s", "k", text("c"), UNVERSIONED)
        .unwrap();

    assert_eq!((first, second, third), (1, 2, 3));
    assert!(
        second > first && third > second,
        "versions must increase monotonically"
    );
    assert_eq!(third, store.read_version(Some("u"), "s", "k").unwrap());
    assert_eq!(text_of(&store.get(Some("u"), "s", "k").await.unwrap()), "c");
}

#[tokio::test]
async fn unversioned_write_is_one_statement_and_ignores_a_concurrent_writer() {
    // The atomicity guarantee, made deterministic: a competing writer commits immediately
    // after the store's *first* session-state statement.
    //
    //  * Single-statement (`RETURNING`) implementation: the UPSERT already produced version 1
    //    and returned it; the competitor's later bump to 2 cannot change what we report.
    //  * Write-then-read implementation: the UPSERT lands at 1, the competitor bumps to 2,
    //    and the follow-up SELECT wrongly reports 2 — a version this call never wrote.
    let (store, instrumented) = new_instrumented_store();
    instrumented.arm_after(1);

    let returned = store
        .save_if_version(Some("u"), "s", "k", text("mine"), UNVERSIONED)
        .unwrap();

    assert!(
        instrumented.was_interposed(),
        "the competing write must have been injected; otherwise this test proves nothing"
    );
    assert_eq!(
        instrumented.session_statement_count(),
        1,
        "an UNVERSIONED write must issue exactly one statement (write + version in one trip)"
    );
    assert_eq!(
        returned, 1,
        "must report the version this write produced, not a concurrent writer's"
    );
    // The competitor's write is the one that ended up last-write-wins on the payload; the
    // point is only that our *version* is attributable to our own write.
    assert_eq!(store.read_version(Some("u"), "s", "k").unwrap(), 2);
}

// ---------------------------------------------------------------------------
//  GAP-02: `version = 0` rows must not be permanently un-writable
// ---------------------------------------------------------------------------

/// Pre-seeds a row the way the production code can leave one behind: a `session_state_insert`
/// (the list-element INSERT) omits the `version` column, so it lands on the DDL default `0`.
fn seed_version_zero_row(
    executor: &RusqliteExecutor,
    slot_id: &str,
    key: &str,
    value: &serde_json::Value,
) {
    let dialect = SqliteDialect::new();
    executor
        .execute(&dialect.session_state_insert(
            slot_id,
            key,
            0,
            &serde_json::to_string(value).unwrap(),
        ))
        .expect("seeding a version-0 row must succeed");
}

fn raw_version(executor: &RusqliteExecutor, slot_id: &str, key: &str) -> i64 {
    let dialect = SqliteDialect::new();
    executor
        .query(&dialect.session_state_select(slot_id, key, 0))
        .unwrap()
        .into_iter()
        .next()
        .and_then(|r| r.into_iter().nth(1))
        .and_then(|c| c.as_int())
        .unwrap_or(0)
}

#[tokio::test]
async fn save_if_version_zero_self_heals_a_stale_version_zero_row() {
    // (a) A row sitting on the DDL default must be writable by create-if-absent.
    let (store, executor) = new_store_with_executor();
    seed_version_zero_row(&executor, "u:s", "k", &text("stale"));
    assert_eq!(
        raw_version(&executor, "u:s", "k"),
        0,
        "fixture must be version 0"
    );

    assert_eq!(
        store
            .save_if_version(Some("u"), "s", "k", text("fresh"), 0)
            .unwrap(),
        1,
        "a version-0 row must be accepted as create-if-absent, not reported as a conflict"
    );

    assert_eq!(
        text_of(&store.get(Some("u"), "s", "k").await.unwrap()),
        "fresh"
    );
    assert_eq!(store.read_version(Some("u"), "s", "k").unwrap(), 1);
}

#[tokio::test]
async fn save_if_version_zero_does_not_overwrite_a_versioned_row() {
    // (b) The self-heal must stay a CAS, not become an unconditional overwrite.
    let store = new_store();
    store
        .save_if_version(Some("u"), "s", "k", text("a"), 0)
        .unwrap();
    assert_eq!(
        store
            .save_if_version(Some("u"), "s", "k", text("b"), 1)
            .unwrap(),
        2
    );
    assert_eq!(
        store
            .save_if_version(Some("u"), "s", "k", text("c"), 2)
            .unwrap(),
        3
    );
    assert_eq!(store.read_version(Some("u"), "s", "k").unwrap(), 3);

    // Version is 3 here; expected=0 must still conflict and leave the data untouched.
    assert_eq!(
        store
            .save_if_version(Some("u"), "s", "k", text("d"), 0)
            .unwrap(),
        UNVERSIONED,
        "expected=0 against a version-3 row must still conflict"
    );
    assert_eq!(
        text_of(&store.get(Some("u"), "s", "k").await.unwrap()),
        "c",
        "a rejected create-if-absent must not modify the stored value"
    );
    assert_eq!(store.read_version(Some("u"), "s", "k").unwrap(), 3);
}

// ---------------------------------------------------------------------------
//  JDBC-01: `save_list`'s read-modify-write is one unit of work
// ---------------------------------------------------------------------------

#[tokio::test]
async fn save_list_append_is_atomic_against_a_writer_committing_mid_write() {
    // Java: JdbcAgentStateStore.save(lists) wraps the whole read-modify-write in
    // `executeInWriteTransaction`. The competing writer is injected exactly between this
    // call's reads (`:_hash`, `MAX(item_index)`) and its writes — the window a
    // non-transactional implementation leaves open:
    //
    //  * without the transaction the stale `existing` count makes the tail INSERT collide
    //    with the competitor's row, the batch is rolled back and `save_list` only logs a
    //    warning — an append silently lost;
    //  * with it no statement of this call runs outside the transaction, so the reads and
    //    writes are one unit of work and no duplicate-key abort can lose the append.
    let (store, instrumented) =
        new_instrumented_store_with(Arc::new(SqliteDialect::new()), CompetingWrite::ListAppend);
    // Base state: the list already holds `a1`.
    store.save_list(Some("u"), "s", "l", &[text("a1")]).await;
    // Arm the competing writer for after this call's second session statement (the
    // `MAX(item_index)` read).
    instrumented.arm_after(2);

    store
        .save_list(Some("u"), "s", "l", &[text("a1"), text("a2")])
        .await;

    let conflicts = instrumented.conflict_count();
    let outside = instrumented.session_statement_count();
    let inside = instrumented.transaction_statement_count();
    instrumented.disarm();

    assert_eq!(
        outside, 0,
        "every statement of a save_list append must run inside the transaction"
    );
    assert!(
        inside >= 4,
        "both reads, the tail INSERT and the hash UPSERT must be inside the transaction, saw {inside}"
    );
    assert_eq!(
        conflicts, 0,
        "the append must not abort on a duplicate key — that would be a silently lost append"
    );
    let items = store.get_list(Some("u"), "s", "l").await;
    assert_eq!(
        items.iter().map(text_of).collect::<Vec<_>>(),
        vec!["a1".to_string(), "a2".to_string()],
        "the append is durable: nothing was dropped"
    );

    // A second append on top of the committed prefix still works incrementally.
    store
        .save_list(Some("u"), "s", "l", &[text("a1"), text("a2"), text("a3")])
        .await;
    let items = store.get_list(Some("u"), "s", "l").await;
    assert_eq!(
        items.iter().map(text_of).collect::<Vec<_>>(),
        vec!["a1".to_string(), "a2".to_string(), "a3".to_string()],
        "total entries = base + both appends, with nothing lost"
    );
}

// ---------------------------------------------------------------------------
//  JDBC-02: `UNVERSIONED` write-then-read is one unit of work
// ---------------------------------------------------------------------------

/// A `SqliteDialect` that offers **no** returning UPSERT — the shape MySQL (no `RETURNING`)
/// and H2/ANSI (no portable returning form) have. Everything else is delegated, so the store
/// runs the same SQLite SQL while `save_if_version(UNVERSIONED)` takes the UPSERT-then-
/// `SELECT version` fallback.
struct NoReturningSqliteDialect {
    inner: SqliteDialect,
}

impl NoReturningSqliteDialect {
    fn new() -> Self {
        Self {
            inner: SqliteDialect::new(),
        }
    }
}

impl TableNameConfigurable for NoReturningSqliteDialect {
    fn table_names(&self) -> &TableNames {
        self.inner.table_names()
    }
}

impl StoreDialect for NoReturningSqliteDialect {
    fn store_create_table_ddls(&self) -> Vec<String> {
        self.inner.store_create_table_ddls()
    }

    fn store_upsert(
        &self,
        namespace_path: &str,
        key: &str,
        json: &str,
        timestamp: i64,
    ) -> BoundSql {
        self.inner
            .store_upsert(namespace_path, key, json, timestamp)
    }
}

impl SessionStateDialect for NoReturningSqliteDialect {
    fn session_state_create_table_ddls(&self) -> Vec<String> {
        self.inner.session_state_create_table_ddls()
    }

    fn session_state_check_table_exists(&self, table_name: &str) -> BoundSql {
        self.inner.session_state_check_table_exists(table_name)
    }

    fn session_state_upsert(&self, slot_id: &str, key: &str, index: i32, json: &str) -> BoundSql {
        self.inner.session_state_upsert(slot_id, key, index, json)
    }

    // `session_state_upsert_returning_version` deliberately keeps the trait default (`None`):
    // that is what puts the store on the write-then-read fallback.
}

impl SnapshotDialect for NoReturningSqliteDialect {
    fn snapshot_create_table_ddls(&self) -> Vec<String> {
        self.inner.snapshot_create_table_ddls()
    }

    fn snapshot_upsert(&self, snapshot_id: &str, data: &[u8]) -> BoundSql {
        self.inner.snapshot_upsert(snapshot_id, data)
    }
}

impl SkillDialect for NoReturningSqliteDialect {
    fn skill_create_table_ddls(&self) -> Vec<String> {
        self.inner.skill_create_table_ddls()
    }
}

impl SkillResourcesDialect for NoReturningSqliteDialect {
    fn skill_resources_create_table_ddls(&self) -> Vec<String> {
        self.inner.skill_resources_create_table_ddls()
    }
}

impl AbstractJdbcDialect for NoReturningSqliteDialect {
    fn kind(&self) -> DialectKind {
        DialectKind::Sqlite
    }

    fn table_columns_query(&self, table_name: &str) -> BoundSql {
        self.inner.table_columns_query(table_name)
    }
}

#[tokio::test]
async fn unversioned_write_without_returning_reports_its_own_version() {
    // Java: JdbcAgentStateStore.saveIfVersion uses `executeInWriteTransaction`, so the UPSERT
    // and the `SELECT version` cannot be split by another writer. Without the transaction the
    // competitor injected right after the UPSERT bumps the row to version 2 and our follow-up
    // SELECT reports 2 — a version this call never wrote.
    let (store, instrumented) = new_instrumented_store_with(
        Arc::new(NoReturningSqliteDialect::new()),
        CompetingWrite::SingleKey,
    );
    instrumented.arm_after(1);

    let returned = store
        .save_if_version(Some("u"), "s", "k", text("mine"), UNVERSIONED)
        .unwrap();

    let outside = instrumented.session_statement_count();
    let inside = instrumented.transaction_statement_count();
    instrumented.disarm();

    assert_eq!(
        outside, 0,
        "the fallback UPSERT and its version read must both run inside the transaction"
    );
    assert_eq!(
        inside, 2,
        "no-RETURNING fallback = UPSERT + SELECT version, both inside one transaction"
    );
    assert_eq!(
        returned, 1,
        "must report the version this write produced, not a concurrent writer's"
    );
    assert_eq!(store.read_version(Some("u"), "s", "k").unwrap(), 1);
    assert_eq!(
        text_of(&store.get(Some("u"), "s", "k").await.unwrap()),
        "mine"
    );
}

// ---------------------------------------------------------------------------
//  JDBC-03: negative `expected_version` is rejected up front
// ---------------------------------------------------------------------------

#[tokio::test]
async fn save_if_version_negative_expected_is_rejected() {
    // Java: JdbcAgentStateStore.saveIfVersion throws IllegalArgumentException for a negative
    // expected version other than UNVERSIONED — a token that can never match a stored version,
    // so reporting it as a conflict would blame another writer for a caller bug.
    let store = new_store();
    let err = store
        .save_if_version(Some("u"), "s", "k", text("a"), -2)
        .expect_err("a negative expected version must be rejected, not reported as a conflict");
    assert!(err.to_string().contains("expectedVersion"), "{err}");
    let untouched = store.get_versioned(Some("u"), "s", "k").unwrap();
    assert!(
        !untouched.is_present(),
        "a rejected write must not touch the store"
    );

    // The core trait method is infallible, so it reports the rejection as UNVERSIONED — and
    // still writes nothing.
    assert_eq!(
        AgentStateStore::save_if_version(&store, Some("u"), "s", "k", text("a"), -2).await,
        UNVERSIONED
    );
    let after_trait_call = store.get_versioned(Some("u"), "s", "k").unwrap();
    assert!(!after_trait_call.is_present());
}

// ---------------------------------------------------------------------------
//  JDBC-12: the `try_*` channel reports failures instead of degrading
// ---------------------------------------------------------------------------

#[tokio::test]
async fn try_get_versioned_reports_a_corrupt_payload() {
    // Java: JdbcAgentStateStore.getVersioned deserializes inside `executeInWriteTransaction`
    // and a Jackson failure becomes a RuntimeException. The infallible core method can only
    // degrade, so `try_get_versioned` must be the channel that never turns a torn row into
    // "nothing stored" — a `version == 0` would let the next write silently overwrite it.
    let (store, executor) = new_store_with_executor();
    let dialect = SqliteDialect::new();
    executor
        .execute(&dialect.session_state_upsert("u:s", "k", 0, "{not json"))
        .expect("seeding a corrupt payload must succeed");

    let err = AgentStateStore::try_get_versioned(&store, Some("u"), "s", "k")
        .await
        .expect_err("a corrupt payload must surface as an error, not as an absent key");
    assert!(err.to_string().contains("deserialize state"), "{err}");
    assert!(
        matches!(err, agentscope_core::Error::Session(_)),
        "state-store failures travel on core's session error channel: {err:?}"
    );

    // The inherent fallible method agrees, and the infallible surface degrades deterministically.
    assert!(store.get_versioned(Some("u"), "s", "k").is_err());
    let degraded = AgentStateStore::get_versioned(&store, Some("u"), "s", "k").await;
    assert!(!degraded.is_present());
    assert_eq!(degraded.version, UNVERSIONED);
}

#[tokio::test]
async fn try_get_versioned_reports_a_row_without_a_readable_payload() {
    // `state_data` is `NOT NULL`, so a row whose payload is not text at all (here a BLOB
    // written by hand) is a torn row, not an absent key.
    let (store, executor) = new_store_with_executor();
    executor
        .execute(&BoundSql::no_params(
            "INSERT INTO agentscope_sessions (session_id, state_key, item_index, state_data, \
             version) VALUES ('u:s', 'k', 0, x'00', 1)",
        ))
        .expect("seeding a non-text payload must succeed");

    let err = AgentStateStore::try_get_versioned(&store, Some("u"), "s", "k")
        .await
        .expect_err("an unreadable payload must be an error");
    assert!(err.to_string().contains("no readable payload"), "{err}");
}

#[tokio::test]
async fn try_save_if_version_keeps_conflicts_and_failures_apart() {
    // `UNVERSIONED` must mean "another writer won"; a backend/validation failure must be an
    // `Err`. Collapsing the two (the infallible surface's behaviour) is what JDBC-12 is about.
    let store = new_store();
    assert_eq!(
        AgentStateStore::try_save_if_version(&store, Some("u"), "s", "k", text("a"), 0)
            .await
            .expect("create-if-absent on a fresh key must succeed"),
        1
    );
    assert_eq!(
        AgentStateStore::try_save_if_version(&store, Some("u"), "s", "k", text("b"), 0)
            .await
            .expect("a CAS conflict is not a failure"),
        UNVERSIONED,
        "a lost race is reported as a conflict, not as an error"
    );
    assert_eq!(
        AgentStateStore::try_save_if_version(&store, Some("u"), "s", "k", text("c"), 1)
            .await
            .unwrap(),
        2
    );

    let err = AgentStateStore::try_save_if_version(&store, Some("u"), "s", "k", text("d"), -3)
        .await
        .expect_err("an invalid expected version must be an error, not a conflict");
    assert!(err.to_string().contains("expectedVersion"), "{err}");

    assert_eq!(
        text_of(&store.get(Some("u"), "s", "k").await.unwrap()),
        "c",
        "the rejected write must not reach the store"
    );
    assert_eq!(store.read_version(Some("u"), "s", "k").unwrap(), 2);
}

// ---------------------------------------------------------------------------
//  JDBC-04: identifiers must fit the `VARCHAR(255)` columns
// ---------------------------------------------------------------------------

#[tokio::test]
async fn over_long_state_key_is_rejected_before_reaching_the_driver() {
    // Java: JdbcAgentStateStore rejects `key.length() > 255` with IllegalArgumentException.
    // Without the guard MySQL/PG report a driver error that the infallible trait methods
    // swallow as a warning — a silently lost write.
    let store = new_store();
    let key = "k".repeat(256);

    let err = store
        .save_if_version(Some("u"), "s", &key, text("a"), 0)
        .expect_err("an over-long key must be rejected");
    assert!(err.to_string().contains("too long"), "{err}");
    assert!(store.read_version(Some("u"), "s", &key).is_err());
    assert!(store.get_versioned(Some("u"), "s", &key).is_err());

    // The infallible entry points refuse it too, so no partial write reaches the backend.
    AgentStateStore::save(&store, Some("u"), "s", &key, text("a")).await;
    assert!(store.get(Some("u"), "s", &key).await.is_none());
    AgentStateStore::save_list(&store, Some("u"), "s", &key, &[text("a")]).await;
    assert!(store.get_list(Some("u"), "s", &key).await.is_empty());
}

#[tokio::test]
async fn state_key_at_the_column_limit_is_accepted() {
    let store = new_store();
    let key = "k".repeat(255);
    assert_eq!(
        store
            .save_if_version(Some("u"), "s", &key, text("a"), 0)
            .expect("255 characters is exactly what VARCHAR(255) holds"),
        1
    );
    assert_eq!(
        text_of(&store.get(Some("u"), "s", &key).await.unwrap()),
        "a"
    );
}

#[tokio::test]
async fn over_long_session_slot_is_rejected() {
    // The `session_id` column holds the `user:session` composite, so the limit applies to that
    // value (Java checks `slotId.length()`): 253 characters of session id + `u:` fit exactly.
    let store = new_store();
    let fits = "s".repeat(253);
    assert_eq!(
        store
            .save_if_version(Some("u"), &fits, "k", text("a"), 0)
            .expect("a slot of exactly 255 characters must be accepted"),
        1
    );

    let too_long = "s".repeat(254);
    let err = store
        .save_if_version(Some("u"), &too_long, "k", text("a"), 0)
        .expect_err("a slot over 255 characters must be rejected");
    assert!(err.to_string().contains("too long"), "{err}");
    assert!(
        !AgentStateStore::exists(&store, Some("u"), &too_long).await,
        "the infallible entry points must refuse it as well"
    );
}

#[tokio::test]
async fn length_limits_count_utf16_code_units() {
    // Java measures with `String.length()` (UTF-16 code units), so an emoji counts as 2: 127 of
    // them are 254 units and fit, 128 are 256 and do not (a `chars().count()` check would let
    // the latter through and hit the driver instead).
    let store = new_store();
    let fits = "😀".repeat(127);
    assert_eq!(
        store
            .save_if_version(Some("u"), "s", &fits, text("a"), 0)
            .expect("254 UTF-16 code units fit"),
        1
    );
    let too_long = "😀".repeat(128);
    assert!(
        store
            .save_if_version(Some("u"), "s", &too_long, text("a"), 0)
            .is_err(),
        "256 UTF-16 code units must be rejected"
    );
}

// ---------------------------------------------------------------------------
//  JDBC-12 (rest): every swallowing `AgentStateStore` method has a `try_*` twin
// ---------------------------------------------------------------------------

use std::sync::atomic::{AtomicBool, Ordering};

/// Executor that fails every statement once armed — the "backend unreachable" case the
/// infallible surface collapses into "nothing stored".
///
/// Reads and writes are armed separately so a test can fail a probe without also failing the
/// seed write. Statements inside a transaction never reach [`Self::execute`]/[`Self::query`]
/// (the inner backend owns the transaction), hence the explicit `in_transaction` guard.
#[derive(Debug)]
struct FailingExecutor {
    inner: Arc<RusqliteExecutor>,
    fail_reads: AtomicBool,
    fail_writes: AtomicBool,
}

impl FailingExecutor {
    fn new(inner: Arc<RusqliteExecutor>) -> Self {
        Self {
            inner,
            fail_reads: AtomicBool::new(false),
            fail_writes: AtomicBool::new(false),
        }
    }

    fn fail_reads(&self) {
        self.fail_reads.store(true, Ordering::SeqCst);
    }

    fn fail_writes(&self) {
        self.fail_writes.store(true, Ordering::SeqCst);
    }
}

impl JdbcExecutor for FailingExecutor {
    fn execute(&self, bound: &BoundSql) -> JdbcResult<usize> {
        if self.fail_writes.load(Ordering::SeqCst) {
            return Err(JdbcError::database("injected write failure"));
        }
        self.inner.execute(bound)
    }

    fn query(&self, bound: &BoundSql) -> JdbcResult<Vec<Vec<ColumnValue>>> {
        if self.fail_reads.load(Ordering::SeqCst) {
            return Err(JdbcError::database("injected read failure"));
        }
        self.inner.query(bound)
    }

    fn in_transaction(
        &self,
        work: &mut dyn FnMut(&mut dyn JdbcSession) -> JdbcResult<()>,
    ) -> JdbcResult<()> {
        if self.fail_writes.load(Ordering::SeqCst) {
            return Err(JdbcError::database("injected transaction failure"));
        }
        self.inner.in_transaction(work)
    }
}

fn new_failing_store() -> (JdbcAgentStateStore, Arc<FailingExecutor>) {
    let inner = Arc::new(RusqliteExecutor::in_memory().unwrap());
    let failing = Arc::new(FailingExecutor::new(inner));
    let executor: Arc<dyn JdbcExecutor> = failing.clone();
    let dialect: Arc<dyn AbstractJdbcDialect> = Arc::new(SqliteDialect::new());
    let store = JdbcAgentStateStore::with_schema(executor, dialect).unwrap();
    (store, failing)
}

#[tokio::test]
async fn try_star_agrees_with_the_infallible_surface_on_the_happy_path() {
    // The `try_*` overrides share their implementation with the infallible methods, so a
    // caller switching to the fallible channel must observe exactly the same data.
    let store = new_store();
    AgentStateStore::try_save(&store, Some("u"), "s", "k", text("a"))
        .await
        .expect("a healthy write must succeed");
    assert_eq!(
        AgentStateStore::try_get(&store, Some("u"), "s", "k")
            .await
            .unwrap()
            .map(|v| text_of(&v)),
        AgentStateStore::get(&store, Some("u"), "s", "k")
            .await
            .map(|v| text_of(&v)),
    );
    assert_eq!(
        store.get(Some("u"), "s", "k").await.map(|v| text_of(&v)),
        Some("a".to_string())
    );

    AgentStateStore::try_save_list(&store, Some("u"), "s", "l", &[text("x"), text("y")])
        .await
        .expect("a healthy list write must succeed");
    let via_try = AgentStateStore::try_get_list(&store, Some("u"), "s", "l")
        .await
        .unwrap();
    assert_eq!(
        via_try,
        AgentStateStore::get_list(&store, Some("u"), "s", "l").await
    );
    assert_eq!(via_try.len(), 2);

    assert!(AgentStateStore::try_exists(&store, Some("u"), "s")
        .await
        .unwrap());
    assert_eq!(
        AgentStateStore::try_list_session_ids(&store, Some("u"))
            .await
            .unwrap(),
        AgentStateStore::list_session_ids(&store, Some("u")).await
    );

    // An absent key/list is not a failure.
    assert!(AgentStateStore::try_get(&store, Some("u"), "s", "missing")
        .await
        .unwrap()
        .is_none());
    assert!(
        AgentStateStore::try_get_list(&store, Some("u"), "s", "missing")
            .await
            .unwrap()
            .is_empty()
    );

    AgentStateStore::try_delete(&store, Some("u"), "s")
        .await
        .expect("deleting an existing session must succeed");
    assert!(!AgentStateStore::exists(&store, Some("u"), "s").await);
    // Deleting an already-absent session is a no-op, not an error.
    AgentStateStore::try_delete(&store, Some("u"), "s")
        .await
        .expect("deleting an absent session must not fail");
}

#[tokio::test]
async fn try_save_reports_a_backend_failure_and_an_invalid_id() {
    let (store, failing) = new_failing_store();
    failing.fail_writes();

    let err = AgentStateStore::try_save(&store, Some("u"), "s", "k", text("a"))
        .await
        .expect_err("a failed write must not be reported as success");
    assert!(err.to_string().contains("injected write failure"), "{err}");
    assert!(matches!(err, agentscope_core::Error::Session(_)), "{err:?}");

    let err = AgentStateStore::try_save_list(&store, Some("u"), "s", "l", &[text("a")])
        .await
        .expect_err("a rolled-back append must not be reported as success");
    assert!(
        err.to_string().contains("injected transaction failure"),
        "{err}"
    );

    // Identifier validation is part of the same fallible channel; the infallible method keeps
    // silently refusing (its `()` signature cannot say why).
    let healthy = new_store();
    assert!(
        AgentStateStore::try_save(&healthy, Some("u"), "  ", "k", text("a"))
            .await
            .is_err()
    );
    assert!(
        AgentStateStore::try_save(&healthy, Some("u"), "s", &"k".repeat(256), text("a"))
            .await
            .is_err()
    );
    assert!(AgentStateStore::get(&healthy, Some("u"), "s", "k")
        .await
        .is_none());
}

#[tokio::test]
async fn try_get_reports_a_corrupt_payload_instead_of_an_absent_key() {
    // Same ruling as `try_get_versioned`: a row exists, so its unreadable payload is a torn
    // row — reporting `Ok(None)` would let the next write overwrite it unnoticed.
    let (store, executor) = new_store_with_executor();
    let dialect = SqliteDialect::new();
    executor
        .execute(&dialect.session_state_upsert("u:s", "k", 0, "{not json"))
        .expect("seeding a corrupt payload must succeed");
    executor
        .execute(&BoundSql::no_params(
            "INSERT INTO agentscope_sessions (session_id, state_key, item_index, state_data, \
             version) VALUES ('u:s', 'blob', 0, x'00', 1)",
        ))
        .expect("seeding a non-text payload must succeed");

    let err = AgentStateStore::try_get(&store, Some("u"), "s", "k")
        .await
        .expect_err("a corrupt payload must be an error");
    assert!(err.to_string().contains("deserialize state"), "{err}");
    assert!(matches!(err, agentscope_core::Error::Session(_)), "{err:?}");

    let err = AgentStateStore::try_get(&store, Some("u"), "s", "blob")
        .await
        .expect_err("an unreadable payload must be an error");
    assert!(err.to_string().contains("no readable payload"), "{err}");
}

#[tokio::test]
async fn try_get_list_reports_a_corrupt_item() {
    let (store, executor) = new_store_with_executor();
    let dialect = SqliteDialect::new();
    for (index, payload) in [(0, text("ok")), (1, text("ok"))].into_iter() {
        executor
            .execute(&dialect.session_state_insert(
                "u:s",
                "l",
                index,
                &serde_json::to_string(&payload).unwrap(),
            ))
            .expect("seeding a list item must succeed");
    }
    executor
        .execute(&dialect.session_state_insert("u:s", "bad", 0, "{not json"))
        .expect("seeding a corrupt item must succeed");

    assert_eq!(
        AgentStateStore::try_get_list(&store, Some("u"), "s", "l")
            .await
            .expect("a healthy list must load")
            .len(),
        2
    );
    let err = AgentStateStore::try_get_list(&store, Some("u"), "s", "bad")
        .await
        .expect_err("a corrupt item must not be dropped silently");
    assert!(err.to_string().contains("deserialize list item 0"), "{err}");
}

#[tokio::test]
async fn try_exists_reports_a_backend_failure_instead_of_false() {
    let (store, failing) = new_failing_store();
    AgentStateStore::try_save(&store, Some("u"), "s", "k", text("a"))
        .await
        .expect("the seed write must succeed");
    assert!(AgentStateStore::try_exists(&store, Some("u"), "s")
        .await
        .unwrap());

    failing.fail_reads();
    let err = AgentStateStore::try_exists(&store, Some("u"), "s")
        .await
        .expect_err("an unreachable backend must not read as 'no state'");
    assert!(err.to_string().contains("injected read failure"), "{err}");
    // The infallible surface still degrades, which is exactly why the `try_*` twin exists.
    assert!(!AgentStateStore::exists(&store, Some("u"), "s").await);

    let err = AgentStateStore::try_list_session_ids(&store, Some("u"))
        .await
        .expect_err("an unreachable backend must not read as 'no sessions'");
    assert!(err.to_string().contains("injected read failure"), "{err}");
    assert!(AgentStateStore::list_session_ids(&store, Some("u"))
        .await
        .is_empty());
}

#[tokio::test]
async fn try_delete_reports_a_backend_failure() {
    let (store, failing) = new_failing_store();
    AgentStateStore::try_save(&store, Some("u"), "s", "k", text("a"))
        .await
        .expect("the seed write must succeed");

    failing.fail_writes();
    let err = AgentStateStore::try_delete(&store, Some("u"), "s")
        .await
        .expect_err("a failed delete must not report success");
    assert!(err.to_string().contains("injected write failure"), "{err}");
    assert!(matches!(err, agentscope_core::Error::Session(_)), "{err:?}");

    assert!(
        AgentStateStore::try_get(&store, Some("u"), "s", "k")
            .await
            .unwrap()
            .is_some(),
        "the failed delete must have left the row in place"
    );
}

// ---------------------------------------------------------------------------
//  Constraint classification — JDBC-05
// ---------------------------------------------------------------------------

#[test]
fn sqlite_constraint_classes_are_told_apart_by_their_codes() {
    // SQLite words *every* constraint failure as "constraint failed", so the message alone
    // cannot tell a duplicate key from a NOT NULL / CHECK / foreign-key violation. The
    // extended result code can, and that is what `is_unique_violation` keys on.
    let executor = RusqliteExecutor::in_memory().unwrap();
    executor
        .execute(&BoundSql::no_params(
            "CREATE TABLE probe (id INTEGER PRIMARY KEY, name TEXT NOT NULL)",
        ))
        .unwrap();
    executor
        .execute(&BoundSql::no_params(
            "INSERT INTO probe (id, name) VALUES (1, 'a')",
        ))
        .unwrap();

    let duplicate = executor
        .execute(&BoundSql::no_params(
            "INSERT INTO probe (id, name) VALUES (1, 'b')",
        ))
        .unwrap_err();
    assert!(is_unique_violation(&duplicate), "{duplicate}");

    let not_null = executor
        .execute(&BoundSql::no_params(
            "INSERT INTO probe (id, name) VALUES (2, NULL)",
        ))
        .unwrap_err();
    assert!(
        !is_unique_violation(&not_null),
        "a NOT NULL violation must not be mistaken for a duplicate key: {not_null}"
    );

    // The verdict comes from the structured code, not from the text.
    match not_null {
        JdbcError::Database(db) => {
            assert_eq!(Some(1299), db.vendor_code(), "SQLITE_CONSTRAINT_NOTNULL");
            assert_eq!(None, db.sql_state(), "SQLite has no SQLSTATE");
        }
        other => panic!("expected a database error, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
//  Multiple stores over one database file — Java: JdbcUnconditionalVersionTest
// ---------------------------------------------------------------------------

/// A unique path for a file-backed database, so each test owns its own file.
///
/// File-backed rather than `:memory:` because these cases need several *connections*:
/// the store's own connection serializes nothing across instances.
fn temp_db_path(label: &str) -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir()
        .join(format!(
            "agentscope-jdbc-{label}-{}-{seq}-{nanos}.sqlite3",
            std::process::id()
        ))
        .to_string_lossy()
        .into_owned()
}

/// Opens another store over the same database file — a second connection, i.e. the
/// multi-instance shape of the Java concurrency tests (one pool connection per writer).
fn open_store(path: &str, dialect: &Arc<dyn AbstractJdbcDialect>) -> Arc<JdbcAgentStateStore> {
    let executor = Arc::new(RusqliteExecutor::open(path).expect("open the database file"));
    Arc::new(
        JdbcAgentStateStore::new(executor, dialect.clone(), false).expect("schema already exists"),
    )
}

#[test]
fn concurrent_writers_get_unique_contiguous_versions() {
    // Java: JdbcUnconditionalVersionTest.concurrentStoreInstancesReturnUniqueContiguousVersions
    // (4 writers × 25 rounds). Every write is unconditional, so the store — not the caller —
    // hands out the version and no two writers may ever see the same one.
    const WRITERS: usize = 4;
    const ROUNDS: usize = 25;
    let path = temp_db_path("versions");
    let dialect: Arc<dyn AbstractJdbcDialect> = Arc::new(SqliteDialect::new());
    {
        let executor = Arc::new(RusqliteExecutor::open(&path).expect("open the database file"));
        JdbcAgentStateStore::with_schema(executor, dialect.clone()).expect("schema");
    }

    let mut handles = Vec::with_capacity(WRITERS);
    for writer in 0..WRITERS {
        let path = path.clone();
        let dialect = dialect.clone();
        handles.push(std::thread::spawn(move || {
            let store = open_store(&path, &dialect);
            (0..ROUNDS)
                .map(|round| {
                    store
                        .save_if_version(
                            None,
                            "s",
                            "counter",
                            serde_json::json!({ "writer": writer, "round": round }),
                            UNVERSIONED,
                        )
                        .expect("an unconditional write must succeed")
                })
                .collect::<Vec<i64>>()
        }));
    }
    let mut versions: Vec<i64> = handles
        .into_iter()
        .flat_map(|handle| handle.join().expect("writer thread"))
        .collect();
    versions.sort_unstable();
    assert_eq!(
        (1..=(WRITERS * ROUNDS) as i64).collect::<Vec<_>>(),
        versions,
        "every version must be handed out exactly once, without gaps"
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn concurrent_writes_to_distinct_keys_all_land() {
    // The companion to the version test: four connections writing side by side must neither
    // fail with `SQLITE_BUSY` nor lose a row (the busy timeout is what makes the loser wait
    // for the winner's commit instead of erroring out).
    const WRITERS: usize = 4;
    const ROUNDS: usize = 25;
    let path = temp_db_path("distinct");
    let dialect: Arc<dyn AbstractJdbcDialect> = Arc::new(SqliteDialect::new());
    {
        let executor = Arc::new(RusqliteExecutor::open(&path).expect("open the database file"));
        JdbcAgentStateStore::with_schema(executor, dialect.clone()).expect("schema");
    }

    let mut handles = Vec::with_capacity(WRITERS);
    for writer in 0..WRITERS {
        let path = path.clone();
        let dialect = dialect.clone();
        handles.push(std::thread::spawn(move || {
            let store = open_store(&path, &dialect);
            for round in 0..ROUNDS {
                let key = format!("k-{writer}-{round}");
                let version = store
                    .save_if_version(
                        None,
                        "s",
                        &key,
                        serde_json::json!({ "writer": writer, "round": round }),
                        UNVERSIONED,
                    )
                    .expect("a write to a private key must succeed");
                assert_eq!(1, version, "a fresh key starts at version 1");
            }
        }));
    }
    for handle in handles {
        handle.join().expect("writer thread");
    }

    let store = open_store(&path, &dialect);
    for writer in 0..WRITERS {
        for round in 0..ROUNDS {
            let key = format!("k-{writer}-{round}");
            let state = store.get_versioned(None, "s", &key).expect("read back");
            assert_eq!(1, state.version, "{key}");
            assert_eq!(
                Some(&serde_json::json!({ "writer": writer, "round": round })),
                state.value.as_ref(),
                "{key} must hold exactly what its writer stored"
            );
        }
    }
    std::fs::remove_file(&path).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_appends_never_collide_on_the_item_index() {
    // JDBC-01 follow-up under real concurrency: the list read-modify-write runs in one
    // IMMEDIATE transaction per store, so two stores cannot compute the same tail index. Each
    // writer submits `[seed, own]`: whoever loses the race sees the winner's committed items,
    // so the append neither errors (an index collision would roll the whole batch back) nor
    // leaves a half-written list.
    const WRITERS: usize = 4;
    let path = temp_db_path("appends");
    let dialect: Arc<dyn AbstractJdbcDialect> = Arc::new(SqliteDialect::new());
    {
        let executor = Arc::new(RusqliteExecutor::open(&path).expect("open the database file"));
        JdbcAgentStateStore::with_schema(executor, dialect.clone()).expect("schema");
    }

    let mut tasks = Vec::with_capacity(WRITERS);
    for writer in 0..WRITERS {
        let path = path.clone();
        let dialect = dialect.clone();
        tasks.push(tokio::spawn(async move {
            let store = open_store(&path, &dialect);
            let values = vec![text("seed"), text(&format!("writer-{writer}"))];
            store.try_save_list(None, "s", "log", &values).await
        }));
    }
    for task in tasks {
        task.await
            .expect("append task")
            .expect("a concurrent append must not fail");
    }

    let store = open_store(&path, &dialect);
    let items = store.get_list(None, "s", "log").await;
    assert_eq!(
        2,
        items.len(),
        "exactly the seed plus one winner: {:?}",
        items.iter().map(text_of).collect::<Vec<_>>()
    );
    assert_eq!("seed", text_of(&items[0]));
    std::fs::remove_file(&path).ok();
}
