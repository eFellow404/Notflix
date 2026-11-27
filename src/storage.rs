//! SQLite persistence for TV shows, persisted doubly linked lists, and playback state.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension, Row, Transaction};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{NotflixError, Result};
use crate::intelligence::{scan_and_order_folder, AnalyzedEpisode};
use crate::intro_detector::IntroSpan;
use crate::linked_list::{EpisodeJson, EpisodeLinkedList, EpisodeNode};
use crate::util::{expand_tilde_str, finite_non_negative, finite_unit, resolve_path, utc_now};

/// Fraction of an episode's duration at or above which it is automatically marked watched.
pub const WATCHED_THRESHOLD: f64 = 0.90;

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS shows (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    folder_path TEXT NOT NULL UNIQUE,
    head_episode_id TEXT,
    tail_episode_id TEXT,
    current_episode_id TEXT,
    manual_order INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    last_scanned_at TEXT NOT NULL,
    last_watched_at TEXT,
    intro_scanned_at TEXT
);

CREATE TABLE IF NOT EXISTS episodes (
    id TEXT PRIMARY KEY,
    show_id TEXT NOT NULL REFERENCES shows(id) ON DELETE CASCADE,
    file_path TEXT NOT NULL,
    relative_path TEXT NOT NULL,
    filename TEXT NOT NULL,
    title TEXT NOT NULL,
    season INTEGER,
    episode_number INTEGER,
    disc INTEGER,
    is_extra INTEGER NOT NULL DEFAULT 0,
    confidence REAL NOT NULL DEFAULT 0.0,
    sort_reason TEXT NOT NULL DEFAULT '',
    prev_episode_id TEXT,
    next_episode_id TEXT,
    position_sec REAL NOT NULL DEFAULT 0.0,
    duration_sec REAL NOT NULL DEFAULT 0.0,
    watched INTEGER NOT NULL DEFAULT 0,
    file_size INTEGER NOT NULL DEFAULT 0,
    intro_start_sec REAL,
    intro_end_sec REAL,
    intro_confidence REAL NOT NULL DEFAULT 0.0,
    intro_source TEXT NOT NULL DEFAULT '',
    outro_start_sec REAL,
    outro_source TEXT NOT NULL DEFAULT '',
    UNIQUE(show_id, relative_path)
);

CREATE INDEX IF NOT EXISTS idx_episodes_show_id ON episodes(show_id);
"#;

const MIGRATIONS: &[(&str, &str, &str)] = &[
    ("shows", "intro_scanned_at", "TEXT"),
    ("episodes", "intro_start_sec", "REAL"),
    ("episodes", "intro_end_sec", "REAL"),
    ("episodes", "intro_confidence", "REAL NOT NULL DEFAULT 0.0"),
    ("episodes", "intro_source", "TEXT NOT NULL DEFAULT ''"),
    ("episodes", "outro_start_sec", "REAL"),
    ("episodes", "outro_source", "TEXT NOT NULL DEFAULT ''"),
];

const EPISODE_INSERT_SQL: &str = "\
INSERT INTO episodes (
    id, show_id, file_path, relative_path, filename, title,
    season, episode_number, disc, is_extra, confidence, sort_reason,
    prev_episode_id, next_episode_id, position_sec, duration_sec,
    watched, file_size, intro_start_sec, intro_end_sec,
    intro_confidence, intro_source, outro_start_sec, outro_source
) VALUES (
    ?1, ?2, ?3, ?4, ?5, ?6,
    ?7, ?8, ?9, ?10, ?11, ?12,
    ?13, ?14, ?15, ?16,
    ?17, ?18, ?19, ?20,
    ?21, ?22, ?23, ?24
)";

/// Persisted TV show metadata record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShowRecord {
    pub id: String,
    pub name: String,
    pub folder_path: String,
    pub head_episode_id: Option<String>,
    pub tail_episode_id: Option<String>,
    pub current_episode_id: Option<String>,
    pub manual_order: bool,
    pub created_at: String,
    pub last_scanned_at: String,
    pub last_watched_at: Option<String>,
    pub intro_scanned_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder_exists: Option<bool>,
}

/// Show metadata combined with aggregate library statistics and current/next episode pointers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShowSummary {
    #[serde(flatten)]
    pub show: ShowRecord,
    pub episode_count: usize,
    pub main_episode_count: usize,
    pub watched_count: usize,
    pub season_count: usize,
    pub disc_count: usize,
    pub extra_count: usize,
    pub intros_detected_count: usize,
    pub has_poster: bool,
    pub current_episode: Option<EpisodeJson>,
    pub next_episode: Option<EpisodeJson>,
}

/// Summary of episode changes detected during a folder rescan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RescanDiff {
    pub added: usize,
    pub removed: usize,
    pub unchanged: usize,
    pub total: usize,
}

/// Portable watch-history snapshot for a single episode.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportEpisode {
    pub relative_path: String,
    pub filename: String,
    pub position_sec: f64,
    pub duration_sec: f64,
    pub watched: bool,
    pub intro_start_sec: Option<f64>,
    pub intro_end_sec: Option<f64>,
    pub intro_source: String,
    pub outro_start_sec: Option<f64>,
    pub outro_source: String,
}

/// Portable watch-history snapshot for a single TV show.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportShow {
    pub name: String,
    pub folder_path: String,
    pub current_episode_relative_path: Option<String>,
    pub last_watched_at: Option<String>,
    pub episodes: Vec<ExportEpisode>,
}

/// Top-level portable watch-history JSON backup format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchHistoryExport {
    pub version: u32,
    pub exported_at: String,
    pub shows: Vec<ExportShow>,
}

/// Result counts returned after importing a [`WatchHistoryExport`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportSummary {
    pub matched_shows: usize,
    pub unmatched_shows: usize,
    pub updated_episodes: usize,
    pub skipped_episodes: usize,
}

