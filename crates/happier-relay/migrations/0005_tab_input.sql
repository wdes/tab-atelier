-- Per-user queue of keystrokes destined for a specific tab. A mobile
-- client POSTs to /v1/tab-input; tab-atelier pulls from
-- /v1/tab-input/pending?since=N (long-poll) and flushes to the PTY.
--
-- Rows are deleted once consumed — we keep enough history for the
-- long-poll cursor (`since`) to make sense across reconnections,
-- but a periodic vacuum could trim aggressively.

CREATE TABLE IF NOT EXISTS tab_input (
    seq         INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id  TEXT    NOT NULL,
    tab_name    TEXT    NOT NULL,
    bytes       BLOB    NOT NULL,
    created_at  INTEGER NOT NULL,
    FOREIGN KEY (account_id) REFERENCES accounts (id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS tab_input_user_seq_idx ON tab_input (account_id, seq);
