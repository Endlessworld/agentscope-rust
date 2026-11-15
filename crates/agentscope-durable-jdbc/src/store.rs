//! PostgreSQL as the durable runtime's `RecordStore`.
//!
//! # What a scope is
//!
//! One row in `agentscope_scope`, one row per record in `agentscope_part`. A commit writes the
//! records it changed, never the conversation: on the file store a turn rewrites a document that
//! grows with the history, and the cost of the tenth turn is the size of the first nine. Here the
//! cost of a commit is the number of rows it touches.
//!
//! # What a transaction is
//!
//! `BEGIN`, `SELECT revision … FOR UPDATE`, the writes, then
//! `UPDATE … SET revision = revision + 1 WHERE revision = <read>`, `COMMIT`. Two hosts writing the
//! same scope are serialised by the row lock — the same exclusion a scope lock file gives on one
//! disk — and the revision predicate is the assertion that the read set was still current when
//! the writes landed. A miss rolls the transaction back and answers `CONFLICT`; it never applies a
//! stale read on top of a newer one.
//!
//! # What is deliberately not here
//!
//! `scope_bytes` answers `None`. Measuring a scope means aggregating its rows on every commit, and
//! the compaction decision it feeds can be made out of band with one query (see
//! `docs/DURABLE_JDBC.md`). A store that guessed a size would either pay for the aggregate on
//! every commit or compact on a number it invented.

use std::cell::{Cell, RefCell};

use agentscope_durable::store::{RecordStore, Txn, WorkItem};
use agentscope_durable::{DurableError, Result};
use serde_json::Value;

use crate::mapping::{self, Param};
use crate::schema;
use crate::session::{SqlSession, SqlTransaction};
use crate::statements as sql;

/// A `RecordStore` over any [`SqlSession`].
pub struct PostgresStore<S: SqlSession> {
    session: S,
}

impl<S: SqlSession> PostgresStore<S> {
    pub fn new(session: S) -> Self {
        Self { session }
    }

    /// The session this store issues its statements on.
    pub fn session(&self) -> &S {
        &self.session
    }

    /// Applies the schema. Safe to call on every connect: every statement is idempotent.
    pub fn ensure_schema(&self) -> Result<()> {
        for statement in schema::statements() {
            self.session.query(&statement, &[])?;
        }
        Ok(())
    }
}

impl<S: SqlSession> RecordStore for PostgresStore<S> {
    fn transaction(
        &self,
        scope: &str,
        work: &mut dyn FnMut(&mut dyn Txn) -> Result<()>,
    ) -> Result<()> {
        let scope_key = mapping::scope_key(scope)?.to_string();
        self.session
            .transaction(&mut |txn: &mut dyn SqlTransaction| {
                let now = txn.now_millis()?;
                let revision = lock_scope(txn, &scope_key, now)?;
                let written = {
                    // The reborrow is what lets the transaction be used for the compare-and-set
                    // below: the records handle borrows the transaction, it does not take it.
                    let mut records = ScopeTxn {
                        txn: RefCell::new(&mut *txn),
                        scope: &scope_key,
                        now,
                        written: Cell::new(false),
                    };
                    work(&mut records)?;
                    records.written.get()
                };
                if !written {
                    // A read costs no write. The revision is what a commit is checked against, and
                    // a transaction that changed nothing has nothing to check: bumping it would
                    // turn every control poll into a scope update and invalidate every concurrent
                    // reader's revision for no reason.
                    return Ok(());
                }
                let bump = mapping::cas_bump(&scope_key, revision, now);
                let updated = txn.execute(bump.sql, &bump.params)?;
                mapping::verify_cas(updated)
            })
    }

    /// Due entries of one queue, in readiness order, from the queue's own index.
    fn ready(&self, queue: &str, now: i64, limit: usize) -> Result<Vec<WorkItem>> {
        let rows = self.session.query(
            sql::READY,
            &[
                Param::Text(queue.to_string()),
                Param::BigInt(now),
                Param::BigInt(mapping::page_limit(limit)),
            ],
        )?;
        let mut due = Vec::with_capacity(rows.len());
        for row in &rows {
            due.push(WorkItem {
                queue: queue.to_string(),
                work_id: row.text(0)?.to_string(),
                ready_at: row.int(1)?,
                payload: row.json_opt(2)?,
            });
        }
        Ok(due)
    }