fn normalize_path_key(raw: &str) -> String {
    let replaced = raw.trim().replace('\\', "/");
    let trimmed = replaced.trim_end_matches('/');
    if trimmed.is_empty() && !replaced.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_ascii_lowercase()
    }
}

fn normalize_rel_path(raw: &str) -> String {
    let replaced = raw.trim().replace('\\', "/");
    let stripped = replaced.trim_start_matches("./").trim_start_matches('/');
    stripped.to_ascii_lowercase()
}

/// Determine the default SQLite database path.
///
/// Honors `NOTFLIX_DB_PATH` first (expanding a leading `~`), preserves backwards compatibility
/// with existing `$HOME/.local/share/notflix/notflix.db` installations, and falls back to the
/// platform-appropriate local data directory on Windows or `$HOME/.local/share/notflix/notflix.db`
/// on Unix.
pub fn default_db_path() -> PathBuf {
    if let Ok(env_path) = std::env::var("NOTFLIX_DB_PATH") {
        if !env_path.is_empty() {
            return resolve_path(&expand_tilde_str(&env_path));
        }
    }

    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let legacy = home
        .join(".local")
        .join("share")
        .join("notflix")
        .join("notflix.db");
    if legacy.exists() {
        return legacy;
    }

    #[cfg(windows)]
    {
        dirs::data_local_dir()
            .unwrap_or(home)
            .join("notflix")
            .join("notflix.db")
    }

    #[cfg(not(windows))]
    {
        legacy
    }
}

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
        conn.busy_timeout(Duration::from_secs(15))?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;
        conn.execute_batch(SCHEMA_SQL)?;
        Self::migrate_schema(&conn)?;

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

    fn migrate_schema(conn: &Connection) -> Result<()> {
        let mut table_cols: HashMap<&str, HashSet<String>> = HashMap::new();
        for &(table, column, col_def) in MIGRATIONS {
            let cols = match table_cols.get_mut(table) {
                Some(existing) => existing,
                None => {
                    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
                    let names: HashSet<String> = stmt
                        .query_map([], |row| row.get::<_, String>("name"))?
                        .collect::<std::result::Result<HashSet<_>, _>>()?;
                    table_cols.insert(table, names);
                    table_cols.get_mut(table).expect("inserted above")
                }
            };

            if !cols.contains(column) {
                conn.execute(
                    &format!("ALTER TABLE {table} ADD COLUMN {column} {col_def}"),
                    [],
                )?;
                cols.insert(column.to_string());
            }
        }
        Ok(())
    }

    fn row_to_show(row: &Row<'_>, include_folder_exists: bool) -> rusqlite::Result<ShowRecord> {
        let folder_path: String = row.get("folder_path")?;
        let folder_exists = if include_folder_exists {
            Some(Path::new(&folder_path).is_dir())
        } else {
            None
        };
        Ok(ShowRecord {
            id: row.get("id")?,
            name: row.get("name")?,
            folder_path,
            head_episode_id: row.get("head_episode_id")?,
            tail_episode_id: row.get("tail_episode_id")?,
            current_episode_id: row.get("current_episode_id")?,
            manual_order: row.get::<_, i64>("manual_order")? != 0,
            created_at: row.get("created_at")?,
            last_scanned_at: row.get("last_scanned_at")?,
            last_watched_at: row.get("last_watched_at")?,
            intro_scanned_at: row.get("intro_scanned_at")?,
            folder_exists,
        })
    }

    fn row_to_node(row: &Row<'_>) -> rusqlite::Result<EpisodeNode> {
        let file_size_raw: i64 = row.get("file_size")?;
        Ok(EpisodeNode {
            id: row.get("id")?,
            show_id: row.get("show_id")?,
            file_path: row.get("file_path")?,
            relative_path: row.get("relative_path")?,
            filename: row.get("filename")?,
            title: row.get("title")?,
            season: row.get("season")?,
            episode_number: row.get("episode_number")?,
            disc: row.get("disc")?,
            is_extra: row.get::<_, i64>("is_extra")? != 0,
            confidence: row.get("confidence")?,
            sort_reason: row.get("sort_reason")?,
            position_sec: row.get("position_sec")?,
            duration_sec: row.get("duration_sec")?,
            watched: row.get::<_, i64>("watched")? != 0,
            file_size: file_size_raw.max(0) as u64,
            intro_start_sec: row.get("intro_start_sec")?,
            intro_end_sec: row.get("intro_end_sec")?,
            intro_confidence: row
                .get::<_, Option<f64>>("intro_confidence")?
                .unwrap_or(0.0),
            intro_source: row
                .get::<_, Option<String>>("intro_source")?
                .unwrap_or_default(),
            outro_start_sec: row.get("outro_start_sec")?,
            outro_source: row
                .get::<_, Option<String>>("outro_source")?
                .unwrap_or_default(),
        })
    }

    fn fetch_show(
        conn: &Connection,
        show_id: &str,
        include_folder_exists: bool,
    ) -> Result<Option<ShowRecord>> {
        let mut stmt = conn.prepare_cached("SELECT * FROM shows WHERE id = ?1")?;
        let show = stmt
            .query_row(params![show_id], |row| {
                Self::row_to_show(row, include_folder_exists)
            })
            .optional()?;
        Ok(show)
    }

    fn load_linked_list(conn: &Connection, show: &ShowRecord) -> Result<EpisodeLinkedList> {
        let mut stmt = conn.prepare_cached("SELECT * FROM episodes WHERE show_id = ?1")?;
        let mut nodes = Vec::new();
        let mut next_map: HashMap<String, Option<String>> = HashMap::new();

        let rows = stmt.query_map(params![&show.id], |row| {
            let node = Self::row_to_node(row)?;
            let next_id: Option<String> = row.get("next_episode_id")?;
            Ok((node, next_id))
        })?;

        for item in rows {
            let (node, next_id) = item?;
            next_map.insert(node.id.clone(), next_id);
            nodes.push(node);
        }

        Ok(EpisodeLinkedList::from_persisted(
            nodes,
            &next_map,
            show.head_episode_id.as_deref(),
            show.current_episode_id.as_deref(),
        ))
    }

    fn persist_linked_list(
        tx: &Transaction<'_>,
        show_id: &str,
        ll: &EpisodeLinkedList,
        manual_order: Option<bool>,
        scanned_at: Option<&str>,
    ) -> Result<()> {
        tx.prepare_cached("DELETE FROM episodes WHERE show_id = ?1")?
            .execute(params![show_id])?;

        if !ll.is_empty() {
            let mut stmt = tx.prepare_cached(EPISODE_INSERT_SQL)?;
            for node in ll.iter() {
                let prev_id = ll.prev_id_of(&node.id);
                let next_id = ll.next_id_of(&node.id);
                stmt.execute(params![
                    &node.id,
                    show_id,
                    &node.file_path,
                    &node.relative_path,
                    &node.filename,
                    &node.title,
                    node.season,
                    node.episode_number,
                    node.disc,
                    i64::from(node.is_extra),
                    node.confidence,
                    &node.sort_reason,
                    prev_id,
                    next_id,
                    node.position_sec,
                    node.duration_sec,
                    i64::from(node.watched),
                    node.file_size as i64,
                    node.intro_start_sec,
                    node.intro_end_sec,
                    node.intro_confidence,
                    &node.intro_source,
                    node.outro_start_sec,
                    &node.outro_source,
                ])?;
            }
        }

        let head_id = ll.head().map(|n| n.id.as_str());
        let tail_id = ll.tail().map(|n| n.id.as_str());
        let current_id = ll.current().map(|n| n.id.as_str());
        let manual_flag = manual_order.map(i64::from);

        tx.prepare_cached(
            "UPDATE shows
             SET head_episode_id = ?1,
                 tail_episode_id = ?2,
                 current_episode_id = ?3,
                 manual_order = COALESCE(?4, manual_order),
                 last_scanned_at = COALESCE(?5, last_scanned_at)
             WHERE id = ?6",
        )?
        .execute(params![
            head_id,
            tail_id,
            current_id,
            manual_flag,
            scanned_at,
            show_id,
        ])?;

        Ok(())
    }

    fn build_node(
        show_id: &str,
        analyzed: &AnalyzedEpisode,
        existing: Option<&EpisodeNode>,
    ) -> EpisodeNode {
        EpisodeNode {
            id: existing
                .map(|e| e.id.clone())
                .unwrap_or_else(|| Uuid::new_v4().to_string()),
            show_id: show_id.to_string(),
            file_path: analyzed.file_path.clone(),
            relative_path: analyzed.relative_path.clone(),
            filename: analyzed.filename.clone(),
            title: analyzed.title.clone(),
            season: analyzed.season,
            episode_number: analyzed.episode_number,
            disc: analyzed.disc,
            is_extra: analyzed.is_extra,
            confidence: analyzed.confidence,
            sort_reason: analyzed.sort_reason.clone(),
            position_sec: existing.map_or(0.0, |e| e.position_sec),
            duration_sec: existing.map_or(0.0, |e| e.duration_sec),
            watched: existing.is_some_and(|e| e.watched),
            file_size: analyzed.file_size,
            intro_start_sec: existing.and_then(|e| e.intro_start_sec),
            intro_end_sec: existing.and_then(|e| e.intro_end_sec),
            intro_confidence: existing.map_or(0.0, |e| e.intro_confidence),
            intro_source: existing.map(|e| e.intro_source.clone()).unwrap_or_default(),
            outro_start_sec: existing.and_then(|e| e.outro_start_sec),
            outro_source: existing.map(|e| e.outro_source.clone()).unwrap_or_default(),
        }
    }

    /// Scan `folder_path`, build the ordered episode linked list, and insert the show into SQLite.
    pub fn add_show(
        &self,
        folder_path: &str,
        name: Option<&str>,
    ) -> Result<(ShowRecord, EpisodeLinkedList)> {
        let expanded = expand_tilde_str(folder_path);
        let resolved = resolve_path(&expanded);
        if !resolved.is_dir() {
            return Err(NotflixError::NotFound(format!(
                "Folder not found: {folder_path}"
            )));
        }

        let analyzed = scan_and_order_folder(resolved.as_path())?;
        let fallback_name = resolved
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("Show")
            .to_string();
        let show_name = name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or(fallback_name);

        let now = utc_now();
        let resolved_str = resolved.to_string_lossy().into_owned();

        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;

        let existing: Option<String> = tx
            .query_row(
                "SELECT id FROM shows WHERE folder_path = ?1",
                params![&resolved_str],
                |row| row.get(0),
            )
            .optional()?;
        if existing.is_some() {
            return Err(NotflixError::Conflict(format!(
                "Folder is already in your library: {resolved_str}"
            )));
        }

        let show_id = Uuid::new_v4().to_string();
        tx.execute(
            "INSERT INTO shows (
                id, name, folder_path, head_episode_id, tail_episode_id,
                current_episode_id, manual_order, created_at, last_scanned_at, last_watched_at
            ) VALUES (?1, ?2, ?3, NULL, NULL, NULL, 0, ?4, ?5, NULL)",
            params![&show_id, &show_name, &resolved_str, &now, &now],
        )?;

        let nodes: Vec<EpisodeNode> = analyzed
            .iter()
            .map(|item| Self::build_node(&show_id, item, None))
            .collect();
        let ll = EpisodeLinkedList::from_ordered_nodes(nodes, None);
        Self::persist_linked_list(&tx, &show_id, &ll, Some(false), Some(&now))?;

        let show = Self::fetch_show(&tx, &show_id, false)?.ok_or_else(|| {
            NotflixError::Internal("Inserted show could not be loaded".to_string())
        })?;
        tx.commit()?;
        Ok((show, ll))
    }

    /// Return all shows ordered by most recently watched then creation time, with aggregate stats.
    pub fn list_shows(&self) -> Result<Vec<ShowSummary>> {
        let conn = self.lock_conn()?;
        let mut show_stmt = conn.prepare_cached(
            "SELECT * FROM shows
             ORDER BY
                 CASE WHEN last_watched_at IS NULL THEN 1 ELSE 0 END,
                 last_watched_at DESC,
                 created_at DESC",
        )?;
        let shows: Vec<ShowRecord> = show_stmt
            .query_map([], |row| Self::row_to_show(row, true))?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        if shows.is_empty() {
            return Ok(Vec::new());
        }

        // Single query across all episodes avoids N+1 queries for large libraries.
        type ShowEpisodeRows = (Vec<EpisodeNode>, HashMap<String, Option<String>>);
        let mut ep_stmt = conn.prepare_cached("SELECT * FROM episodes")?;
        let mut episodes_by_show: HashMap<String, ShowEpisodeRows> = HashMap::new();
        let ep_rows = ep_stmt.query_map([], |row| {
            let node = Self::row_to_node(row)?;
            let next_id: Option<String> = row.get("next_episode_id")?;
            Ok((node, next_id))
        })?;

        for item in ep_rows {
            let (node, next_id) = item?;
            let entry = episodes_by_show
                .entry(node.show_id.clone())
                .or_insert_with(|| (Vec::new(), HashMap::new()));
            entry.1.insert(node.id.clone(), next_id);
            entry.0.push(node);
        }

        let mut results = Vec::with_capacity(shows.len());
        for show in shows {
            let (nodes, next_map) = episodes_by_show
                .remove(&show.id)
                .unwrap_or_else(|| (Vec::new(), HashMap::new()));
            let ll = EpisodeLinkedList::from_persisted(
                nodes,
                &next_map,
                show.head_episode_id.as_deref(),
                show.current_episode_id.as_deref(),
            );

            let episode_count = ll.len();
            let mut main_episode_count = 0usize;
            let mut watched_count = 0usize;
            let mut intros_detected_count = 0usize;
            let mut seasons: HashSet<i64> = HashSet::new();
            let mut discs: HashSet<i64> = HashSet::new();

            for ep in ll.iter() {
                if !ep.is_extra {
                    main_episode_count += 1;
                    if let Some(s) = ep.season {
                        seasons.insert(s);
                    }
                    if let Some(d) = ep.disc {
                        discs.insert(d);
                    }
                }
                if ep.watched {
                    watched_count += 1;
                }
                if ep.intro_start_sec.is_some() && ep.intro_end_sec.is_some() {
                    intros_detected_count += 1;
                }
            }

            let current_idx = ll.current().and_then(|c| ll.index_of(&c.id));
            let current_episode = ll
                .current()
                .and_then(|c| ll.node_to_json(&c.id, current_idx));
            let next_episode = ll
                .peek_next()
                .and_then(|n| ll.node_to_json(&n.id, current_idx.map(|i| i + 1)));
            let has_poster = crate::media::find_show_poster(Path::new(&show.folder_path)).is_some();

            results.push(ShowSummary {
                show,
                episode_count,
                main_episode_count,
                watched_count,
                season_count: seasons.len(),
                disc_count: discs.len(),
                extra_count: episode_count.saturating_sub(main_episode_count),
                intros_detected_count,
                has_poster,
                current_episode,
                next_episode,
            });
        }

        Ok(results)
    }

    /// Fetch a show record and its full episode linked list by `show_id`.
    pub fn get_show(&self, show_id: &str) -> Result<Option<(ShowRecord, EpisodeLinkedList)>> {
        let conn = self.lock_conn()?;
        let Some(show) = Self::fetch_show(&conn, show_id, true)? else {
            return Ok(None);
        };
        let ll = Self::load_linked_list(&conn, &show)?;
        Ok(Some((show, ll)))
    }

    /// Remove a show (and its cascaded episodes) from the database.
    pub fn delete_show(&self, show_id: &str) -> Result<bool> {
        let conn = self.lock_conn()?;
        let affected = conn.execute("DELETE FROM shows WHERE id = ?1", params![show_id])?;
        Ok(affected > 0)
    }

    /// Rename a show. Returns `BadRequest` if the trimmed name is empty.
    pub fn rename_show(&self, show_id: &str, name: &str) -> Result<Option<ShowRecord>> {
        let clean = name.trim();
        if clean.is_empty() {
            return Err(NotflixError::BadRequest(
                "Show name cannot be empty".to_string(),
            ));
        }

        let conn = self.lock_conn()?;
        let affected = conn.execute(
            "UPDATE shows SET name = ?1 WHERE id = ?2",
            params![clean, show_id],
        )?;
        if affected == 0 {
            return Ok(None);
        }
        Self::fetch_show(&conn, show_id, true)
    }

    /// Re-scan a show's folder for added/removed episodes while preserving timestamps, watched
    /// state, intro markers, current episode pointer, and optional manual order.
    pub fn rescan_show(
        &self,
        show_id: &str,
        reset_order: bool,
    ) -> Result<Option<(ShowRecord, EpisodeLinkedList, RescanDiff)>> {
        // Read folder_path briefly and release the DB lock before scanning disk so concurrent
        // playback/progress requests are never blocked by slow filesystem I/O.
        let folder_path: String = {
            let conn = self.lock_conn()?;
            let mut stmt = conn.prepare_cached("SELECT folder_path FROM shows WHERE id = ?1")?;
            let Some(path) = stmt
                .query_row(params![show_id], |row| row.get(0))
                .optional()?
            else {
                return Ok(None);
            };
            path
        };

        let analyzed = scan_and_order_folder(Path::new(&folder_path))?;

        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;

        let Some(show) = Self::fetch_show(&tx, show_id, false)? else {
            return Ok(None);
        };

        let existing_ll = Self::load_linked_list(&tx, &show)?;
        let old_by_rel: HashMap<String, EpisodeNode> = existing_ll
            .iter()
            .map(|n| (n.relative_path.clone(), n.clone()))
            .collect();
        let old_current_rel = existing_ll.current().map(|n| n.relative_path.clone());

        let new_by_rel: HashMap<&str, &AnalyzedEpisode> = analyzed
            .iter()
            .map(|item| (item.relative_path.as_str(), item))
            .collect();

        let added = new_by_rel
            .keys()
            .filter(|r| !old_by_rel.contains_key(**r))
            .count();
        let removed = old_by_rel
            .keys()
            .filter(|r| !new_by_rel.contains_key(r.as_str()))
            .count();
        let unchanged = new_by_rel.len().saturating_sub(added);

        let keep_manual = show.manual_order && !reset_order;
        let ordered_nodes: Vec<EpisodeNode> = if keep_manual {
            let mut nodes = Vec::with_capacity(analyzed.len());
            for old_node in existing_ll.iter() {
                if let Some(item) = new_by_rel.get(old_node.relative_path.as_str()) {
                    nodes.push(Self::build_node(show_id, item, Some(old_node)));
                }
            }
            for item in &analyzed {
                if !old_by_rel.contains_key(&item.relative_path) {
                    nodes.push(Self::build_node(show_id, item, None));
                }
            }
            nodes
        } else {
            analyzed
                .iter()
                .map(|item| Self::build_node(show_id, item, old_by_rel.get(&item.relative_path)))
                .collect()
        };

        let new_current_id = old_current_rel.as_deref().and_then(|cur_rel| {
            ordered_nodes
                .iter()
                .find(|node| node.relative_path == cur_rel)
                .map(|node| node.id.clone())
        });

        let new_ll =
            EpisodeLinkedList::from_ordered_nodes(ordered_nodes, new_current_id.as_deref());
        let now = utc_now();
        Self::persist_linked_list(&tx, show_id, &new_ll, Some(keep_manual), Some(&now))?;

        let updated_show = Self::fetch_show(&tx, show_id, false)?.ok_or_else(|| {
            NotflixError::Internal("Rescanned show could not be loaded".to_string())
        })?;
        let diff = RescanDiff {
            added,
            removed,
            unchanged,
            total: new_ll.len(),
        };
        tx.commit()?;
        Ok(Some((updated_show, new_ll, diff)))
    }

    /// Set the active episode pointer for a show and update `last_watched_at`.
    pub fn select_episode(&self, show_id: &str, episode_id: &str) -> Result<Option<EpisodeNode>> {
        let conn = self.lock_conn()?;
        let node = conn
            .prepare_cached("SELECT * FROM episodes WHERE id = ?1 AND show_id = ?2")?
            .query_row(params![episode_id, show_id], Self::row_to_node)
            .optional()?;
        let Some(node) = node else {
            return Ok(None);
        };
        conn.prepare_cached(
            "UPDATE shows SET current_episode_id = ?1, last_watched_at = ?2 WHERE id = ?3",
        )?
        .execute(params![&node.id, utc_now(), show_id])?;
        Ok(Some(node))
    }

    fn step_episode(
        &self,
        show_id: &str,
        forward: bool,
    ) -> Result<Option<(EpisodeNode, EpisodeLinkedList)>> {
        let conn = self.lock_conn()?;
        let Some(show) = Self::fetch_show(&conn, show_id, false)? else {
            return Ok(None);
        };
        let mut ll = Self::load_linked_list(&conn, &show)?;
        let maybe_node = if forward {
            ll.advance_next()
        } else {
            ll.step_prev()
        };
        let Some(node) = maybe_node.cloned() else {
            return Ok(None);
        };
        conn.prepare_cached(
            "UPDATE shows SET current_episode_id = ?1, last_watched_at = ?2 WHERE id = ?3",
        )?
        .execute(params![&node.id, utc_now(), show_id])?;
        Ok(Some((node, ll)))
    }

    /// Advance the show's current episode pointer forward by one node.
    pub fn advance_next(&self, show_id: &str) -> Result<Option<(EpisodeNode, EpisodeLinkedList)>> {
        self.step_episode(show_id, true)
    }

    /// Step the show's current episode pointer backward by one node.
    pub fn step_prev(&self, show_id: &str) -> Result<Option<(EpisodeNode, EpisodeLinkedList)>> {
        self.step_episode(show_id, false)
    }

    /// Move an episode to `target_index` in the linked list and mark the show as manually ordered.
    pub fn reorder_episode(
        &self,
        show_id: &str,
        episode_id: &str,
        target_index: isize,
    ) -> Result<Option<EpisodeLinkedList>> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;

        let Some(show) = Self::fetch_show(&tx, show_id, false)? else {
            return Ok(None);
        };
        let mut ll = Self::load_linked_list(&tx, &show)?;
        if !ll.move_node(episode_id, target_index) {
            return Ok(None);
        }
        Self::persist_linked_list(&tx, show_id, &ll, Some(true), None)?;
        tx.commit()?;
        Ok(Some(ll))
    }

    /// Fetch a single episode node by ID.
    pub fn get_episode(&self, episode_id: &str) -> Result<Option<EpisodeNode>> {
        let conn = self.lock_conn()?;
        let ep = conn
            .prepare_cached("SELECT * FROM episodes WHERE id = ?1")?
            .query_row(params![episode_id], Self::row_to_node)
            .optional()?;
        Ok(ep)
    }

    /// Return persisted `(prev_episode_id, next_episode_id)` links for an episode.
    pub fn get_episode_links(&self, episode_id: &str) -> Result<(Option<String>, Option<String>)> {
        let conn = self.lock_conn()?;
        let links = conn
            .prepare_cached("SELECT prev_episode_id, next_episode_id FROM episodes WHERE id = ?1")?
            .query_row(params![episode_id], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?;
        Ok(links.unwrap_or((None, None)))
    }

    /// Fetch an episode serialized with its persisted `prev_id` and `next_id`.
    pub fn get_episode_json(&self, episode_id: &str) -> Result<Option<EpisodeJson>> {
        let conn = self.lock_conn()?;
        let result = conn
            .prepare_cached("SELECT * FROM episodes WHERE id = ?1")?
            .query_row(params![episode_id], |row| {
                let node = Self::row_to_node(row)?;
                let prev_id: Option<String> = row.get("prev_episode_id")?;
                let next_id: Option<String> = row.get("next_episode_id")?;
                Ok(node.to_json(prev_id, next_id, None))
            })
            .optional()?;
        Ok(result)
    }

    /// Serialize an existing [`EpisodeNode`] together with its persisted `prev_id` and `next_id`.
    pub fn episode_to_json_with_links(&self, node: &EpisodeNode) -> Result<EpisodeJson> {
        let (prev_id, next_id) = self.get_episode_links(&node.id)?;
        Ok(node.to_json(prev_id, next_id, None))
    }

    /// Update playback position, duration, and watched status for an episode.
    pub fn update_progress(
        &self,
        episode_id: &str,
        position_sec: Option<f64>,
        duration_sec: Option<f64>,
        watched: Option<bool>,
        set_current: Option<bool>,
        reset_position: bool,
    ) -> Result<Option<EpisodeNode>> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;

        let existing = tx
            .prepare_cached("SELECT * FROM episodes WHERE id = ?1")?
            .query_row(params![episode_id], Self::row_to_node)
            .optional()?;
        let Some(mut existing) = existing else {
            return Ok(None);
        };

        let mut pos = if reset_position {
            0.0
        } else if let Some(p) = position_sec {
            finite_non_negative(p)
        } else {
            existing.position_sec
        };

        let dur = match duration_sec {
            Some(d) if d.is_finite() && d > 0.0 => d,
            _ => existing.duration_sec,
        };

        if dur > 0.0 && pos > dur {
            pos = dur;
        }

        let is_watched = match watched {
            Some(w) => w,
            None => existing.watched || (dur > 0.0 && (pos / dur) >= WATCHED_THRESHOLD),
        };

        tx.prepare_cached(
            "UPDATE episodes SET position_sec = ?1, duration_sec = ?2, watched = ?3 WHERE id = ?4",
        )?
        .execute(params![pos, dur, i64::from(is_watched), episode_id])?;

        let should_set_current = set_current.unwrap_or(position_sec.is_some());
        if should_set_current {
            tx.prepare_cached(
                "UPDATE shows SET current_episode_id = ?1, last_watched_at = ?2 WHERE id = ?3",
            )?
            .execute(params![episode_id, utc_now(), &existing.show_id])?;
        }

        tx.commit()?;

        existing.position_sec = pos;
        existing.duration_sec = dur;
        existing.watched = is_watched;
        Ok(Some(existing))
    }

    /// Bulk mark episodes watched or unwatched without altering the active playback pointer.
    pub fn set_episodes_watched(
        &self,
        show_id: &str,
        episode_ids: &[String],
        watched: bool,
        reset_position: bool,
    ) -> Result<Option<EpisodeLinkedList>> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;

        let Some(show) = Self::fetch_show(&tx, show_id, false)? else {
            return Ok(None);
        };

        if !episode_ids.is_empty() {
            let flag = i64::from(watched);
            let sql = if reset_position {
                "UPDATE episodes SET watched = ?1, position_sec = 0.0 WHERE id = ?2 AND show_id = ?3"
            } else {
                "UPDATE episodes SET watched = ?1 WHERE id = ?2 AND show_id = ?3"
            };
            let mut stmt = tx.prepare_cached(sql)?;
            for ep_id in episode_ids {
                stmt.execute(params![flag, ep_id, show_id])?;
            }
        }

        let ll = Self::load_linked_list(&tx, &show)?;
        tx.commit()?;
        Ok(Some(ll))
    }

    /// Save or clear an episode's intro timestamp range.
    pub fn set_episode_intro(
        &self,
        episode_id: &str,
        intro_start_sec: Option<f64>,
        intro_end_sec: Option<f64>,
        confidence: f64,
        source: &str,
    ) -> Result<Option<EpisodeNode>> {
        let conn = self.lock_conn()?;
        let existing = conn
            .prepare_cached("SELECT * FROM episodes WHERE id = ?1")?
            .query_row(params![episode_id], Self::row_to_node)
            .optional()?;
        let Some(mut node) = existing else {
            return Ok(None);
        };

        match (intro_start_sec, intro_end_sec) {
            (Some(start_raw), Some(end_raw)) => {
                let start = finite_non_negative(start_raw);
                let end = finite_non_negative(end_raw);
                if end <= start + 1.0 {
                    return Err(NotflixError::BadRequest(
                        "intro_end_sec must be greater than intro_start_sec".to_string(),
                    ));
                }
                let conf = finite_unit(confidence);
                conn.prepare_cached(
                    "UPDATE episodes
                     SET intro_start_sec = ?1, intro_end_sec = ?2,
                         intro_confidence = ?3, intro_source = ?4
                     WHERE id = ?5",
                )?
                .execute(params![start, end, conf, source, episode_id])?;

                node.intro_start_sec = Some(start);
                node.intro_end_sec = Some(end);
                node.intro_confidence = conf;
                node.intro_source = source.to_string();
            }
            _ => {
                conn.prepare_cached(
                    "UPDATE episodes
                     SET intro_start_sec = NULL, intro_end_sec = NULL,
                         intro_confidence = 0.0, intro_source = ''
                     WHERE id = ?1",
                )?
                .execute(params![episode_id])?;

                node.intro_start_sec = None;
                node.intro_end_sec = None;
                node.intro_confidence = 0.0;
                node.intro_source.clear();
            }
        }

        Ok(Some(node))
    }

    /// Persist detected intro spans for episodes in a show.
    pub fn save_show_intros(
        &self,
        show_id: &str,
        intros: &HashMap<String, IntroSpan>,
        preserve_manual: bool,
        mark_scanned: bool,
    ) -> Result<Option<(ShowRecord, EpisodeLinkedList)>> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;

        if Self::fetch_show(&tx, show_id, false)?.is_none() {
            return Ok(None);
        }

        if !intros.is_empty() {
            let mut src_stmt =
                tx.prepare_cached("SELECT id, intro_source FROM episodes WHERE show_id = ?1")?;
            let existing_sources: HashMap<String, String> = src_stmt
                .query_map(params![show_id], |row| {
                    let id: String = row.get(0)?;
                    let src: String = row.get::<_, Option<String>>(1)?.unwrap_or_default();
                    Ok((id, src))
                })?
                .collect::<std::result::Result<HashMap<_, _>, _>>()?;

            let mut update_stmt = tx.prepare_cached(
                "UPDATE episodes
                 SET intro_start_sec = ?1, intro_end_sec = ?2,
                     intro_confidence = ?3, intro_source = ?4
                 WHERE id = ?5 AND show_id = ?6",
            )?;

            for (ep_id, span) in intros {
                let Some(existing_source) = existing_sources.get(ep_id) else {
                    continue;
                };
                if preserve_manual && existing_source == "manual" {
                    continue;
                }
                let start = finite_non_negative(span.start_sec);
                let end = finite_non_negative(span.end_sec);
                let conf = finite_unit(span.confidence);
                update_stmt.execute(params![start, end, conf, &span.source, ep_id, show_id])?;
            }
        }

        if mark_scanned {
            tx.prepare_cached("UPDATE shows SET intro_scanned_at = ?1 WHERE id = ?2")?
                .execute(params![utc_now(), show_id])?;
        }

        let updated_show = Self::fetch_show(&tx, show_id, true)?.ok_or_else(|| {
            NotflixError::Internal("Updated show could not be loaded".to_string())
        })?;
        let ll = Self::load_linked_list(&tx, &updated_show)?;
        tx.commit()?;
        Ok(Some((updated_show, ll)))
    }

    /// Persist detected outro/credits timestamps for episodes in a show.
    pub fn save_show_outros(
        &self,
        show_id: &str,
        outros: &HashMap<String, IntroSpan>,
        preserve_manual: bool,
    ) -> Result<Option<(ShowRecord, EpisodeLinkedList)>> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;

        if Self::fetch_show(&tx, show_id, false)?.is_none() {
            return Ok(None);
        }

        if !outros.is_empty() {
            let mut src_stmt =
                tx.prepare_cached("SELECT id, outro_source FROM episodes WHERE show_id = ?1")?;
            let existing_sources: HashMap<String, String> = src_stmt
                .query_map(params![show_id], |row| {
                    let id: String = row.get(0)?;
                    let src: String = row.get::<_, Option<String>>(1)?.unwrap_or_default();
                    Ok((id, src))
                })?
                .collect::<std::result::Result<HashMap<_, _>, _>>()?;

            let mut update_stmt = tx.prepare_cached(
                "UPDATE episodes
                 SET outro_start_sec = ?1, outro_source = ?2
                 WHERE id = ?3 AND show_id = ?4",
            )?;

            for (ep_id, span) in outros {
                let Some(existing_source) = existing_sources.get(ep_id) else {
                    continue;
                };
                if preserve_manual && existing_source == "manual" {
                    continue;
                }
                let start = finite_non_negative(span.start_sec);
                update_stmt.execute(params![start, &span.source, ep_id, show_id])?;
            }
        }

        let updated_show = Self::fetch_show(&tx, show_id, true)?.ok_or_else(|| {
            NotflixError::Internal("Updated show could not be loaded".to_string())
        })?;
        let ll = Self::load_linked_list(&tx, &updated_show)?;
        tx.commit()?;
        Ok(Some((updated_show, ll)))
    }

    /// Export all shows' watch progress, active episode pointers, and intro/outro markers
    /// as a portable JSON-serializable snapshot.
    pub fn export_watch_history(&self) -> Result<WatchHistoryExport> {
        let conn = self.lock_conn()?;
        let mut show_stmt = conn.prepare_cached(
            "SELECT * FROM shows
             ORDER BY
                 CASE WHEN last_watched_at IS NULL THEN 1 ELSE 0 END,
                 last_watched_at DESC,
                 created_at DESC",
        )?;
        let shows: Vec<ShowRecord> = show_stmt
            .query_map([], |row| Self::row_to_show(row, false))?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let mut exported_shows = Vec::with_capacity(shows.len());
        for show in shows {
            let ll = Self::load_linked_list(&conn, &show)?;
            let current_episode_relative_path = ll.current().map(|n| n.relative_path.clone());
            let episodes: Vec<ExportEpisode> = ll
                .iter()
                .map(|ep| ExportEpisode {
                    relative_path: ep.relative_path.clone(),
                    filename: ep.filename.clone(),
                    position_sec: ep.position_sec,
                    duration_sec: ep.duration_sec,
                    watched: ep.watched,
                    intro_start_sec: ep.intro_start_sec,
                    intro_end_sec: ep.intro_end_sec,
                    intro_source: ep.intro_source.clone(),
                    outro_start_sec: ep.outro_start_sec,
                    outro_source: ep.outro_source.clone(),
                })
                .collect();

            exported_shows.push(ExportShow {
                name: show.name,
                folder_path: show.folder_path,
                current_episode_relative_path,
                last_watched_at: show.last_watched_at,
                episodes,
            });
        }

        Ok(WatchHistoryExport {
            version: 1,
            exported_at: utc_now(),
            shows: exported_shows,
        })
    }

    /// Import a portable [`WatchHistoryExport`] into matching shows and episodes in this database.
    ///
    /// Matches shows by normalized `folder_path` first, falling back to case-insensitive `name`
    /// so backups transfer cleanly across operating systems and drive layouts.
    pub fn import_watch_history(&self, export: &WatchHistoryExport) -> Result<ImportSummary> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;

        let local_shows: Vec<ShowRecord> = tx
            .prepare_cached("SELECT * FROM shows")?
            .query_map([], |row| Self::row_to_show(row, false))?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let mut shows_by_path: HashMap<String, usize> = HashMap::with_capacity(local_shows.len());
        let mut shows_by_name: HashMap<String, usize> = HashMap::with_capacity(local_shows.len());
        for (idx, show) in local_shows.iter().enumerate() {
            shows_by_path
                .entry(normalize_path_key(&show.folder_path))
                .or_insert(idx);
            shows_by_name
                .entry(show.name.trim().to_ascii_lowercase())
                .or_insert(idx);
        }

        let mut matched_shows = 0usize;
        let mut unmatched_shows = 0usize;
        let mut updated_episodes = 0usize;
        let mut skipped_episodes = 0usize;

        for exp_show in &export.shows {
            let local_idx = shows_by_path
                .get(&normalize_path_key(&exp_show.folder_path))
                .copied()
                .or_else(|| {
                    shows_by_name
                        .get(&exp_show.name.trim().to_ascii_lowercase())
                        .copied()
                });

            let Some(idx) = local_idx else {
                unmatched_shows += 1;
                skipped_episodes += exp_show.episodes.len();
                continue;
            };

            matched_shows += 1;
            let local_show = &local_shows[idx];
            let local_ll = Self::load_linked_list(&tx, local_show)?;

            let mut ep_id_by_rel: HashMap<String, String> = HashMap::with_capacity(local_ll.len());
            let mut ep_id_by_filename: HashMap<String, String> =
                HashMap::with_capacity(local_ll.len());
            let mut filename_counts: HashMap<String, usize> =
                HashMap::with_capacity(local_ll.len());
            let mut ep_by_id: HashMap<String, EpisodeNode> = HashMap::with_capacity(local_ll.len());

            for ep in local_ll.iter() {
                ep_id_by_rel.insert(normalize_rel_path(&ep.relative_path), ep.id.clone());
                let fn_key = ep.filename.trim().to_ascii_lowercase();
                *filename_counts.entry(fn_key.clone()).or_insert(0) += 1;
                ep_id_by_filename.insert(fn_key, ep.id.clone());
                ep_by_id.insert(ep.id.clone(), ep.clone());
            }

            {
                let mut update_ep_stmt = tx.prepare_cached(
                    "UPDATE episodes
                     SET position_sec = ?1,
                         duration_sec = ?2,
                         watched = ?3,
                         intro_start_sec = ?4,
                         intro_end_sec = ?5,
                         intro_source = ?6,
                         outro_start_sec = ?7,
                         outro_source = ?8
                     WHERE id = ?9 AND show_id = ?10",
                )?;

                for exp_ep in &exp_show.episodes {
                    let matched_ep_id = ep_id_by_rel
                        .get(&normalize_rel_path(&exp_ep.relative_path))
                        .cloned()
                        .or_else(|| {
                            let fn_key = exp_ep.filename.trim().to_ascii_lowercase();
                            if filename_counts.get(&fn_key).copied() == Some(1) {
                                ep_id_by_filename.get(&fn_key).cloned()
                            } else {
                                None
                            }
                        });

                    let Some(ep_id) = matched_ep_id else {
                        skipped_episodes += 1;
                        continue;
                    };
                    let Some(local_ep) = ep_by_id.get(&ep_id) else {
                        skipped_episodes += 1;
                        continue;
                    };

                    let mut pos = finite_non_negative(exp_ep.position_sec);
                    let dur = if exp_ep.duration_sec.is_finite() && exp_ep.duration_sec > 0.0 {
                        exp_ep.duration_sec
                    } else {
                        local_ep.duration_sec
                    };
                    if dur > 0.0 && pos > dur {
                        pos = dur;
                    }

                    let (intro_start, intro_end, intro_source) =
                        if let Some(start) = exp_ep.intro_start_sec {
                            (
                                Some(finite_non_negative(start)),
                                exp_ep.intro_end_sec.map(finite_non_negative),
                                exp_ep.intro_source.clone(),
                            )
                        } else {
                            (
                                local_ep.intro_start_sec,
                                local_ep.intro_end_sec,
                                local_ep.intro_source.clone(),
                            )
                        };

                    let (outro_start, outro_source) = if let Some(start) = exp_ep.outro_start_sec {
                        (
                            Some(finite_non_negative(start)),
                            exp_ep.outro_source.clone(),
                        )
                    } else {
                        (local_ep.outro_start_sec, local_ep.outro_source.clone())
                    };

                    update_ep_stmt.execute(params![
                        pos,
                        dur,
                        i64::from(exp_ep.watched),
                        intro_start,
                        intro_end,
                        &intro_source,
                        outro_start,
                        &outro_source,
                        &ep_id,
                        &local_show.id,
                    ])?;
                    updated_episodes += 1;
                }
            }

            if let Some(ref cur_rel) = exp_show.current_episode_relative_path {
                let norm_cur = normalize_rel_path(cur_rel);
                let matched_cur_id = ep_id_by_rel.get(&norm_cur).cloned().or_else(|| {
                    let fname = norm_cur.rsplit('/').next().unwrap_or("");
                    if !fname.is_empty() && filename_counts.get(fname).copied() == Some(1) {
                        ep_id_by_filename.get(fname).cloned()
                    } else {
                        None
                    }
                });
                if let Some(cur_id) = matched_cur_id {
                    tx.prepare_cached("UPDATE shows SET current_episode_id = ?1 WHERE id = ?2")?
                        .execute(params![cur_id, &local_show.id])?;
                }
            }

            if let Some(last_watched) = exp_show
                .last_watched_at
                .as_deref()
                .filter(|s| !s.trim().is_empty())
            {
                tx.prepare_cached("UPDATE shows SET last_watched_at = ?1 WHERE id = ?2")?
                    .execute(params![last_watched, &local_show.id])?;
            }
        }

        tx.commit()?;
        Ok(ImportSummary {
            matched_shows,
            unmatched_shows,
            updated_episodes,
            skipped_episodes,
        })
    }
}
