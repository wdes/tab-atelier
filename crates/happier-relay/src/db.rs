// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::path::Path;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;

/// Open (or create) the `SQLite` file at `path` and run pending migrations.
///
/// Migrations live in `crates/happier-relay/migrations/` and are baked into
/// the binary at compile time via `sqlx::migrate!`.
pub async fn open(path: &Path) -> anyhow::Result<SqlitePool> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}

/// Upsert by public key. Returns the account id (uuid v4 string).
///
/// happier's reference implementation writes the optional content key +
/// signature alongside the public key on every successful login; we mirror
/// that so a future client that switches between content-key on/off keeps
/// the latest pair on file.
pub async fn upsert_account(
    pool: &SqlitePool,
    public_key_hex: &str,
    content_public_key: Option<&[u8]>,
    content_public_key_sig: Option<&[u8]>,
) -> anyhow::Result<String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0_i64, |d| d.as_secs().cast_signed());

    let existing: Option<(String,)> = sqlx::query_as("SELECT id FROM accounts WHERE public_key_hex = ?1")
        .bind(public_key_hex)
        .fetch_optional(pool)
        .await?;
    if let Some((id,)) = existing {
        sqlx::query("UPDATE accounts SET content_public_key = ?1, content_public_key_sig = ?2, updated_at = ?3 WHERE id = ?4")
            .bind(content_public_key)
            .bind(content_public_key_sig)
            .bind(now)
            .bind(&id)
            .execute(pool)
            .await?;
        return Ok(id);
    }

    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO accounts (id, public_key_hex, content_public_key, content_public_key_sig, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")
        .bind(&id)
        .bind(public_key_hex)
        .bind(content_public_key)
        .bind(content_public_key_sig)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await?;
    Ok(id)
}
