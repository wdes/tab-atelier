-- Sessions and their messages.
--
-- The schema follows happier's Prisma model closely so that field
-- names line up with what the mobile client expects on the wire.
-- Encryption mode and ciphertext bytes are kept verbatim — the server
-- never reads them.
--
-- Optimistic locking: metadata_version and agent_state_version are
-- separate so a client can PATCH one without bumping the other.
--
-- Single-tenant note: account_id is still recorded so we can flip
-- back to multi-user later without a schema migration.

CREATE TABLE IF NOT EXISTS sessions (
    id                      TEXT    PRIMARY KEY,
    account_id              TEXT    NOT NULL,
    tag                     TEXT    NOT NULL,
    seq                     INTEGER NOT NULL DEFAULT 0,
    encryption_mode         TEXT    NOT NULL DEFAULT 'e2ee',
    metadata                TEXT,            -- ciphertext (base64) or plaintext per mode
    metadata_version        INTEGER NOT NULL DEFAULT 0,
    agent_state             TEXT,
    agent_state_version     INTEGER NOT NULL DEFAULT 0,
    data_encryption_key     BLOB,
    active                  INTEGER NOT NULL DEFAULT 1,
    active_at               INTEGER,
    archived_at             INTEGER,
    created_at              INTEGER NOT NULL,
    updated_at              INTEGER NOT NULL,
    FOREIGN KEY (account_id) REFERENCES accounts (id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS sessions_account_idx ON sessions (account_id, updated_at DESC);
CREATE INDEX IF NOT EXISTS sessions_account_archived_idx ON sessions (account_id, archived_at);
CREATE UNIQUE INDEX IF NOT EXISTS sessions_account_tag_idx ON sessions (account_id, tag);

CREATE TABLE IF NOT EXISTS session_messages (
    id              TEXT    PRIMARY KEY,
    session_id      TEXT    NOT NULL,
    seq             INTEGER NOT NULL,
    local_id        TEXT,
    sidechain_id    TEXT,
    message_role    TEXT,
    content         TEXT    NOT NULL,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    FOREIGN KEY (session_id) REFERENCES sessions (id) ON DELETE CASCADE
);

-- Same shape happier uses for the "by local id" lookup. Single-tenant
-- doesn't strictly need it yet but the index is tiny and harmless.
CREATE UNIQUE INDEX IF NOT EXISTS session_messages_local_idx
    ON session_messages (session_id, local_id)
    WHERE local_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS session_messages_seq_idx ON session_messages (session_id, seq);
