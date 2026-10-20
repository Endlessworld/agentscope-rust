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

use agentscope_extensions_jdbc::dialect::vendor::SqliteDialect;
use agentscope_extensions_jdbc::dialect::{AbstractJdbcDialect, SessionStateDialect};
use agentscope_extensions_jdbc::{
    AgentStateStore, JdbcAgentStateStore, JdbcExecutor, RusqliteExecutor, UNVERSIONED,
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

/// Wraps a real executor to observe how many statements a call issues, and to let a test
/// inject a *competing writer* at a precise point (GAP-01).
///
/// Interposition happens **after** the triggering statement has completed but before the
/// caller regains control. That is exactly the window a non-atomic
/// "UPSERT then SELECT version" implementation exposes: the write lands, another writer
/// commits, and the follow-up read reports the other writer's version. For a
/// `RETURNING`-based implementation the row is already produced by the single statement, so
/// interposition cannot corrupt what the caller sees.
#[derive(Debug)]
struct InstrumentedExecutor {
    inner: Arc<RusqliteExecutor>,
    /// Statements touching the session-state table, in order.
    session_statements: std::sync::atomic::AtomicUsize,
    /// Fire the competing write right after the Nth session statement
    /// (`usize::MAX` = disarmed). Starts disarmed so schema creation is not counted.
    interpose_after: std::sync::atomic::AtomicUsize,
    interposed: std::sync::atomic::AtomicBool,
}

impl InstrumentedExecutor {
    fn new(inner: Arc<RusqliteExecutor>) -> Self {
        Self {
            inner,
            session_statements: std::sync::atomic::AtomicUsize::new(0),
            interpose_after: std::sync::atomic::AtomicUsize::new(usize::MAX),
            interposed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Starts counting from zero and arms the competing write for after statement `n`.
    fn arm_after(&self, n: usize) {
        self.session_statements
            .store(0, std::sync::atomic::Ordering::SeqCst);
        self.interposed
            .store(false, std::sync::atomic::Ordering::SeqCst);
        self.interpose_after
            .store(n, std::sync::atomic::Ordering::SeqCst);
    }

    fn session_statement_count(&self) -> usize {
        self.session_statements
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    fn was_interposed(&self) -> bool {
        self.interposed.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// A competing `save_if_version(UNVERSIONED)`-style overwrite of the same key, run as a
    /// separate UPSERT (it bumps `version` and replaces the payload).
    fn interpose_competing_write(&self) {
        let bound = SqliteDialect::new().session_state_upsert(
            "u:s",
            "k",
            0,
            &serde_json::to_string(&text("competitor")).unwrap(),
        );
        self.inner
            .execute(&bound)
            .expect("competing write must succeed");
    }

    fn observe(&self, sql: &str) {
        if !sql.to_ascii_uppercase().contains("AGENTSCOPE_SESSIONS") {
            return;
        }
        let n = self
            .session_statements
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        if self.interpose_after.load(std::sync::atomic::Ordering::SeqCst) == n
            && !self
                .interposed
                .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            self.interpose_competing_write();
        }
    }
}

impl JdbcExecutor for InstrumentedExecutor {
    fn execute(
        &self,
        bound: &agentscope_extensions_jdbc::dialect::BoundSql,
    ) -> agentscope_extensions_jdbc::JdbcResult<usize> {
        let affected = self.inner.execute(bound)?;
        self.observe(&bound.sql);
        Ok(affected)
    }

    fn query(
        &self,
        bound: &agentscope_extensions_jdbc::dialect::BoundSql,
    ) -> agentscope_extensions_jdbc::JdbcResult<Vec<Vec<agentscope_extensions_jdbc::ColumnValue>>> {
        let rows = self.inner.query(bound)?;
        self.observe(&bound.sql);
        Ok(rows)
    }
}

fn new_instrumented_store() -> (JdbcAgentStateStore, Arc<InstrumentedExecutor>) {
    let inner = Arc::new(RusqliteExecutor::in_memory().unwrap());
    let instrumented = Arc::new(InstrumentedExecutor::new(inner));
    let executor: Arc<dyn JdbcExecutor> = instrumented.clone();
    let dialect: Arc<dyn AbstractJdbcDialect> = Arc::new(SqliteDialect::new());
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
    assert!(store.get(Some("user1"), "session1", "missing").await.is_none());
}

#[tokio::test]
async fn save_overwrites_single() {
    // Java: JdbcAgentStateStoreH2Test.saveOverwritesSingle
    let store = new_store();
    store.save(Some("user1"), "s1", "k", text("v1")).await;
    store.save(Some("user1"), "s1", "k", text("v2")).await;
    assert_eq!(
        store.get(Some("user1"), "s1", "k").await.map(|v| text_of(&v)),
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
    assert_eq!(hash, agentscope_core::state::list_hash::compute_hash(&[text("a"), text("b")]));

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
        store.get(Some("user1"), "s1", "k").await.map(|v| text_of(&v)),
        Some("one".to_string())
    );
    assert_eq!(
        store.get(Some("user2"), "s1", "k").await.map(|v| text_of(&v)),
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
        .save_list(Some("u"), "s", "l", &[text("a"), text("b"), text("c"), text("d")])
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
    assert_ne!(returned, UNVERSIONED, "unconditional overwrite always succeeds");
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
    assert!(!store.get_versioned(Some("bob"), "s", "k").unwrap().is_present());
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
    assert!(second > first && third > second, "versions must increase monotonically");
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
    assert_eq!(raw_version(&executor, "u:s", "k"), 0, "fixture must be version 0");

    assert_eq!(
        store
            .save_if_version(Some("u"), "s", "k", text("fresh"), 0)
            .unwrap(),
        1,
        "a version-0 row must be accepted as create-if-absent, not reported as a conflict"
    );

    assert_eq!(text_of(&store.get(Some("u"), "s", "k").await.unwrap()), "fresh");
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
