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

/// The research DB's `format_version` this crate reads (ib-option-calc's
/// docs/research-db.md).
pub const RESEARCH_FORMAT_VERSION: i64 = 1;

/// Which layout a snapshot file has. Both are read-only and read through the
/// same loaders; only their SQL differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// A capture snapshot (`VACUUM INTO` of the capture DB): `bars` keyed by
    /// text timestamp, bar size and data type.
    Capture,
    /// A research DB built by ib-option-calc's `scripts/research.py`: option
    /// quote closes as integers, possibly with UW's tape merged in.
    Research,
}

#[derive(Debug)]
pub struct Snapshot {
    conn: Connection,
    path: PathBuf,
    format: Format,
    /// A research DB built with UW quotes v3 or later carries `quotes.age`.
    quote_age: bool,
}

impl Snapshot {
    /// Open a snapshot read-only: a capture snapshot no older than
    /// [`MIN_SCHEMA_VERSION`], or a research DB of [`RESEARCH_FORMAT_VERSION`].
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            bail!("no snapshot at {}", path.display());
        }
        // The URI form is what actually enforces read-only at the VFS layer;
        // SQLITE_OPEN_READ_ONLY alone would still be honoured, but passing both
        // means a future refactor cannot quietly drop the guarantee by editing
        // one of them.
        let uri = format!("{}?mode=ro", file_uri(&path.canonicalize()?));
        let conn = Connection::open_with_flags(
            &uri,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )
        .with_context(|| format!("opening snapshot {}", path.display()))?;

        let research: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'research_meta')",
            [],
            |row| row.get(0),
        )?;
        let format = if research {
            let version: Option<String> = conn
                .query_row(
                    "SELECT value FROM research_meta WHERE key = 'format_version'",
                    [],
                    |row| row.get(0),
                )
                .ok();
            if version.as_deref() != Some(&RESEARCH_FORMAT_VERSION.to_string()) {
                bail!(
                    "research DB {} is format {}; this tool reads format {RESEARCH_FORMAT_VERSION}",
                    path.display(),
                    version.as_deref().unwrap_or("(none)")
                );
            }
            Format::Research
        } else {
            let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if version < MIN_SCHEMA_VERSION {
                bail!(
                    "snapshot {} is schema v{version}; this tool needs v{MIN_SCHEMA_VERSION} or newer \
                     (v6 added the stock bars and events table the backtester reads)",
                    path.display()
                );
            }
            Format::Capture
        };

        let quote_age = format == Format::Research
            && conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM pragma_table_info('quotes') WHERE name = 'age')",
                [],
                |row| row.get(0),
            )?;
        Ok(Self {
            conn,
            path: path.to_path_buf(),
            format,
            quote_age,
        })
    }

    pub fn format(&self) -> Format {
        self.format
    }

    /// Whether the option quotes carry their staleness (`quotes.age`).
    pub fn has_quote_age(&self) -> bool {
        self.quote_age
    }

    /// What a result was computed from, for its report: the file, its
    /// format, and a research DB's build metadata (which snapshot, which UW
    /// quotes, whether tape spreads were corrected).
    pub fn provenance(&self) -> Result<serde_json::Value> {
        let mut out = serde_json::json!({
            "path": self.path.display().to_string(),
            "format": match self.format {
                Format::Capture => "capture",
                Format::Research => "research",
            },
            "schema_version": self.schema_version()?,
        });
        if self.format == Format::Research {
            let mut stmt = self
                .conn
                .prepare("SELECT key, value FROM research_meta ORDER BY key")?;
            let meta: serde_json::Map<String, serde_json::Value> = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .map(|row| row.map(|(k, v)| (k, serde_json::Value::String(v))))
                .collect::<rusqlite::Result<_>>()?;
            out["research"] = serde_json::Value::Object(meta);
        }
        Ok(out)
    }

    /// Open the newest snapshot in `~/Documents/Options Database`, judged by the
    /// ISO date that ends its filename.
    pub fn open_default() -> Result<Self> {
        Self::open(default_snapshot_path()?)
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The capture schema's `user_version`; for a research DB, that of the
    /// snapshot it was built from.
    pub fn schema_version(&self) -> Result<i64> {
        if self.format == Format::Research {
            return Ok(self.conn.query_row(
                "SELECT CAST(value AS INTEGER) FROM research_meta WHERE key = 'snapshot_schema'",
                [],
                |row| row.get(0),
            )?);
        }
        Ok(self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?)
    }
}

