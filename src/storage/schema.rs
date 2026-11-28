//! SQLite schema definitions, migrations, row mappers, and shared low-level DB helpers.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension, Row, Transaction};
use uuid::Uuid;

use crate::error::Result;
use crate::intelligence::AnalyzedEpisode;
use crate::linked_list::{EpisodeLinkedList, EpisodeNode};
use crate::util::{env_var_trimmed, resolve_path, utc_now};

use super::shows::ShowRecord;

/// Fraction of an episode's duration at or above which it is automatically marked watched.
pub const WATCHED_THRESHOLD: f64 = 0.90;

pub(super) const MIN_INTRO_DURATION_SEC: f64 = 1.0;
pub(super) const WATCH_HISTORY_EXPORT_VERSION: u32 = 1;

pub(super) const SCHEMA_SQL: &str = r#"
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

pub(super) const MIGRATIONS: &[(&str, &str, &str)] = &[
    ("shows", "intro_scanned_at", "TEXT"),
    ("episodes", "intro_start_sec", "REAL"),
    ("episodes", "intro_end_sec", "REAL"),
    ("episodes", "intro_confidence", "REAL NOT NULL DEFAULT 0.0"),
    ("episodes", "intro_source", "TEXT NOT NULL DEFAULT ''"),
    ("episodes", "outro_start_sec", "REAL"),
    ("episodes", "outro_source", "TEXT NOT NULL DEFAULT ''"),
];

pub(super) const EPISODE_INSERT_SQL: &str = "\
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

pub(super) const SHOWS_ORDERED_SQL: &str = "\
SELECT * FROM shows
ORDER BY
    CASE WHEN last_watched_at IS NULL THEN 1 ELSE 0 END,
    last_watched_at DESC,
    created_at DESC";

pub(super) type ShowEpisodeRows = (Vec<EpisodeNode>, HashMap<String, Option<String>>);

/// Determine the default SQLite database path.
///
/// Honors `NOTFLIX_DB_PATH` first (expanding a leading `~`), preserves backwards compatibility
/// with existing `$HOME/.local/share/notflix/notflix.db` installations, and falls back to the
/// platform-appropriate local data directory on Windows or `$HOME/.local/share/notflix/notflix.db`
/// on Unix.
pub fn default_db_path() -> PathBuf {
    if let Some(env_path) = env_var_trimmed("NOTFLIX_DB_PATH") {
        return resolve_path(Path::new(&env_path));
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

pub(super) fn migrate_schema(conn: &Connection) -> Result<()> {
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

/// Map a `shows` row into a [`ShowRecord`] without touching the filesystem.
///
/// Leaves `folder_exists` as `None` so callers can populate it after dropping the SQLite lock.
pub(super) fn row_to_show(row: &Row<'_>) -> rusqlite::Result<ShowRecord> {
    Ok(ShowRecord {
        id: row.get("id")?,
        name: row.get("name")?,
        folder_path: row.get("folder_path")?,
        head_episode_id: row.get("head_episode_id")?,
        tail_episode_id: row.get("tail_episode_id")?,
        current_episode_id: row.get("current_episode_id")?,
        manual_order: row.get::<_, i64>("manual_order")? != 0,
        created_at: row.get("created_at")?,
        last_scanned_at: row.get("last_scanned_at")?,
        last_watched_at: row.get("last_watched_at")?,
        intro_scanned_at: row.get("intro_scanned_at")?,
        folder_exists: None,
    })
}

pub(super) fn row_to_node(row: &Row<'_>) -> rusqlite::Result<EpisodeNode> {
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

pub(super) fn fetch_show(conn: &Connection, show_id: &str) -> Result<Option<ShowRecord>> {
    let mut stmt = conn.prepare_cached("SELECT * FROM shows WHERE id = ?1")?;
    let show = stmt.query_row(params![show_id], row_to_show).optional()?;
    Ok(show)
}

pub(super) fn fetch_episode(conn: &Connection, episode_id: &str) -> Result<Option<EpisodeNode>> {
    let mut stmt = conn.prepare_cached("SELECT * FROM episodes WHERE id = ?1")?;
    let ep = stmt
        .query_row(params![episode_id], row_to_node)
        .optional()?;
    Ok(ep)
}

pub(super) fn load_linked_list(conn: &Connection, show: &ShowRecord) -> Result<EpisodeLinkedList> {
    let mut stmt = conn.prepare_cached("SELECT * FROM episodes WHERE show_id = ?1")?;
    let mut nodes = Vec::new();
    let mut next_map: HashMap<String, Option<String>> = HashMap::new();

    let rows = stmt.query_map(params![&show.id], |row| {
        let node = row_to_node(row)?;
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

/// Fetch all episodes in a single query grouped by `show_id` to avoid N+1 queries.
pub(super) fn load_all_episodes_by_show(
    conn: &Connection,
) -> Result<HashMap<String, ShowEpisodeRows>> {
    let mut ep_stmt = conn.prepare_cached("SELECT * FROM episodes")?;
    let mut episodes_by_show: HashMap<String, ShowEpisodeRows> = HashMap::new();
    let ep_rows = ep_stmt.query_map([], |row| {
        let node = row_to_node(row)?;
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

    Ok(episodes_by_show)
}

pub(super) fn persist_linked_list(
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
        for (node, prev_id, next_id) in ll.iter_with_links() {
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

    let head_id = ll.head_id();
    let tail_id = ll.tail_id();
    let current_id = ll.current_id();
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

pub(super) fn set_current_episode_pointer(
    conn: &Connection,
    show_id: &str,
    episode_id: &str,
) -> Result<()> {
    conn.prepare_cached(
        "UPDATE shows SET current_episode_id = ?1, last_watched_at = ?2 WHERE id = ?3",
    )?
    .execute(params![episode_id, utc_now(), show_id])?;
    Ok(())
}

pub(super) fn build_node(
    show_id: &str,
    analyzed: AnalyzedEpisode,
    existing: Option<&EpisodeNode>,
) -> EpisodeNode {
    EpisodeNode {
        id: existing
            .map(|e| e.id.clone())
            .unwrap_or_else(|| Uuid::new_v4().to_string()),
        show_id: show_id.to_string(),
        file_path: analyzed.file_path,
        relative_path: analyzed.relative_path,
        filename: analyzed.filename,
        title: analyzed.title,
        season: analyzed.season,
        episode_number: analyzed.episode_number,
        disc: analyzed.disc,
        is_extra: analyzed.is_extra,
        confidence: analyzed.confidence,
        sort_reason: analyzed.sort_reason,
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
