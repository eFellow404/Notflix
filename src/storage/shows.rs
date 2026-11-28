//! Show CRUD, summary aggregation, and non-destructive folder rescanning.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{NotflixError, Result};
use crate::intelligence::{scan_and_order_folder, AnalyzedEpisode};
use crate::linked_list::{EpisodeJson, EpisodeLinkedList, EpisodeNode};
use crate::util::{resolve_path, utc_now};

use super::schema::{
    build_node, fetch_show, load_all_episodes_by_show, load_linked_list, persist_linked_list,
    row_to_show, ShowEpisodeRows, SHOWS_ORDERED_SQL,
};
use super::NotflixStore;

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

impl ShowRecord {
    pub(super) fn populate_folder_exists(&mut self) {
        self.folder_exists = Some(Path::new(&self.folder_path).is_dir());
    }
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

fn build_show_summary(mut show: ShowRecord, episodes: Option<ShowEpisodeRows>) -> ShowSummary {
    show.populate_folder_exists();

    let (nodes, next_map) = episodes.unwrap_or_else(|| (Vec::new(), HashMap::new()));
    let ll = EpisodeLinkedList::from_persisted(
        nodes,
        &next_map,
        show.head_episode_id.as_deref(),
        show.current_episode_id.as_deref(),
    );

    let episode_count = ll.len();
    let current_id = ll.current_id();
    let mut current_idx = None;
    let mut main_episode_count = 0usize;
    let mut watched_count = 0usize;
    let mut intros_detected_count = 0usize;
    let mut seasons: HashSet<i64> = HashSet::new();
    let mut discs: HashSet<i64> = HashSet::new();

    for (idx, ep) in ll.iter().enumerate() {
        if current_id == Some(ep.id.as_str()) {
            current_idx = Some(idx);
        }
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

    let current_episode = current_id.and_then(|id| ll.node_to_json(id, current_idx));
    let next_episode = ll
        .peek_next()
        .and_then(|n| ll.node_to_json(&n.id, current_idx.map(|i| i + 1)));
    let has_poster = crate::media::find_show_poster(Path::new(&show.folder_path)).is_some();

    ShowSummary {
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
    }
}

fn reconcile_rescanned_episodes(
    show_id: &str,
    existing_ll: &EpisodeLinkedList,
    analyzed: Vec<AnalyzedEpisode>,
    keep_manual: bool,
) -> (EpisodeLinkedList, RescanDiff) {
    let old_by_rel: HashMap<&str, (usize, &EpisodeNode)> = existing_ll
        .iter()
        .enumerate()
        .map(|(idx, n)| (n.relative_path.as_str(), (idx, n)))
        .collect();
    let old_current_rel = existing_ll.current().map(|n| n.relative_path.as_str());

    let new_rel_set: HashSet<&str> = analyzed
        .iter()
        .map(|item| item.relative_path.as_str())
        .collect();

    let added = new_rel_set
        .iter()
        .filter(|r| !old_by_rel.contains_key(**r))
        .count();
    let removed = old_by_rel
        .keys()
        .filter(|r| !new_rel_set.contains(*r))
        .count();
    let unchanged = new_rel_set.len().saturating_sub(added);
    drop(new_rel_set);

    let ordered_nodes: Vec<EpisodeNode> = if keep_manual {
        let mut surviving: Vec<(usize, EpisodeNode)> = Vec::with_capacity(unchanged);
        let mut added_nodes: Vec<EpisodeNode> = Vec::with_capacity(added);
        for item in analyzed {
            if let Some(&(old_idx, old_node)) = old_by_rel.get(item.relative_path.as_str()) {
                surviving.push((old_idx, build_node(show_id, item, Some(old_node))));
            } else {
                added_nodes.push(build_node(show_id, item, None));
            }
        }
        surviving.sort_unstable_by_key(|(idx, _)| *idx);
        surviving
            .into_iter()
            .map(|(_, node)| node)
            .chain(added_nodes)
            .collect()
    } else {
        analyzed
            .into_iter()
            .map(|item| {
                let existing = old_by_rel
                    .get(item.relative_path.as_str())
                    .map(|&(_, node)| node);
                build_node(show_id, item, existing)
            })
            .collect()
    };

    let new_current_id = old_current_rel.and_then(|cur_rel| {
        ordered_nodes
            .iter()
            .find(|node| node.relative_path == cur_rel)
            .map(|node| node.id.clone())
    });

    let new_ll = EpisodeLinkedList::from_ordered_nodes(ordered_nodes, new_current_id.as_deref());
    let diff = RescanDiff {
        added,
        removed,
        unchanged,
        total: new_ll.len(),
    };
    (new_ll, diff)
}

impl NotflixStore {
    /// Scan `folder_path`, build the ordered episode linked list, and insert the show into SQLite.
    pub fn add_show(
        &self,
        folder_path: &str,
        name: Option<&str>,
    ) -> Result<(ShowRecord, EpisodeLinkedList)> {
        let resolved = resolve_path(Path::new(folder_path));
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
            .into_iter()
            .map(|item| build_node(&show_id, item, None))
            .collect();
        let ll = EpisodeLinkedList::from_ordered_nodes(nodes, None);
        persist_linked_list(&tx, &show_id, &ll, Some(false), Some(&now))?;

        let show = fetch_show(&tx, &show_id)?.ok_or_else(|| {
            NotflixError::Internal("Inserted show could not be loaded".to_string())
        })?;
        tx.commit()?;
        Ok((show, ll))
    }

    /// Return all shows ordered by most recently watched then creation time, with aggregate stats.
    pub fn list_shows(&self) -> Result<Vec<ShowSummary>> {
        let (shows, mut episodes_by_show) = {
            let conn = self.lock_conn()?;
            let mut show_stmt = conn.prepare_cached(SHOWS_ORDERED_SQL)?;
            let shows: Vec<ShowRecord> = show_stmt
                .query_map([], row_to_show)?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            if shows.is_empty() {
                return Ok(Vec::new());
            }

            let episodes_by_show = load_all_episodes_by_show(&conn)?;
            (shows, episodes_by_show)
        };

        let mut results = Vec::with_capacity(shows.len());
        for show in shows {
            let episodes = episodes_by_show.remove(&show.id);
            results.push(build_show_summary(show, episodes));
        }

        Ok(results)
    }

    /// Fetch a show record and its full episode linked list by `show_id`.
    pub fn get_show(&self, show_id: &str) -> Result<Option<(ShowRecord, EpisodeLinkedList)>> {
        let maybe_show = {
            let conn = self.lock_conn()?;
            let Some(show) = fetch_show(&conn, show_id)? else {
                return Ok(None);
            };
            let ll = load_linked_list(&conn, &show)?;
            (show, ll)
        };

        let (mut show, ll) = maybe_show;
        show.populate_folder_exists();
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

        let mut updated = {
            let conn = self.lock_conn()?;
            let affected = conn.execute(
                "UPDATE shows SET name = ?1 WHERE id = ?2",
                params![clean, show_id],
            )?;
            if affected == 0 {
                return Ok(None);
            }
            fetch_show(&conn, show_id)?
        };

        if let Some(show) = &mut updated {
            show.populate_folder_exists();
        }
        Ok(updated)
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

        let Some(show) = fetch_show(&tx, show_id)? else {
            return Ok(None);
        };

        let existing_ll = load_linked_list(&tx, &show)?;
        let keep_manual = show.manual_order && !reset_order;
        let (new_ll, diff) =
            reconcile_rescanned_episodes(show_id, &existing_ll, analyzed, keep_manual);

        let now = utc_now();
        persist_linked_list(&tx, show_id, &new_ll, Some(keep_manual), Some(&now))?;

        let updated_show = fetch_show(&tx, show_id)?.ok_or_else(|| {
            NotflixError::Internal("Rescanned show could not be loaded".to_string())
        })?;
        tx.commit()?;
        Ok(Some((updated_show, new_ll, diff)))
    }
}
