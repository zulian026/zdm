//! SQLite registry — same schema as the Python ZDM (`downloads.db`),
//! so history is shared between the two apps.

use rusqlite::{params, Connection};
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize)]
pub struct DownloadRow {
    pub id: i64,
    pub url: String,
    pub filename: Option<String>,
    pub dest_path: Option<String>,
    pub total: Option<i64>,
    pub downloaded: i64,
    pub status: String,
    pub category: String,
    pub created: Option<String>,
    pub updated: Option<String>,
    /// Expected hash hex ("<algo>:<hex>" is not used — the column
    /// holds the hex only; the algorithm is inferred from its
    /// length, 64 = SHA-256, 32 = MD5).
    pub expected_hash: Option<String>,
    /// Hash actually computed from the finished file.
    pub computed_hash: Option<String>,
    /// "unavailable" | "verified" | "mismatch".
    pub verify_state: Option<String>,
    /// True when the task was paused by the daily scheduler (window
    /// ended). Persisted so a restart still knows the pause was the
    /// scheduler's, not the user's — auto-resume treats the two
    /// differently (see `engine::Manager::startup_resume`).
    pub paused_by_schedule: bool,
    /// ClamAV scan outcome: "unavailable" | "scanning" | "clean" |
    /// "found". "scanning" is never persisted (a restart must not
    /// restore a stale scan in progress); the engine only writes the
    /// settled states.
    pub scan_state: Option<String>,
    /// Threat name when a virus was found (`Eicar-Test-Signature`).
    pub threat_name: Option<String>,
    /// Stream kind when this row is a merged stream ("hls" /
    /// "dash"), None for ordinary files.
    pub stream_kind: Option<String>,
    /// Failure reason for failed rows (live stream rejected, merge
    /// failure, ...), so the reason survives a restart.
    pub error: Option<String>,
}

pub fn db_path_for(download_dir: &str) -> PathBuf {
    Path::new(download_dir).join("downloads.db")
}

pub fn init_db(db_path: &Path) -> Result<(), String> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let conn = Connection::open(db_path).map_err(|e| e.to_string())?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS downloads (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            url TEXT NOT NULL, filename TEXT, dest_path TEXT,
            total INTEGER, downloaded INTEGER DEFAULT 0,
            status TEXT DEFAULT 'queued', category TEXT DEFAULT 'General',
            created TEXT, updated TEXT)",
        [],
    )
    .map_err(|e| e.to_string())?;
    // Additive migration: old databases (written before checksum
    // verification existed) gain the new columns as NULLs. Each
    // ALTER fails harmlessly when the column already exists.
    for col in [
        "ALTER TABLE downloads ADD COLUMN expected_hash TEXT",
        "ALTER TABLE downloads ADD COLUMN computed_hash TEXT",
        "ALTER TABLE downloads ADD COLUMN verify_state TEXT",
        "ALTER TABLE downloads ADD COLUMN paused_by_schedule INTEGER DEFAULT 0",
        "ALTER TABLE downloads ADD COLUMN scan_state TEXT",
        "ALTER TABLE downloads ADD COLUMN threat_name TEXT",
        "ALTER TABLE downloads ADD COLUMN stream_kind TEXT",
        "ALTER TABLE downloads ADD COLUMN error TEXT",
    ] {
        let _ = conn.execute(col, []);
    }
    Ok(())
}

pub fn insert_download(
    db_path: &Path,
    url: &str,
    filename: &str,
    dest_path: &str,
    total: Option<i64>,
    category: &str,
) -> Result<i64, String> {
    init_db(db_path)?;
    let conn = Connection::open(db_path).map_err(|e| e.to_string())?;
    let now = chrono::Local::now().to_rfc3339();
    conn.execute(
        "INSERT INTO downloads (url, filename, dest_path, total, downloaded, status, category, created, updated)
         VALUES (?1, ?2, ?3, ?4, 0, 'queued', ?5, ?6, ?6)",
        params![url, filename, dest_path, total, category, now],
    )
    .map_err(|e| e.to_string())?;
    Ok(conn.last_insert_rowid())
}

