//! Storage/Registry module.
//!
//! This is the *only* place in the codebase that maps a public share ID to a
//! real filesystem path. Nothing outside this module ever sees a raw path
//! derived from network input, and no function here accepts a path from an
//! untrusted (network) caller -- paths only ever come from the local UI
//! (drag-and-drop), which runs on the same machine as the server.

use r2d2_sqlite::rusqlite::params;
use r2d2_sqlite::SqliteConnectionManager;
use serde::Serialize;

pub type Pool = r2d2::Pool<SqliteConnectionManager>;

#[derive(Debug, Clone, Serialize)]
pub struct Share {
    pub id: String,
    pub display_name: String,
    pub file_path: String, // NEVER serialized to a network client; strip before sending out
    pub size_bytes: i64,
    pub is_folder: bool,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub max_downloads: Option<i64>,
    pub download_count: i64,
    pub password_hash: Option<String>,
    pub status: String, // "active" | "revoked"
}

#[derive(Debug, Clone, Serialize)]
pub struct ShareSummary {
    pub id: String,
    pub display_name: String,
    pub size_bytes: i64,
    pub is_folder: bool,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub max_downloads: Option<i64>,
    pub download_count: i64,
    pub has_password: bool,
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct ShareEntry {
    pub stored_name: String,
    pub display_name: String,
    pub size_bytes: i64,
}

impl From<&Share> for ShareSummary {
    fn from(s: &Share) -> Self {
        ShareSummary {
            id: s.id.clone(),
            display_name: s.display_name.clone(),
            size_bytes: s.size_bytes,
            is_folder: s.is_folder,
            created_at: s.created_at,
            expires_at: s.expires_at,
            max_downloads: s.max_downloads,
            download_count: s.download_count,
            has_password: s.password_hash.is_some(),
            status: s.status.clone(),
        }
    }
}

pub fn init_pool(db_path: &str) -> anyhow::Result<Pool> {
    let manager = SqliteConnectionManager::file(db_path);
    let pool = r2d2::Pool::builder().max_size(8).build(manager)?;
    let conn = pool.get()?;
    conn.execute_batch(
        r#"
        PRAGMA journal_mode = WAL;
        PRAGMA foreign_keys = ON;

        CREATE TABLE IF NOT EXISTS shares (
            id              TEXT PRIMARY KEY,
            display_name    TEXT NOT NULL,
            file_path       TEXT NOT NULL,
            size_bytes      INTEGER NOT NULL,
            is_folder       INTEGER NOT NULL DEFAULT 0,
            created_at      INTEGER NOT NULL,
            expires_at      INTEGER,
            max_downloads   INTEGER,
            download_count  INTEGER NOT NULL DEFAULT 0,
            password_hash   TEXT,
            status          TEXT NOT NULL DEFAULT 'active'
        );

        CREATE TABLE IF NOT EXISTS download_events (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            share_id        TEXT NOT NULL,
            ip_address      TEXT NOT NULL,
            timestamp       INTEGER NOT NULL,
            bytes_transferred INTEGER NOT NULL DEFAULT 0,
            completed       INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS failed_auth_attempts (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            share_id        TEXT NOT NULL,
            ip_address      TEXT NOT NULL,
            timestamp       INTEGER NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_failed_auth_lookup
            ON failed_auth_attempts (share_id, ip_address, timestamp);

        -- Additive migration for folder shares. Existing databases remain
        -- compatible; single-file shares have no rows in this table.
        CREATE TABLE IF NOT EXISTS share_entries (
            share_id        TEXT NOT NULL,
            stored_name     TEXT NOT NULL,
            display_name    TEXT NOT NULL,
            size_bytes      INTEGER NOT NULL,
            PRIMARY KEY (share_id, stored_name),
            FOREIGN KEY (share_id) REFERENCES shares(id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_share_entries_share
            ON share_entries (share_id);
        "#,
    )?;
    Ok(pool)
}

pub fn insert_folder_share(
    pool: &Pool,
    share: &Share,
    entries: &[ShareEntry],
) -> anyhow::Result<()> {
    let mut conn = pool.get()?;
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO shares (id, display_name, file_path, size_bytes, is_folder, created_at,
            expires_at, max_downloads, download_count, password_hash, status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            share.id,
            share.display_name,
            share.file_path,
            share.size_bytes,
            share.is_folder as i64,
            share.created_at,
            share.expires_at,
            share.max_downloads,
            share.download_count,
            share.password_hash,
            share.status
        ],
    )?;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO share_entries (share_id, stored_name, display_name, size_bytes)
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        for entry in entries {
            stmt.execute(params![
                share.id,
                entry.stored_name,
                entry.display_name,
                entry.size_bytes
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn list_share_entries(pool: &Pool, share_id: &str) -> anyhow::Result<Vec<ShareEntry>> {
    let conn = pool.get()?;
    let mut stmt = conn.prepare(
        "SELECT stored_name, display_name, size_bytes
         FROM share_entries WHERE share_id = ?1 ORDER BY rowid",
    )?;
    let rows = stmt.query_map(params![share_id], |row| {
        Ok(ShareEntry {
            stored_name: row.get(0)?,
            display_name: row.get(1)?,
            size_bytes: row.get(2)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

pub fn insert_share(pool: &Pool, s: &Share) -> anyhow::Result<()> {
    let conn = pool.get()?;
    conn.execute(
        "INSERT INTO shares (id, display_name, file_path, size_bytes, is_folder, created_at,
            expires_at, max_downloads, download_count, password_hash, status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            s.id,
            s.display_name,
            s.file_path,
            s.size_bytes,
            s.is_folder as i64,
            s.created_at,
            s.expires_at,
            s.max_downloads,
            s.download_count,
            s.password_hash,
            s.status
        ],
    )?;
    Ok(())
}

/// Fetch a share by ID. This is the ONLY lookup path used to resolve a
/// network-supplied ID to a filesystem path. If the ID isn't an exact match
/// in the database, this returns Ok(None) -- no filesystem access is ever
/// attempted based on unmatched/malformed input.
pub fn get_share(pool: &Pool, id: &str) -> anyhow::Result<Option<Share>> {
    let conn = pool.get()?;
    let mut stmt = conn.prepare(
        "SELECT id, display_name, file_path, size_bytes, is_folder, created_at,
                expires_at, max_downloads, download_count, password_hash, status
         FROM shares WHERE id = ?1",
    )?;
    let mut rows = stmt.query(params![id])?;
    if let Some(row) = rows.next()? {
        Ok(Some(Share {
            id: row.get(0)?,
            display_name: row.get(1)?,
            file_path: row.get(2)?,
            size_bytes: row.get(3)?,
            is_folder: row.get::<_, i64>(4)? != 0,
            created_at: row.get(5)?,
            expires_at: row.get(6)?,
            max_downloads: row.get(7)?,
            download_count: row.get(8)?,
            password_hash: row.get(9)?,
            status: row.get(10)?,
        }))
    } else {
        Ok(None)
    }
}

pub fn list_shares(pool: &Pool) -> anyhow::Result<Vec<Share>> {
    let conn = pool.get()?;
    let mut stmt = conn.prepare(
        "SELECT id, display_name, file_path, size_bytes, is_folder, created_at,
                expires_at, max_downloads, download_count, password_hash, status
         FROM shares ORDER BY created_at DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(Share {
            id: row.get(0)?,
            display_name: row.get(1)?,
            file_path: row.get(2)?,
            size_bytes: row.get(3)?,
            is_folder: row.get::<_, i64>(4)? != 0,
            created_at: row.get(5)?,
            expires_at: row.get(6)?,
            max_downloads: row.get(7)?,
            download_count: row.get(8)?,
            password_hash: row.get(9)?,
            status: row.get(10)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

pub fn increment_download_count(pool: &Pool, id: &str) -> anyhow::Result<()> {
    let conn = pool.get()?;
    conn.execute(
        "UPDATE shares SET download_count = download_count + 1 WHERE id = ?1",
        params![id],
    )?;
    Ok(())
}

/// Atomically re-checks all download constraints and reserves one download.
/// This prevents concurrent requests from exceeding `max_downloads`.
pub fn claim_download(pool: &Pool, id: &str, now: i64) -> anyhow::Result<bool> {
    let mut conn = pool.get()?;
    let tx = conn.transaction()?;
    let changed = tx.execute(
        "UPDATE shares
         SET download_count = download_count + 1
         WHERE id = ?1
           AND status = 'active'
           AND (expires_at IS NULL OR expires_at > ?2)
           AND (max_downloads IS NULL OR download_count < max_downloads)",
        params![id, now],
    )?;
    tx.commit()?;
    Ok(changed == 1)
}

pub fn revoke_share(pool: &Pool, id: &str) -> anyhow::Result<bool> {
    let conn = pool.get()?;
    let n = conn.execute(
        "UPDATE shares SET status = 'revoked' WHERE id = ?1",
        params![id],
    )?;
    Ok(n > 0)
}

pub fn delete_share(pool: &Pool, id: &str) -> anyhow::Result<bool> {
    let mut conn = pool.get()?;
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM share_entries WHERE share_id = ?1", params![id])?;
    let n = tx.execute("DELETE FROM shares WHERE id = ?1", params![id])?;
    tx.commit()?;
    Ok(n > 0)
}

pub fn update_share_settings(
    pool: &Pool,
    id: &str,
    expires_at: Option<Option<i64>>,
    max_downloads: Option<Option<i64>>,
    password_hash: Option<Option<String>>,
) -> anyhow::Result<bool> {
    let conn = pool.get()?;
    let mut updated = false;
    if let Some(v) = expires_at {
        conn.execute(
            "UPDATE shares SET expires_at = ?1 WHERE id = ?2",
            params![v, id],
        )?;
        updated = true;
    }
    if let Some(v) = max_downloads {
        conn.execute(
            "UPDATE shares SET max_downloads = ?1 WHERE id = ?2",
            params![v, id],
        )?;
        updated = true;
    }
    if let Some(v) = password_hash {
        conn.execute(
            "UPDATE shares SET password_hash = ?1 WHERE id = ?2",
            params![v, id],
        )?;
        updated = true;
    }
    Ok(updated)
}

pub fn delete_expired_shares(pool: &Pool, now: i64) -> anyhow::Result<Vec<Share>> {
    // Only mark as revoked here (we don't touch the underlying file). Actual
    // file deletion is a separate, explicit user action -- see delete_share
    // plus deleting the file, which the caller in main.rs does only when the
    // user explicitly asks for it via the UI.
    let expired = {
        let conn = pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT id, display_name, file_path, size_bytes, is_folder, created_at,
                    expires_at, max_downloads, download_count, password_hash, status
             FROM shares WHERE status = 'active' AND expires_at IS NOT NULL AND expires_at <= ?1",
        )?;
        let rows = stmt.query_map(params![now], |row| {
            Ok(Share {
                id: row.get(0)?,
                display_name: row.get(1)?,
                file_path: row.get(2)?,
                size_bytes: row.get(3)?,
                is_folder: row.get::<_, i64>(4)? != 0,
                created_at: row.get(5)?,
                expires_at: row.get(6)?,
                max_downloads: row.get(7)?,
                download_count: row.get(8)?,
                password_hash: row.get(9)?,
                status: row.get(10)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        out
    };
    let conn = pool.get()?;
    for s in &expired {
        conn.execute(
            "UPDATE shares SET status = 'revoked' WHERE id = ?1",
            params![s.id],
        )?;
    }
    Ok(expired)
}

pub fn record_download_event(
    pool: &Pool,
    share_id: &str,
    ip: &str,
    ts: i64,
    bytes: i64,
    completed: bool,
) -> anyhow::Result<()> {
    let conn = pool.get()?;
    conn.execute(
        "INSERT INTO download_events (share_id, ip_address, timestamp, bytes_transferred, completed)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![share_id, ip, ts, bytes, completed as i64],
    )?;
    Ok(())
}

pub fn record_failed_auth(pool: &Pool, share_id: &str, ip: &str, ts: i64) -> anyhow::Result<()> {
    let conn = pool.get()?;
    conn.execute(
        "INSERT INTO failed_auth_attempts (share_id, ip_address, timestamp) VALUES (?1, ?2, ?3)",
        params![share_id, ip, ts],
    )?;
    Ok(())
}

/// Count failed auth attempts for a given share+IP within the last `window_secs` seconds.
pub fn count_recent_failed_auth(
    pool: &Pool,
    share_id: &str,
    ip: &str,
    since: i64,
) -> anyhow::Result<i64> {
    let conn = pool.get()?;
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM failed_auth_attempts
         WHERE share_id = ?1 AND ip_address = ?2 AND timestamp >= ?3",
        params![share_id, ip, since],
        |row| row.get(0),
    )?;
    Ok(count)
}
