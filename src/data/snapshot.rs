//! Read-only access to a capture snapshot.
//!
//! Snapshots are produced by `VACUUM INTO`: self-contained, `journal_mode=delete`,
//! no `-wal` sidecar. For expired options the data is irreplaceable — IB will
//! never serve it again — so this module opens the file `mode=ro` and there is
//! deliberately no code path that opens it any other way.

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};

/// Where the capture side drops its snapshots.
const SNAPSHOT_DIR: &str = "Documents/Options Database";

/// The lowest schema version whose layout this crate understands. v6 is what
/// added real 30-minute stock bars and the `events` table; everything here
/// assumes both are present.
pub const MIN_SCHEMA_VERSION: i64 = 6;

#[derive(Debug)]
pub struct Snapshot {
    conn: Connection,
    path: PathBuf,
}

impl Snapshot {
    /// Open a snapshot read-only, refusing anything older than [`MIN_SCHEMA_VERSION`].
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            bail!("no snapshot at {}", path.display());
        }
        // The URI form is what actually enforces read-only at the VFS layer;
        // SQLITE_OPEN_READ_ONLY alone would still be honoured, but passing both
        // means a future refactor cannot quietly drop the guarantee by editing
        // one of them.
        let uri = format!("file:{}?mode=ro", path.canonicalize()?.display());
        let conn = Connection::open_with_flags(
            &uri,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )
        .with_context(|| format!("opening snapshot {}", path.display()))?;

        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version < MIN_SCHEMA_VERSION {
            bail!(
                "snapshot {} is schema v{version}; this tool needs v{MIN_SCHEMA_VERSION} or newer \
                 (v6 added the stock bars and events table the backtester reads)",
                path.display()
            );
        }

        Ok(Self {
            conn,
            path: path.to_path_buf(),
        })
    }

    /// Open the newest snapshot in `~/Documents/Options Database`.
    ///
    /// Filenames carry an ISO date (`...-2026-08-26.sqlite3`), so lexicographic
    /// order is chronological order.
    pub fn open_default() -> Result<Self> {
        Self::open(default_snapshot_path()?)
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn schema_version(&self) -> Result<i64> {
        Ok(self.conn.query_row("PRAGMA user_version", [], |row| row.get(0))?)
    }
}

pub fn default_snapshot_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    let dir = PathBuf::from(home).join(SNAPSHOT_DIR);
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(&dir)
        .with_context(|| format!("reading snapshot directory {}", dir.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "sqlite3"))
        .collect();
    candidates.sort();
    candidates
        .pop()
        .with_context(|| format!("no *.sqlite3 snapshot in {}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_a_missing_file() {
        let err = Snapshot::open("/nonexistent/snapshot.sqlite3").unwrap_err();
        assert!(err.to_string().contains("no snapshot at"));
    }

    #[test]
    fn rejects_a_snapshot_older_than_v6() {
        let dir = std::env::temp_dir().join("ib-spread-trader-test-old-schema");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.sqlite3");
        let _ = std::fs::remove_file(&path);
        let writable = Connection::open(&path).unwrap();
        writable.pragma_update(None, "user_version", 5i64).unwrap();
        drop(writable);

        let err = Snapshot::open(&path).unwrap_err();
        assert!(err.to_string().contains("schema v5"), "{err}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn opened_snapshots_reject_writes() {
        let dir = std::env::temp_dir().join("ib-spread-trader-test-readonly");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ro.sqlite3");
        let _ = std::fs::remove_file(&path);
        let writable = Connection::open(&path).unwrap();
        writable.pragma_update(None, "user_version", 6i64).unwrap();
        writable.execute_batch("CREATE TABLE t (a INTEGER)").unwrap();
        drop(writable);

        let snapshot = Snapshot::open(&path).unwrap();
        let err = snapshot
            .conn()
            .execute("INSERT INTO t (a) VALUES (1)", [])
            .unwrap_err();
        assert!(err.to_string().contains("readonly"), "{err}");
        let _ = std::fs::remove_file(&path);
    }
}
