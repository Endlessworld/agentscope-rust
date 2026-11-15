# PostgreSQL as the durable record store

`agentscope-durable-jdbc` maps the durable runtime's `RecordStore` onto PostgreSQL. It is a
*mapping*, not a driver: it decides what a scope row is, what a record row is, how a page becomes
an index range and how a commit is fenced by a revision compare-and-set. A driver supplies
`SqlSession` — four methods — and holds no decisions.

This is the `(a)` deliverable of L4‑c: nothing here waits on a network, `cargo test` runs without
a database, and what only a server can answer lives in `tests/schema_live.rs` behind
`DATABASE_URL`.

## 1. Tables

The whole schema is `crates/agentscope-durable-jdbc/schema.sql` (`SCHEMA_SQL`, split into
statements by `schema::statements()`). Every statement is `CREATE … IF NOT EXISTS`, so applying it
on each connect is safe and there is no migration table for this version.

| Table | Row means | Key | Columns |
| --- | --- | --- | --- |
| `agentscope_scope` | one session scope | `scope_key` PK | `revision bigint` (CAS token), `updated_at bigint` |
| `agentscope_part` | one `Txn` record | `(scope_key, k)` PK | `v jsonb` |
| `agentscope_work` | one readiness entry | `(queue, work_id)` PK | `ready_at bigint`, `payload jsonb` |

Plus `agentscope_work_due (queue, ready_at, work_id)` for `ready()`.

Why a row per record instead of one document per scope: the file store rewrites the whole scope on
every commit, so a turn costs the size of the conversation so far (the write amplification the
journal work already fought). Here a commit costs the rows it changed. The same row key is the
access path for all three shapes the backend needs:

* one record — `scope_key = $1 AND k = $2` (a control poll several times a second);
* several records — `scope_key = $1 AND k = ANY($2::text[])` (`read_parts`, one round trip, no lock);
* a page — `scope_key = $1 AND k >= prefix AND k < bound ORDER BY k LIMIT n` (journal, queue, threads).

### `COLLATE "C"` is load-bearing

Every key column is `text COLLATE "C"`. The runtime orders part keys with Rust's `str` order,
which is byte order; only the C collation makes PostgreSQL answer the same order. A server default
of `en_US.UTF-8` or ICU reorders punctuation, case and accents, which would change where a page
ends — the in-memory store and PostgreSQL would disagree about the same data. `tests/schema_live.rs`
asserts the order (`AZabá`) on a real server.

Prefix pages avoid `LIKE` for the same reason: the upper edge is computed in Rust
(`mapping::prefix_bound`, the last code point incremented, surrogates skipped, `U+10FFFF` falling
back to the parent), so the page is an equality-plus-range index scan with no escape rules and no
`($3 IS NULL OR k > $3)` filter — that disjunction would re-read the prefix from its start on every
page, the quadratic read this schema exists to avoid.

## 2. Transaction boundary

One `RecordStore::transaction` is one SQL transaction, and it is the durability unit: the
checkpoint, the tool intent and the events are the same `COMMIT` or none of them happened.

```
BEGIN
  SELECT revision FROM agentscope_scope WHERE scope_key = $1 FOR UPDATE   -- row lock
  ... the closure's reads and writes (one statement per record) ...
  -- only if the closure wrote something:
  UPDATE agentscope_scope SET revision = revision + 1, updated_at = $3
   WHERE scope_key = $1 AND revision = $2                                 -- compare-and-set
COMMIT
```

* A scope row is created on first write (`INSERT … ON CONFLICT (scope_key) DO NOTHING`, then the
  locking select again). The extra statement happens once per scope, not once per transaction.
* A transaction that changed nothing issues **no update and no bump**: the revision counts writes,
  not transactions. Without this, every control poll would rewrite the scope row and invalidate
the revision of every concurrent reader.
* Writes go to the database immediately rather than being staged in the process. The SQL
  transaction is the rollback; a staged copy would be a second representation of every value in
  memory, which a journal flush of tens of megabytes must not build.

## 3. Multi-host concurrency

The durable runtime already fences a thread with a lease (`claim`/`renew`, `epoch`, `FENCED`).
PostgreSQL adds the layer the file store gets from its scope lock file:

* **The row lock serialises writers of one scope.** `SELECT … FOR UPDATE` on `agentscope_scope`
  means two hosts write one scope one after the other, never interleaved. Reads through
  `transaction` wait behind the writer for the duration of the statement, exactly as they wait for a
  lock file; `read_parts` does not take the lock at all, which is why the hot poll path is cheap.
* **The revision predicate is the assertion that the read set was current.** If the bump moves zero
  rows — the lock did not hold, a statement ran outside its transaction, or an unknown writer
  touched the scope — the transaction is rolled back and answers `CONFLICT`. It never applies a
  stale read on top of a newer one.
* **`shared_across_hosts()` is `true`.** Any number of hosts may open the same database; nothing
  claims exclusive ownership of a scope or of the store.

Isolation level: the default `READ COMMITTED` is enough because every write path takes the row
lock first. Do not lower it.

## 4. Cost

| Operation | Statements |
| --- | --- |
| a commit that wrote *n* records | `BEGIN`, 1 lock select, *n* upserts/deletes, 1 CAS update, `COMMIT` |
| a read-only transaction | `BEGIN`, 1 lock select, reads, `COMMIT` (no write) |
| one control poll (`read_parts`) | 1 select, no transaction |
| `ready(queue, now, n)` | 1 index select, no transaction |

