-- One row per (hook, target). Rows that run out of attempts, or get a permanent error,
-- stay here with status 'dead': that is the dead-letter list.
CREATE TABLE deliveries (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    hook_id            INTEGER NOT NULL REFERENCES hooks (id),
    target_url         TEXT    NOT NULL,
    status             TEXT    NOT NULL DEFAULT 'pending'
                       CHECK (status IN ('pending', 'delivered', 'dead')),
    attempts           INTEGER NOT NULL DEFAULT 0,
    next_attempt_at_ms INTEGER NOT NULL,
    last_status_code   INTEGER,
    last_error         TEXT,
    updated_at_ms      INTEGER NOT NULL
);

CREATE INDEX deliveries_due ON deliveries (next_attempt_at_ms) WHERE status = 'pending';
CREATE INDEX deliveries_hook ON deliveries (hook_id);