pub fn update_download(
    db_path: &Path,
    id: i64,
    downloaded: i64,
    total: Option<i64>,
    status: &str,
    paused_by_schedule: bool,
) -> Result<(), String> {
    init_db(db_path)?;
    let conn = Connection::open(db_path).map_err(|e| e.to_string())?;
    let now = chrono::Local::now().to_rfc3339();
    conn.execute(
        "UPDATE downloads SET downloaded=?1, total=?2, status=?3, updated=?4, paused_by_schedule=?5 WHERE id=?6",
        params![downloaded, total, status, now, paused_by_schedule as i64, id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn update_verification(
    db_path: &Path,
    id: i64,
    expected_hash: Option<&str>,
    computed_hash: Option<&str>,
    verify_state: &str,
) -> Result<(), String> {
    init_db(db_path)?;
    let conn = Connection::open(db_path).map_err(|e| e.to_string())?;
    conn.execute(
        "UPDATE downloads SET expected_hash=?1, computed_hash=?2, verify_state=?3 WHERE id=?4",
        params![expected_hash, computed_hash, verify_state, id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Persist a settled ClamAV scan outcome (see `scan.rs`).
pub fn update_scan(
    db_path: &Path,
    id: i64,
    scan_state: &str,
    threat_name: Option<&str>,
) -> Result<(), String> {
    init_db(db_path)?;
    let conn = Connection::open(db_path).map_err(|e| e.to_string())?;
    conn.execute(
        "UPDATE downloads SET scan_state=?1, threat_name=?2 WHERE id=?3",
        params![scan_state, threat_name, id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Persist a stream kind for a row (see `stream.rs`).
pub fn update_stream(db_path: &Path, id: i64, stream_kind: &str) -> Result<(), String> {
    init_db(db_path)?;
    let conn = Connection::open(db_path).map_err(|e| e.to_string())?;
    conn.execute(
        "UPDATE downloads SET stream_kind=?1 WHERE id=?2",
        params![stream_kind, id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Persist a failure reason for a row (see `engine::update_db`).
pub fn update_error(db_path: &Path, id: i64, error: &str) -> Result<(), String> {
    init_db(db_path)?;
    let conn = Connection::open(db_path).map_err(|e| e.to_string())?;
    conn.execute(
        "UPDATE downloads SET error=?1 WHERE id=?2",
        params![error, id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn delete_download(db_path: &Path, id: i64) -> Result<(), String> {
    init_db(db_path)?;
    let conn = Connection::open(db_path).map_err(|e| e.to_string())?;
    conn.execute("DELETE FROM downloads WHERE id=?1", params![id])
        .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn list_downloads(db_path: &Path) -> Result<Vec<DownloadRow>, String> {
    init_db(db_path)?;
    let conn = Connection::open(db_path).map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare(
            "SELECT id, url, filename, dest_path, total, downloaded, status, category, created, updated,
                    expected_hash, computed_hash, verify_state, paused_by_schedule, scan_state, threat_name, stream_kind, error
             FROM downloads ORDER BY created DESC",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |row| {
            Ok(DownloadRow {
                id: row.get(0)?,
                url: row.get(1)?,
                filename: row.get(2)?,
                dest_path: row.get(3)?,
                total: row.get(4)?,
                downloaded: row.get(5)?,
                status: row.get(6)?,
                category: row.get(7)?,
                created: row.get(8)?,
                updated: row.get(9)?,
                expected_hash: row.get(10)?,
                computed_hash: row.get(11)?,
                verify_state: row.get(12)?,
                paused_by_schedule: row.get::<_, i64>(13)? != 0,
                scan_state: row.get(14)?,
                threat_name: row.get(15)?,
                stream_kind: row.get(16)?,
                error: row.get(17)?,
            })
        })
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| e.to_string())?);
    }
    Ok(out)
}

/// Bytes of completed downloads whose `updated` date is today (local).
pub fn downloaded_today(db_path: &Path) -> i64 {
    let Ok(conn) = Connection::open(db_path) else {
        return 0;
    };
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    conn.query_row(
        "SELECT COALESCE(SUM(total),0) FROM downloads
         WHERE status='completed' AND substr(updated,1,10)=?1",
        params![today],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static CTR: AtomicU64 = AtomicU64::new(0);

    fn tmp_db() -> PathBuf {
        let n = CTR.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("zdm-test-db-{n}.db"))
    }

    #[test]
    fn db_round_trip() {
        let path = tmp_db();
        let id = insert_download(&path, "https://example.com/a.bin", "a.bin", "/tmp/a.bin", Some(100), "General").unwrap();
        assert!(id > 0);
        update_download(&path, id, 42, Some(100), "downloading", false).unwrap();
        let rows = list_downloads(&path).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].downloaded, 42);
        assert_eq!(rows[0].status, "downloading");
        assert!(!rows[0].paused_by_schedule);
        // The scheduler-pause flag round-trips through the same
        // UPDATE (old rows read it as false via the DEFAULT 0).
        update_download(&path, id, 42, Some(100), "paused", true).unwrap();
        let rows = list_downloads(&path).unwrap();
        assert!(rows[0].paused_by_schedule);
        update_download(&path, id, 42, Some(100), "paused", false).unwrap();
        assert!(!list_downloads(&path).unwrap()[0].paused_by_schedule);
        // Verification columns migrate in as NULL on old rows and
        // round-trip once written.
        assert!(rows[0].expected_hash.is_none());
        update_verification(&path, id, Some("ab12"), Some("cd34"), "verified").unwrap();
        let rows = list_downloads(&path).unwrap();
        assert_eq!(rows[0].expected_hash.as_deref(), Some("ab12"));
        assert_eq!(rows[0].computed_hash.as_deref(), Some("cd34"));
        assert_eq!(rows[0].verify_state.as_deref(), Some("verified"));
        // Scan columns migrate in as NULL on old rows and round-trip
        // once written.
        assert!(rows[0].scan_state.is_none());
        update_scan(&path, id, "found", Some("Eicar-Test-Signature")).unwrap();
        let rows = list_downloads(&path).unwrap();
        assert_eq!(rows[0].scan_state.as_deref(), Some("found"));
        assert_eq!(rows[0].threat_name.as_deref(), Some("Eicar-Test-Signature"));
        update_scan(&path, id, "clean", None).unwrap();
        let rows = list_downloads(&path).unwrap();
        assert_eq!(rows[0].scan_state.as_deref(), Some("clean"));
        assert!(rows[0].threat_name.is_none());
        // Stream kind migrates in as NULL on old rows and round-trips
        // once written.
        assert!(rows[0].stream_kind.is_none());
        update_stream(&path, id, "hls").unwrap();
        assert_eq!(
            list_downloads(&path).unwrap()[0].stream_kind.as_deref(),
            Some("hls")
        );
        // Failure reason migrates in as NULL and round-trips.
        assert!(rows[0].error.is_none());
        update_error(&path, id, "live stream tidak didukung").unwrap();
        assert_eq!(
            list_downloads(&path).unwrap()[0].error.as_deref(),
            Some("live stream tidak didukung")
        );
        delete_download(&path, id).unwrap();
        assert!(list_downloads(&path).unwrap().is_empty());
        let _ = std::fs::remove_file(&path);
    }
}
