-- Artifacts: opaque encrypted blob storage. happier splits the
-- header (small, indexed for listings) and the body (potentially
-- large, fetched on demand). Each has its own version counter so a
-- client can update the header without re-uploading the body.

CREATE TABLE IF NOT EXISTS artifacts (
    id                   TEXT    PRIMARY KEY,
    account_id           TEXT    NOT NULL,
    header               BLOB    NOT NULL,
    header_version       INTEGER NOT NULL DEFAULT 1,
    body                 BLOB    NOT NULL,
    body_version         INTEGER NOT NULL DEFAULT 1,
    data_encryption_key  BLOB    NOT NULL,
    seq                  INTEGER NOT NULL DEFAULT 0,
    created_at           INTEGER NOT NULL,
    updated_at           INTEGER NOT NULL,
    FOREIGN KEY (account_id) REFERENCES accounts (id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS artifacts_account_idx ON artifacts (account_id, updated_at DESC);
