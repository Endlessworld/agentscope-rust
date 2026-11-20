# SQLite as the durable record store

`crates/agentscope-durable-sqlite` implements `RecordStore` over one SQLite database file. It is
one row per record, the same three tables as the PostgreSQL mapping
(`crates/agentscope-durable-jdbc/schema.sql`), and the same revision compare-and-set. The
engine-specific choices are listed where they are made, not hidden.

## 1. Tables

The DDL lives in `crates/agentscope-durable-sqlite/src/schema.sql` and is applied on open
(`schema::statements()`); every statement is idempotent, so no separate startup hook is needed.

| Table | Key | Purpose |
| --- | --- | --- |
| `agentscope_scope` | `scope_key` | One row per scope; `revision` is the compare-and-set token, `updated_at` the last write. |
| `agentscope_part` | `(scope_key, k)` | One row per `Txn` record; `v` is JSON text. Foreign key to the scope, `ON DELETE CASCADE`. |
| `agentscope_work` | `(queue, work_id)` | The readiness index; `agentscope_work_due` covers `(queue, ready_at, work_id)`. |

What matches PostgreSQL on purpose:

* the foreign key and the cascade (`PRAGMA foreign_keys = ON` is per connection and is switched on
  in `from_connection`);
* `CHECK (length(scope_key) BETWEEN 1 AND 256)`, the same bound `mapping::scope_key` enforces;
* `revision INTEGER NOT NULL DEFAULT 0`;
* an explicit `COLLATE BINARY` on every key column (SQLite's default, but also the contract: the
  runtime orders keys with Rust's byte order, and PostgreSQL writes the same as `COLLATE "C"`);
* the three-column readiness index, so `ORDER BY ready_at, work_id` is read in index order;
* the key page is four statements (with/without cursor × with/without upper bound), so a cursor
  and a bound are range constraints instead of filters after the scan. The PostgreSQL mapping
  splits the same way for the same reason.

What is deliberately different:

* `v` and `payload` are JSON text, not `jsonb` (SQLite has no such type). An unparseable stored
  value is reported as `INVALID`.
* The tables keep their rowid; `WITHOUT ROWID` was measured to be ~7% larger on a real session
  database (long scope keys, large values), so it is not used.
* `scope_bytes()` really measures the scope (`SUM(length(CAST(v AS BLOB)))`). PostgreSQL answers
  `None` because the aggregate is expensive there; on a local file the measurement is what makes
  threshold compaction work.

## 2. Pragmas

| Pragma | Value | Why |
| --- | --- | --- |
| `auto_vacuum` | `INCREMENTAL` | Only on a database created by this store, and set before the first write (WAL writes a header, after which the pragma is a no-op). `reclaim_space()` runs `VACUUM` only when a trim released a real share of the file. |
| `journal_mode` | `WAL` | Readers see the last committed state without waiting for the writer. |
| `synchronous` | `NORMAL` | The standard WAL trade-off: a crash can lose the last transaction, not corrupt the file. |
| `busy_timeout` | 10 s | SQLite has one writer. Every ACP session opens its own connection on the same file, so a writer that finds the write lock busy waits instead of failing with `SQLITE_BUSY`. Matches the harness and JDBC backends. |
| `foreign_keys` | `ON` | Off by default in SQLite; the schema's foreign key is only real when this is on. |
| `application_id` | `0x41534350` (`"ASCP"`) | Set on first open; a non-zero id that is not this one is refused rather than written into. |
| `user_version` | `2` | The schema version. A newer version is refused; version 1 is migrated in place on open. |

## 3. Transaction boundary

A write transaction is `BEGIN IMMEDIATE`, the writes (one upsert or delete per record), then
`UPDATE agentscope_scope SET revision = revision + 1 WHERE scope_key = ? AND revision = <read>`,
then `COMMIT`. `BEGIN IMMEDIATE` serialises writers on the database file the way the PostgreSQL
row lock serialises them on the scope row; the revision predicate is the assertion that the read
set was still current when the writes landed. A miss rolls back and answers `CONFLICT`.

A read-only transaction is `BEGIN DEFERRED` and refuses `put`/`schedule`. `read_parts()` — the
control poll that runs several times a second while a model call streams — is one indexed
statement and no transaction at all.

## 4. Schema version and migration

`PRAGMA user_version` is the version. On open:

* `>` 2: refused with `INVALID` (written by a build that knows a shape this one does not);
* `== 2`: nothing to do;
* `< 2` and no `agentscope_part` table: the current schema is created;
* `< 2` and a version 1 table: the documented table-rebuild procedure runs in one `IMMEDIATE`
  transaction — create `*_v2` tables, copy all three, drop the old ones, rename, recreate the
  index, `PRAGMA foreign_key_check`, then record the version. A crash leaves the version 1
  database untouched.

A database whose version pragma was never written but whose tables already have the version 2
shape (a migration that committed and then failed to record) is not rebuilt again: the shape is
detected from `sqlite_master`.

## 5. Known limits

* **One host.** WAL is not safe on a network filesystem, so `shared_across_hosts()` answers `false`.
* **One writer.** The connection sits behind a mutex; concurrent sessions in one process queue on
  it, and a second process waits on `busy_timeout`.
* **`U+0000` is refused inside a value.** PostgreSQL's `jsonb` cannot store it, and the file and
  in-memory stores can; the SQLite store refuses it too, so a commit succeeds or fails the same
  way on every store. A record that one store accepts and another refuses would turn the choice of
  store into data loss.
* **`scope_bytes()` is a scan** of the scope's rows on every commit (the same query PostgreSQL
  declines to run). It feeds threshold compaction; a deployment that does not want the scan can
  leave the store model at its default and compact out of band.

## 6. Running the tests

```powershell
cargo test -p agentscope-durable-sqlite -j 2
$env:DURABLE_CONTRACT_STORE = 'sqlite'
cargo test -p agentscope-durable --test backend_contract -j 2
```

The first suite includes the in-place migration (a version 1 database with rows is opened and
every row is read back), the foreign-key cascade, the key bound, the page cap and the `U+0000`
refusal. The second runs the backend contract against the same SQLite store instead of the
in-memory map.
