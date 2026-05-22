-- Single-tenant accounts table. In Option C we only ever expect one row,
-- but keeping the schema flexible lets us flip to multi-tenant later.
CREATE TABLE IF NOT EXISTS accounts (
    id                     TEXT    PRIMARY KEY,
    public_key_hex         TEXT    NOT NULL UNIQUE,
    content_public_key     BLOB,
    content_public_key_sig BLOB,
    created_at             INTEGER NOT NULL,
    updated_at             INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS accounts_public_key_idx ON accounts (public_key_hex);