`scope_bytes()` answers `None`. A real size means aggregating a scope's rows on every commit, and
what it feeds — compaction — can be done out of band:

```sql
-- scopes worth compacting, by live bytes
SELECT scope_key, count(*) AS records, sum(pg_column_size(v)) AS bytes
FROM agentscope_part
GROUP BY scope_key
HAVING sum(pg_column_size(v)) > 4 * 1024 * 1024
ORDER BY bytes DESC;
```

Because a store answering `None` never triggers threshold compaction on its own, a deployment that
wants it runs that query on a schedule and prunes through the backend. `prune_events` is not
implemented here (the trait's default returns `0`), so a PostgreSQL deployment keeps its whole
event log unless it prunes explicitly — the honest state, not a silent one.

## 5. Known limits

* **`jsonb` cannot store `U+0000`.** A value containing it is refused with `INVALID` and a message
  naming the reason (`mapping::encode`), rather than silently rewritten. The file and in-memory
  stores keep such a value; PostgreSQL cannot, and a record that would have to change shape is a
  refusal, not a corruption.
* **Keys and scope names must be `text`-storable.** A key containing `U+0000` is refused; an empty
  key is allowed (the other two stores allow it).
* **`U+10FFFF`-terminated prefixes** borrow their parent's edge; correct, and covered by tests.
* **No connection pool, no retry loop.** A driver adapter owns both. A `CONFLICT` is for the caller
  to retry, not for this crate to hide.

## 6. Running the live tests

The offline suite (`cargo test -p agentscope-durable-jdbc`) needs nothing. The server-side
suite needs `psql` on `PATH` and a database:

```powershell
# PowerShell
$env:DATABASE_URL = 'postgresql://postgres:postgres@127.0.0.1:5432/agentscope_test'
cargo test -p agentscope-durable-jdbc --test schema_live -- --test-threads=1
```

```bash
# bash
export DATABASE_URL='postgresql://postgres:postgres@127.0.0.1:5432/agentscope_test'
cargo test -p agentscope-durable-jdbc --test schema_live -- --test-threads=1
```

With `DATABASE_URL` unset, or `psql` missing, each test prints what it skipped and passes, so a
plain `cargo test` stays green. The tests create and drop only rows they own (`live:*`).

What they prove, which the offline double cannot: the DDL is valid PostgreSQL and applies twice;
the key page is an `Index` plan and not a `Seq Scan`; the key collation orders bytes the way Rust
does; the revision compare-and-set moves one row or none; `ready` answers due entries in readiness
order and a removal is a deleted row.

## 7. Wiring a driver

A driver implements three things and decides nothing:

```rust
trait SqlTransaction {
    fn now_millis(&mut self) -> Result<i64>;                       // SELECT clock_timestamp()
    fn query(&mut self, sql: &str, params: &[Param]) -> Result<Vec<Row>>;
    fn execute(&mut self, sql: &str, params: &[Param]) -> Result<u64>;  // rows affected
}

trait SqlSession {
    fn transaction(&self, work: &mut dyn FnMut(&mut dyn SqlTransaction) -> Result<()>) -> Result<()>;
    fn query(&self, sql: &str, params: &[Param]) -> Result<Vec<Row>>;   // autocommit read
}
```

`Param` is `Text` / `BigInt` / `TextArray` / `Null`, and `Param::cast()` names the type to bind.
`Row::new(values)` takes the columns in select order; a driver that only speaks text decodes a
`jsonb` column with `mapping::decode` before building the row, because a JSON *string* is a value
and not an encoding.

`RecordStore` is synchronous while `tokio-postgres` is not, so a real adapter owns the blocking
hop — a dedicated thread with the current-thread runtime, or a small `block_on` bridge — and must
run `BEGIN`/`COMMIT`/`ROLLBACK` around the closure, mapping a failed closure to `ROLLBACK`. That
adapter is not in this crate on purpose: it is the only part that needs the dependency, and adding
it here would make the mapping untestable offline. The `testing` feature exposes `MemoryPg`, the
in-memory `SqlSession`, for a downstream test that wants the same contract checks.

## 8. Migration steps

1. Create the database and role; apply `schema.sql` (or call `PostgresStore::ensure_schema` on
   connect — it is idempotent). Verify with `tests/schema_live.rs` against that database.
2. Pick the backend construction point (where `TxnBackend::with_store` is called today with a
   `FileStore`) and pass `Arc::new(PostgresStore::new(driver))` instead. Nothing above
   `RecordStore` changes: the backend, the runtime and the agents are store-agnostic.
3. Migrate existing sessions by replaying them through the backend, not by copying rows: the file
   store's document layout is an implementation detail, while `SessionBackend` is the stable
   contract. A session is `create`, then a checkpoint and its journal entries through `restore`;
   doing it that way keeps one writer of the format (the backend).
4. Roll back by pointing the same construction site back at the file store. No data written by
   PostgreSQL is read by the file store, so a rollback means starting those sessions from their
   last checkpoint in the file store.
5. Watch two numbers after the switch: `SELECT mean number of statements per commit` (should be
   rows touched + 4) and the count of `CONFLICT` answers (should be near zero: a conflict means the
   lease and the row lock disagreed).