/// A `file:` URI SQLite accepts for an absolute path. On Windows `canonicalize`
/// returns a verbatim `\\?\C:\...` path, which SQLite cannot open as a URI, so
/// the prefix goes and the separators turn to `/` (`file:///C:/...`). `%`, `?`
/// and `#` are escaped, since SQLite's URI parser gives them meaning.
fn file_uri(path: &Path) -> String {
    let text = path.to_string_lossy();
    let text = text
        .strip_prefix(r"\\?\")
        .unwrap_or(&text)
        .replace('\\', "/");
    let escaped = text
        .replace('%', "%25")
        .replace('?', "%3F")
        .replace('#', "%23");
    if escaped.starts_with('/') {
        format!("file://{escaped}")
    } else {
        format!("file:///{escaped}")
    }
}

pub fn default_snapshot_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    let dir = PathBuf::from(home).join(SNAPSHOT_DIR);
    let candidates: Vec<PathBuf> = std::fs::read_dir(&dir)
        .with_context(|| format!("reading snapshot directory {}", dir.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "sqlite3"))
        .collect();
    newest(candidates).with_context(|| format!("no *.sqlite3 snapshot in {}", dir.display()))
}

/// The snapshot whose filename ends in the latest date. Sorting whole names is
/// not enough: the weekly capture writes `ib-option-calc-2026-10-01`, which must
/// beat the older `ib-option-calc-snapshot-2026-08-28` — and `s` sorts after `2`.
/// An undated name loses to any dated one.
fn newest(paths: Vec<PathBuf>) -> Option<PathBuf> {
    let date_in_name = |path: &PathBuf| {
        let stem = path.file_stem()?.to_str()?;
        let tail = stem.get(stem.len().checked_sub(10)?..)?;
        chrono::NaiveDate::parse_from_str(tail, "%Y-%m-%d").ok()
    };
    paths
        .into_iter()
        .max_by_key(|path| (date_in_name(path), path.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_newest_snapshot_is_judged_by_the_date_in_its_name() {
        let pick = |names: &[&str]| {
            newest(names.iter().map(PathBuf::from).collect()).map(|p| p.display().to_string())
        };
        assert_eq!(
            pick(&[
                "ib-option-calc-snapshot-2026-08-28.sqlite3",
                "ib-option-calc-2026-10-01.sqlite3",
                "scratch.sqlite3",
            ]),
            Some("ib-option-calc-2026-10-01.sqlite3".into())
        );
        assert_eq!(pick(&["scratch.sqlite3"]), Some("scratch.sqlite3".into()));
        assert_eq!(pick(&[]), None);
    }

    #[test]
    fn file_uris_open_on_windows_and_unix_alike() {
        assert_eq!(
            file_uri(Path::new("/Users/k/Documents/Options Database/a.sqlite3")),
            "file:///Users/k/Documents/Options Database/a.sqlite3"
        );
        assert_eq!(
            file_uri(Path::new(
                r"\\?\C:\Users\Kevin\Documents\Options Database\a.sqlite3"
            )),
            "file:///C:/Users/Kevin/Documents/Options Database/a.sqlite3"
        );
        assert_eq!(
            file_uri(Path::new("/tmp/odd?#%.sqlite3")),
            "file:///tmp/odd%3F%23%25.sqlite3"
        );
    }

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
        writable
            .execute_batch("CREATE TABLE t (a INTEGER)")
            .unwrap();
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
