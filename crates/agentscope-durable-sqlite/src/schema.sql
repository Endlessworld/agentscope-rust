-- AgentScope durable record store, SQLite mapping.
--
-- Same three tables as the PostgreSQL schema (`agentscope-durable-jdbc/schema.sql`): one row per
-- scope, one row per record, one readiness index. Nothing here rewrites a scope document, so a
-- commit costs the records it touched, not the size of the conversation.
--
-- What is deliberately engine-specific: `v` and `payload` are JSON text rather than `jsonb`
-- (SQLite has no such type), the tables keep their rowid instead of `WITHOUT ROWID` (measured:
-- the rowid table plus its covering primary key is smaller for these row shapes), and space is
-- returned with `PRAGMA auto_vacuum = INCREMENTAL` instead of autovacuum.
--
-- Every key column is explicitly BINARY. That is SQLite's default, but it is also the contract:
-- the runtime orders part keys with Rust's byte order, and a build that compiled in another
-- collation must not silently change what `keys(prefix, after, limit)` returns. PostgreSQL
-- writes the same contract as COLLATE "C".

-- One row per session scope. `revision` is the compare-and-set token: a transaction reads it
-- under the write lock (BEGIN IMMEDIATE) and its final `UPDATE ... WHERE revision = <read>` is
-- the assertion that the read set was still current when the writes land. The length bound is
-- the same one the PostgreSQL schema and the PostgreSQL mapping enforce, so a scope key that one
-- store accepts is not refused by the other.
CREATE TABLE IF NOT EXISTS agentscope_scope (
    scope_key  TEXT COLLATE BINARY NOT NULL PRIMARY KEY,
    revision   INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL,
    CONSTRAINT agentscope_scope_key_len CHECK (length(scope_key) BETWEEN 1 AND 256)
);

-- One row per `Txn` record. The primary key is the access path for a single read, a batch read
-- and the journal page: equality on the leading column, ordered range on the trailing one. The
-- foreign key makes an orphan record impossible and gives a scope delete its cascade.
CREATE TABLE IF NOT EXISTS agentscope_part (
    scope_key TEXT COLLATE BINARY NOT NULL
        REFERENCES agentscope_scope (scope_key) ON DELETE CASCADE,
    k         TEXT COLLATE BINARY NOT NULL,
    v         TEXT NOT NULL,
    PRIMARY KEY (scope_key, k)
);

-- The readiness index, shared by every scope so that one `ready(queue, now, limit)` answers the
-- due turns of a queue without touching any scope row. A removed entry is a deleted row: there
-- is no tombstone to keep out of the way.
CREATE TABLE IF NOT EXISTS agentscope_work (
    queue    TEXT COLLATE BINARY NOT NULL,
    work_id  TEXT COLLATE BINARY NOT NULL,
    ready_at INTEGER NOT NULL,
    payload  TEXT,
    PRIMARY KEY (queue, work_id)
);

-- `work_id` is part of the index so that `ORDER BY ready_at, work_id` is read in index order
-- instead of being sorted after the page is selected; the PostgreSQL schema has the same three
-- columns.
CREATE INDEX IF NOT EXISTS agentscope_work_due
    ON agentscope_work (queue, ready_at, work_id);
