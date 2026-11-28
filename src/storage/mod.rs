//! SQLite persistence for TV shows, persisted doubly linked lists, and playback state.

mod episodes;
mod history;
mod schema;
mod shows;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::Connection;

use crate::error::{NotflixError, Result};
use crate::util::resolve_path;

pub use history::{ExportEpisode, ExportShow, ImportSummary, WatchHistoryExport};
pub use schema::{default_db_path, WATCHED_THRESHOLD};
pub use shows::{RescanDiff, ShowRecord, ShowSummary};

const DB_BUSY_TIMEOUT: Duration = Duration::from_secs(15);

/// Thread-safe SQLite store persisting shows, episode linked lists, and playback timestamps.
#[derive(Clone)]
pub struct NotflixStore {
    conn: Arc<Mutex<Connection>>,
    db_path: PathBuf,
}

impl NotflixStore {
    /// Open or initialize a Notflix SQLite database at `db_path` (or [`default_db_path`] if `None`).
    pub fn new(db_path: Option<PathBuf>) -> Result<Self> {
        let resolved = match db_path {
            Some(p) => resolve_path(&p),
            None => default_db_path(),
        };

        if let Some(parent) = resolved.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(&resolved)?;
        conn.busy_timeout(DB_BUSY_TIMEOUT)?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;
        conn.execute_batch(schema::SCHEMA_SQL)?;
        schema::migrate_schema(&conn)?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            db_path: resolved,
        })
    }

    /// Return the resolved filesystem path of the SQLite database.
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Return the parent directory containing the SQLite database file.
    pub fn db_dir(&self) -> PathBuf {
        self.db_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    fn lock_conn(&self) -> Result<MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|e| NotflixError::Internal(format!("Database mutex poisoned: {e}")))
    }
}