    /// A few records in one statement, without taking the scope row lock.
    ///
    /// A control poll happens several times a second while a model call streams and needs one
    /// record. Sending it through `transaction` would queue it behind the writer and hold the
    /// lock the writer needs; the batch read is one indexed statement and no lock at all.
    fn read_parts(&self, scope: &str, parts: &[&str]) -> Result<Vec<Option<Value>>> {
        if parts.is_empty() {
            return Ok(Vec::new());
        }
        let scope_key = mapping::scope_key(scope)?;
        let keys = parts
            .iter()
            .map(|part| mapping::part_key(part).map(str::to_string))
            .collect::<Result<Vec<String>>>()?;
        let rows = self.session.query(
            sql::SELECT_PARTS,
            &[Param::Text(scope_key.to_string()), Param::TextArray(keys)],
        )?;
        let mut values: Vec<Option<Value>> = vec![None; parts.len()];
        for row in &rows {
            let key = row.text(0)?;
            if let Some(index) = parts.iter().position(|part| *part == key) {
                values[index] = Some(row.json(1)?);
            }
        }
        Ok(values)
    }

    fn shared_across_hosts(&self) -> bool {
        true
    }
}

/// Takes the scope row lock and answers the revision the writes will be checked against.
///
/// The first write to a scope creates its row; the second `SELECT … FOR UPDATE` is what makes the
/// creator and a concurrent creator agree on one row. The extra statement happens once per scope,
/// not once per transaction, so the steady-state commit is one read, its writes and one update.
fn lock_scope(txn: &mut dyn SqlTransaction, scope: &str, now: i64) -> Result<i64> {
    let params = [Param::Text(scope.to_string())];
    let rows = txn.query(sql::LOCK_SCOPE, &params)?;
    if let Some(row) = rows.first() {
        return row.int(0);
    }
    txn.execute(
        sql::ENSURE_SCOPE,
        &[Param::Text(scope.to_string()), Param::BigInt(now)],
    )?;
    let rows = txn.query(sql::LOCK_SCOPE, &params)?;
    rows.first()
        .ok_or_else(|| {
            DurableError::unavailable("the scope row vanished while its lock was being taken")
        })
        .and_then(|row| row.int(0))
}

/// One transaction's [`Txn`].
///
/// Writes go to the database immediately rather than being staged here: the transaction is what
/// rolls them back, and a staged copy would be a second representation of every value in memory —
/// which is exactly what a journal flush of tens of megabytes must not build.
///
/// The handle sits behind a `RefCell` because `Txn` reads through `&self` while a SQL client
/// mutates through `&mut self`.
struct ScopeTxn<'a> {
    txn: RefCell<&'a mut dyn SqlTransaction>,
    scope: &'a str,
    now: i64,
    /// Whether this transaction changed anything, which is what decides whether the revision is
    /// bumped at commit time. A read never is.
    written: Cell<bool>,
}

