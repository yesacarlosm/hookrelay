-- `IF NOT EXISTS` so databases created before migrations existed (v0.1/v0.2) adopt this one.
CREATE TABLE IF NOT EXISTS hooks (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    source         TEXT    NOT NULL,
    received_at_ms INTEGER NOT NULL,
    headers        TEXT    NOT NULL,
    body           BLOB    NOT NULL
);
