-- Per-user KV store. Mirrors happier's UserKVStore: opaque blob
-- value with a version number for optimistic concurrency.
--
-- Single-tenant: account_id is still kept for the same reason as
-- sessions — adding multi-user later won't need a schema rewrite.

CREATE TABLE IF NOT EXISTS user_kv (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id  TEXT    NOT NULL,
    key         TEXT    NOT NULL,
    value       BLOB,
    version     INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    FOREIGN KEY (account_id) REFERENCES accounts (id) ON DELETE CASCADE
);

CREATE UNIQUE INDEX IF NOT EXISTS user_kv_unique_idx ON user_kv (account_id, key);
CREATE INDEX IF NOT EXISTS user_kv_prefix_idx ON user_kv (account_id, key);