impl Txn for ScopeTxn<'_> {
    fn now(&self) -> i64 {
        self.now
    }

    fn get(&self, part: &str) -> Result<Option<Value>> {
        let key = mapping::part_key(part)?;
        let rows = self.txn.borrow_mut().query(
            sql::SELECT_PART,
            &[
                Param::Text(self.scope.to_string()),
                Param::Text(key.to_string()),
            ],
        )?;
        match rows.first() {
            Some(row) => Ok(Some(row.json(0)?)),
            None => Ok(None),
        }
    }

    fn put(&mut self, part: &str, value: Option<Value>) -> Result<()> {
        let key = mapping::part_key(part)?;
        self.written.set(true);
        match value {
            Some(value) => {
                // Encoded here, not by the driver: the NUL check has to answer before a statement
                // is sent, and a value is serialised exactly once.
                let body = mapping::encode(&value)?;
                self.txn.borrow_mut().execute(
                    sql::UPSERT_PART,
                    &[
                        Param::Text(self.scope.to_string()),
                        Param::Text(key.to_string()),
                        Param::Text(body),
                    ],
                )?;
            }
            None => {
                self.txn.borrow_mut().execute(
                    sql::DELETE_PART,
                    &[
                        Param::Text(self.scope.to_string()),
                        Param::Text(key.to_string()),
                    ],
                )?;
            }
        }
        Ok(())
    }

    fn keys(&self, prefix: &str, after: Option<&str>, limit: usize) -> Result<Vec<String>> {
        let page = mapping::keys_page(self.scope, prefix, after, limit)?;
        let rows = self.txn.borrow_mut().query(page.sql, &page.params)?;
        rows.iter()
            .map(|row| row.text(0).map(str::to_string))
            .collect()
    }

    fn schedule(
        &mut self,
        queue: &str,
        work_id: &str,
        ready_at: i64,
        payload: Option<Value>,
    ) -> Result<()> {
        self.written.set(true);
        if ready_at < 0 {
            self.txn.borrow_mut().execute(
                sql::DELETE_WORK,
                &[
                    Param::Text(queue.to_string()),
                    Param::Text(work_id.to_string()),
                ],
            )?;
            return Ok(());
        }
        let payload = match payload {
            Some(payload) => Param::Text(mapping::encode(&payload)?),
            None => Param::Null,
        };
        self.txn.borrow_mut().execute(
            sql::UPSERT_WORK,
            &[
                Param::Text(queue.to_string()),
                Param::Text(work_id.to_string()),
                Param::BigInt(ready_at),
                payload,
            ],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use agentscope_durable::backend::TxnBackend;
    use agentscope_durable::store::memory::InMemoryStore;
    use agentscope_durable::store::{txn, RecordStore};
    use agentscope_durable::types::*;
    use agentscope_durable::SessionBackend;
    use serde_json::json;

    use super::*;
    use crate::testing::MemoryPg;

    fn store() -> PostgresStore<MemoryPg> {
        PostgresStore::new(MemoryPg::new())
    }

    fn write(store: &dyn RecordStore, scope: &str, work: impl FnOnce(&mut dyn Txn) -> Result<()>) {
        txn(store, scope, work).expect("transaction");
    }

    #[test]
    fn a_record_round_trips_through_its_row() {
        let store = store();
        write(&store, "s:1", |txn| {
            txn.put("turn:t1", Some(json!({ "status": "RUNNING" })))
        });
        assert_eq!(
            txn(&store, "s:1", |txn| txn.get("turn:t1")).expect("read"),
            Some(json!({ "status": "RUNNING" }))
        );
        // A separate transaction sees the row: the write is the database's, not a staged copy.
        write(&store, "s:1", |txn| txn.put("turn:t1", None));
        assert_eq!(
            txn(&store, "s:1", |txn| txn.get("turn:t1")).expect("read"),
            None
        );
        assert_eq!(store.session().revision("s:1"), Some(2));
    }

    #[test]
    fn a_failed_transaction_leaves_no_trace() {
        let store = store();
        write(&store, "s:1", |txn| txn.put("a", Some(json!(1))));
        let error = txn(&store, "s:1", |txn| {
            txn.put("a", Some(json!(2)))?;
            txn.put("b", Some(json!(3)))?;
            Err::<(), _>(DurableError::invalid("boom"))
        })
        .expect_err("the closure failed");
        assert_eq!(error.code(), crate::ErrorCode::Invalid);
        assert_eq!(
            txn(&store, "s:1", |txn| txn.get("a")).expect("read"),
            Some(json!(1)),
            "the failed transaction's write is gone"
        );
        assert_eq!(txn(&store, "s:1", |txn| txn.get("b")).expect("read"), None);
        assert_eq!(
            store.session().revision("s:1"),
            Some(1),
            "a rolled back transaction does not advance the revision"
        );
        assert!(store
            .session()
            .statements()
            .contains(&"ROLLBACK".to_string()));
    }

    #[test]
    fn keys_honour_prefix_after_and_limit_like_the_in_memory_store() {
        let postgres = store();
        let memory = InMemoryStore::new();
        let keys = [
            "journal:0",
            "journal:1",
            "journal:2",
            "turn:1",
            "turn:2",
            "event:9",
        ];
        for store in [&postgres as &dyn RecordStore, &memory as &dyn RecordStore] {
            for key in keys {
                write(store, "s:1", |txn| txn.put(key, Some(json!(key))));
            }
        }
        let pages: [(&str, Option<&str>, usize); 5] = [
            ("journal:", None, 2),
            ("journal:", Some("journal:0"), 2),
            ("journal:", Some("a"), 2),
            ("", Some("journal:1"), 10),
            ("turn:", Some("t"), 10),
        ];
        for (prefix, after, limit) in pages {
            let from_postgres =
                txn(&postgres, "s:1", |txn| txn.keys(prefix, after, limit)).expect("keys");
            let from_memory =
                txn(&memory, "s:1", |txn| txn.keys(prefix, after, limit)).expect("keys");
            assert_eq!(
                from_postgres, from_memory,
                "prefix {prefix:?}, after {after:?}, limit {limit}"
            );
        }
    }

    #[test]
    fn keys_merge_writes_made_earlier_in_the_same_transaction() {
        let store = store();
        write(&store, "s:1", |txn| txn.put("journal:0", Some(json!(0))));
        let seen = txn(&store, "s:1", |txn| {
            txn.put("journal:1", Some(json!(1)))?;
            txn.put("journal:0", None)?;
            txn.keys("journal:", None, 10)
        })
        .expect("keys");
        assert_eq!(seen, vec!["journal:1".to_string()]);
        // And the deletion is real, because it was part of the same committed transaction.
        assert_eq!(
            txn(&store, "s:1", |txn| txn.get("journal:0")).expect("read"),
            None
        );
    }

    #[test]
    fn a_batch_read_is_one_statement_and_takes_no_scope_lock() {
        let store = store();
        write(&store, "s:1", |txn| {
            txn.put("control", Some(json!({ "status": "RUNNING" })))?;
            txn.put("turn:t1", Some(json!({ "revision": 3 })))
        });
        store.session().clear_statements();
        let values = store
            .read_parts("s:1", &["control", "turn:t1", "missing"])
            .expect("read parts");
        assert_eq!(
            values,
            vec![
                Some(json!({ "status": "RUNNING" })),
                Some(json!({ "revision": 3 })),
                None
            ]
        );
        assert_eq!(
            store.session().statements(),
            vec![sql::SELECT_PARTS.to_string()],
            "a control poll must not open a transaction"
        );
    }

    #[test]
    fn ready_answers_due_entries_in_readiness_order_within_the_page() {
        let store = store();
        write(&store, "s:1", |txn| {
            txn.schedule("turns", "a", 10, None)?;
            txn.schedule("turns", "b", 5, Some(json!({ "thread": "main" })))?;
            txn.schedule("turns", "c", 999, None)?;
            txn.schedule("other", "d", 1, None)
        });
        let due = store.ready("turns", 10, 10).expect("ready");
        assert_eq!(
            due.iter()
                .map(|item| item.work_id.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "a"]
        );
        assert_eq!(
            due[0].payload,
            Some(json!({ "thread": "main" })),
            "the payload of a waiting turn travels with its entry"
        );
        assert_eq!(due[1].payload, None);
        assert_eq!(
            store
                .ready("turns", 10, 1)
                .expect("page")
                .iter()
                .map(|item| item.work_id.as_str())
                .collect::<Vec<_>>(),
            vec!["b"],
            "the page is the earliest entries, not an arbitrary one"
        );
    }

    #[test]
    fn a_negative_ready_at_removes_the_entry() {
        let store = store();
        write(&store, "s:1", |txn| txn.schedule("turns", "a", 10, None));
        write(&store, "s:1", |txn| txn.schedule("turns", "a", -1, None));
        assert!(store.ready("turns", 100, 10).expect("ready").is_empty());
        // The removal rolls back with the transaction that made it.
        let _ = txn(&store, "s:1", |txn| {
            txn.schedule("turns", "b", 20, None)?;
            Err::<(), _>(DurableError::invalid("boom"))
        });
        assert!(store.ready("turns", 100, 10).expect("ready").is_empty());
    }

    #[test]
    fn a_revision_that_moved_under_the_transaction_is_a_conflict() {
        let armed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&armed);
        let store = PostgresStore::new(MemoryPg::new().before_bump(move |revision| {
            if flag.load(Ordering::SeqCst) {
                *revision += 1;
            }
        }));
        write(&store, "s:1", |txn| txn.put("a", Some(json!(1))));
        assert_eq!(store.session().revision("s:1"), Some(1));

        // Another host commits between our read and our update.
        armed.store(true, Ordering::SeqCst);
        let error = txn(&store, "s:1", |txn| txn.put("b", Some(json!(2)))).expect_err("conflict");
        assert_eq!(error.code(), crate::ErrorCode::Conflict);
        armed.store(false, Ordering::SeqCst);

        assert_eq!(
            txn(&store, "s:1", |txn| txn.get("b")).expect("read"),
            None,
            "the conflicting write was rolled back"
        );
        assert_eq!(
            store.session().revision("s:1"),
            Some(1),
            "the losing transaction did not advance the revision"
        );
    }

    #[test]
    fn a_read_only_transaction_costs_no_write() {
        let store = store();
        write(&store, "s:1", |txn| txn.put("a", Some(json!(1))));
        store.session().clear_statements();
        assert_eq!(
            txn(&store, "s:1", |txn| txn.get("a")).expect("read"),
            Some(json!(1))
        );
        let statements = store.session().statements();
        assert!(
            statements.contains(&sql::LOCK_SCOPE.to_string()),
            "{statements:?}"
        );
        assert!(
            !statements
                .iter()
                .any(|statement| statement.contains("UPDATE agentscope_scope")),
            "a read must not bump the revision: {statements:?}"
        );
        assert_eq!(
            store.session().revision("s:1"),
            Some(1),
            "the revision counts writes, not transactions"
        );
    }

    #[test]
    fn the_store_is_shared_and_does_not_pretend_to_know_its_own_size() {
        let store = store();
        assert!(store.shared_across_hosts());
        assert_eq!(
            store.scope_bytes("s:1").expect("size"),
            None,
            "a size would cost an aggregate per commit; compaction is out of band"
        );
    }

    #[test]
    fn the_schema_is_idempotent_and_is_not_issued_by_a_transaction() {
        let store = store();
        store.ensure_schema().expect("schema");
        store.ensure_schema().expect("schema again");
        assert_eq!(
            store.session().ddl().len(),
            2 * crate::schema::statements().len()
        );
        assert!(store.session().statements().iter().all(|statement| {
            statement.starts_with("CREATE ") && statement != "BEGIN" && statement != "COMMIT"
        }));
    }

    #[tokio::test]
    async fn a_turn_runs_to_a_fenced_commit_over_the_postgres_mapping() {
        let backend = TxnBackend::with_store(Arc::new(store()));
        let key = SessionKey::new("ns", "s1").expect("session key");
        backend
            .create(SessionSpec {
                key: key.clone(),
                primary_binding: ThreadBinding::new("harness-a", "def-1", "state-1")
                    .expect("binding"),
                metadata: serde_json::Value::Null,
            })
            .await
            .expect("create session");
        let thread = ThreadKey::new(key, "main").expect("thread key");
        let submitted = backend
            .submit(&thread, "req-1", json!({ "text": "hi" }))
            .await
            .expect("submit");
        let lease = backend
            .claim(&thread, "owner-a", Duration::from_secs(30))
            .await
            .expect("claim")
            .expect("a claimable turn");
        let frame = backend.frame(&lease).await.expect("frame");
        assert_eq!(frame.turn.status, TurnStatus::Running);

        let mut commit = Commit::new("c1", frame.turn.revision, TurnStatus::Succeeded);
        commit.output = json!({ "reply": "ok" });
        let receipt = backend
            .commit(&lease, commit.clone())
            .await
            .expect("commit");
        assert_eq!(receipt.revision, 1);
        // The commit released the lease: a terminal turn has no owner, so its state is read
        // through the thread rather than through the lease that wrote it.
        assert_eq!(
            backend
                .turn(&thread, &submitted.turn_id)
                .await
                .expect("turn")
                .status,
            TurnStatus::Succeeded
        );
        // The same commit replayed answers the original receipt, not a second application.
        assert_eq!(
            backend.commit(&lease, commit).await.expect("replay"),
            receipt
        );
    }
}
