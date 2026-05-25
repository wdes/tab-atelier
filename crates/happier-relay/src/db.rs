// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::path::Path;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

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
    let pool = SqlitePoolOptions::new().max_connections(8).connect_with(opts).await?;
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
        sqlx::query(
            "UPDATE accounts SET content_public_key = ?1, content_public_key_sig = ?2, updated_at = ?3 WHERE id = ?4",
        )
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

/// Shared-account variant: the first auth ever seeds the one row in
/// `accounts`; every subsequent auth — regardless of which public key
/// signed the challenge — returns that same id. The signature was
/// already verified at this point, so we don't lose any security
/// guarantee from the request: the caller proved possession of *a*
/// keypair, we just stop caring *which* one.
///
/// The seeding row's `public_key_hex` belongs to whichever device got
/// here first; we never update it on subsequent calls because that
/// would churn the `UNIQUE` index for no benefit.
pub async fn upsert_account_shared(
    pool: &SqlitePool,
    public_key_hex: &str,
    content_public_key: Option<&[u8]>,
    content_public_key_sig: Option<&[u8]>,
) -> anyhow::Result<String> {
    let existing: Option<(String,)> = sqlx::query_as("SELECT id FROM accounts ORDER BY created_at ASC LIMIT 1")
        .fetch_optional(pool)
        .await?;
    if let Some((id,)) = existing {
        return Ok(id);
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0_i64, |d| d.as_secs().cast_signed());
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

#[cfg(test)]
mod tests {
    use super::{open, upsert_account, upsert_account_shared};
    use sqlx::SqlitePool;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::path::Path;

    async fn fresh_pool() -> SqlitePool {
        let opts = SqliteConnectOptions::new()
            .filename(":memory:")
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .expect("connect :memory:");
        sqlx::migrate!("./migrations").run(&pool).await.expect("migrate");
        pool
    }

    #[tokio::test]
    async fn open_memory_runs_migrations() {
        let pool = open(Path::new(":memory:")).await.expect("open :memory:");
        pool.close().await;
    }

    #[tokio::test]
    async fn upsert_account_inserts_then_updates_same_id() {
        let pool = fresh_pool().await;
        let pk = "aa".repeat(32);
        let id1 = upsert_account(&pool, &pk, Some(&[1, 2, 3]), Some(&[4, 5, 6]))
            .await
            .unwrap();
        let id2 = upsert_account(&pool, &pk, Some(&[9, 9]), Some(&[8, 8])).await.unwrap();
        assert_eq!(id1, id2, "same pubkey must return same id");

        let (content,): (Option<Vec<u8>>,) = sqlx::query_as("SELECT content_public_key FROM accounts WHERE id = ?1")
            .bind(&id1)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(content.as_deref(), Some(&[9_u8, 9][..]), "update path ran");

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM accounts")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn upsert_account_different_pubkey_returns_new_id() {
        let pool = fresh_pool().await;
        let id1 = upsert_account(&pool, &"aa".repeat(32), None, None).await.unwrap();
        let id2 = upsert_account(&pool, &"bb".repeat(32), None, None).await.unwrap();
        assert_ne!(id1, id2);
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM accounts")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn upsert_account_handles_none_content() {
        let pool = fresh_pool().await;
        let id = upsert_account(&pool, &"cc".repeat(32), None, None).await.unwrap();
        let (ck, sig): (Option<Vec<u8>>, Option<Vec<u8>>) =
            sqlx::query_as("SELECT content_public_key, content_public_key_sig FROM accounts WHERE id = ?1")
                .bind(&id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(ck.is_none() && sig.is_none());
    }

    #[tokio::test]
    async fn upsert_account_shared_seeds_then_pins_first_pubkey() {
        let pool = fresh_pool().await;
        let first_pk = "aa".repeat(32);
        let id1 = upsert_account_shared(&pool, &first_pk, Some(&[1]), Some(&[2]))
            .await
            .unwrap();
        let id2 = upsert_account_shared(&pool, &"bb".repeat(32), Some(&[3]), Some(&[4]))
            .await
            .unwrap();
        let id3 = upsert_account_shared(&pool, &"cc".repeat(32), None, None)
            .await
            .unwrap();
        assert_eq!(id1, id2);
        assert_eq!(id1, id3);

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM accounts")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1, "shared variant never inserts a second row");

        let (stored_pk,): (String,) = sqlx::query_as("SELECT public_key_hex FROM accounts WHERE id = ?1")
            .bind(&id1)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stored_pk, first_pk, "seeder pubkey is never overwritten");
    }
}
