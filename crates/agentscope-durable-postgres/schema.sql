-- AgentScope durable record store, PostgreSQL mapping.
--
-- One row per record: a scope is one row in `agentscope_scope`, and every record it holds is its
-- own row in `agentscope_part`. Nothing here ever rewrites a scope document, so a commit costs
-- the records it touched, not the size of the conversation.
--
-- Every key column collates as "C". The runtime orders part keys with Rust's `str` order, which
-- is byte order; only the C collation makes PostgreSQL answer the same order, and a server
-- default collation (en_US.UTF-8, ICU) silently changes what `keys(prefix, after, limit)`
-- returns on the same data. It also keeps the prefix range `k >= prefix AND k < bound` a plain
-- b-tree range instead of a collation-aware comparison.

-- One row per session scope. `revision` is the compare-and-set token of the scope: a transaction
-- reads it under `SELECT ... FOR UPDATE`, and its final `UPDATE ... WHERE revision = <read>` is
-- the assertion that the read set was still current when the writes land.
CREATE TABLE IF NOT EXISTS agentscope_scope (
    scope_key  text COLLATE "C" NOT NULL PRIMARY KEY,
    revision   bigint NOT NULL DEFAULT 0,
    updated_at bigint NOT NULL,
    CONSTRAINT agentscope_scope_key_len CHECK (char_length(scope_key) BETWEEN 1 AND 256)
);

-- One row per `Txn` record. The primary key is the access path for both single reads
-- (`scope_key = $1 AND k = $2`), batch reads (`k = ANY($2)`) and the journal page
-- (`k >= prefix AND k < bound ORDER BY k LIMIT n`): equality on the leading column, ordered range
-- on the trailing one.
CREATE TABLE IF NOT EXISTS agentscope_part (
    scope_key text COLLATE "C" NOT NULL
        REFERENCES agentscope_scope (scope_key) ON DELETE CASCADE,
    k         text COLLATE "C" NOT NULL,
    v         jsonb NOT NULL,
    PRIMARY KEY (scope_key, k)
);

-- The readiness index, shared by every scope so that one `ready(queue, now, limit)` answers the
-- due turns of a queue without touching any scope row. A removed entry is a deleted row: there
-- is no tombstone to keep out of the way.
CREATE TABLE IF NOT EXISTS agentscope_work (
    queue    text COLLATE "C" NOT NULL,
    work_id  text COLLATE "C" NOT NULL,
    ready_at bigint NOT NULL,
    payload  jsonb,
    PRIMARY KEY (queue, work_id)
);

CREATE INDEX IF NOT EXISTS agentscope_work_due
    ON agentscope_work (queue, ready_at, work_id);
