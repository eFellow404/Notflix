//! Portable watch-history JSON export and cross-platform import.

use std::collections::HashMap;

use rusqlite::{params, Transaction};
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::linked_list::EpisodeLinkedList;
use crate::util::{finite_non_negative, utc_now};

use super::schema::{
    load_all_episodes_by_show, load_linked_list, row_to_show, SHOWS_ORDERED_SQL,
    WATCH_HISTORY_EXPORT_VERSION,
};
use super::shows::ShowRecord;
use super::NotflixStore;

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

struct ShowLookup {
    by_path: HashMap<String, usize>,
    by_name: HashMap<String, usize>,
}

impl ShowLookup {
    fn from_shows(shows: &[ShowRecord]) -> Self {
        let mut by_path = HashMap::with_capacity(shows.len());
        let mut by_name = HashMap::with_capacity(shows.len());
        for (idx, show) in shows.iter().enumerate() {
            by_path
                .entry(normalize_path_key(&show.folder_path))
                .or_insert(idx);
            by_name
                .entry(show.name.trim().to_ascii_lowercase())
                .or_insert(idx);
        }
        Self { by_path, by_name }
    }

    fn find(&self, exp_show: &ExportShow) -> Option<usize> {
        self.by_path
            .get(&normalize_path_key(&exp_show.folder_path))
            .copied()
            .or_else(|| {
                self.by_name
                    .get(&exp_show.name.trim().to_ascii_lowercase())
                    .copied()
            })
    }
}

struct EpisodeLookup<'a> {
    by_rel: HashMap<String, &'a str>,
    /// Maps lowercased filename to `Some(id)` when unique within the show, or `None` when
    /// ambiguous (multiple episodes share the same filename).
    by_filename: HashMap<String, Option<&'a str>>,
}

impl<'a> EpisodeLookup<'a> {
    fn from_list(ll: &'a EpisodeLinkedList) -> Self {
        let mut by_rel = HashMap::with_capacity(ll.len());
        let mut by_filename: HashMap<String, Option<&'a str>> = HashMap::with_capacity(ll.len());

        for ep in ll.iter() {
            let id = ep.id.as_str();
            by_rel.insert(normalize_rel_path(&ep.relative_path), id);
            let fn_key = ep.filename.trim().to_ascii_lowercase();
            by_filename
                .entry(fn_key)
                .and_modify(|slot| *slot = None)
                .or_insert(Some(id));
        }

        Self {
            by_rel,
            by_filename,
        }
    }

    fn find_episode_id(&self, exp_ep: &ExportEpisode) -> Option<&'a str> {
        self.by_rel
            .get(&normalize_rel_path(&exp_ep.relative_path))
            .copied()
            .or_else(|| {
                let fn_key = exp_ep.filename.trim().to_ascii_lowercase();
                self.by_filename.get(&fn_key).copied().flatten()
            })
    }

    fn find_current_id(&self, cur_rel: &str) -> Option<&'a str> {
        let norm_cur = normalize_rel_path(cur_rel);
        self.by_rel.get(&norm_cur).copied().or_else(|| {
            let fname = norm_cur.rsplit('/').next().unwrap_or("");
            if fname.is_empty() {
                None
            } else {
                self.by_filename.get(fname).copied().flatten()
            }
        })
    }
}

fn import_show_history(
    tx: &Transaction<'_>,
    local_show: &ShowRecord,
    exp_show: &ExportShow,
) -> Result<(usize, usize)> {
    let local_ll = load_linked_list(tx, local_show)?;
    let lookup = EpisodeLookup::from_list(&local_ll);

    let mut updated = 0usize;
    let mut skipped = 0usize;

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
            let Some(ep_id) = lookup.find_episode_id(exp_ep) else {
                skipped += 1;
                continue;
            };
            let Some(local_ep) = local_ll.get(ep_id) else {
                skipped += 1;
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

            let (intro_start, intro_end, intro_source) = if let Some(start) = exp_ep.intro_start_sec
            {
                (
                    Some(finite_non_negative(start)),
                    exp_ep.intro_end_sec.map(finite_non_negative),
                    exp_ep.intro_source.as_str(),
                )
            } else {
                (
                    local_ep.intro_start_sec,
                    local_ep.intro_end_sec,
                    local_ep.intro_source.as_str(),
                )
            };

            let (outro_start, outro_source) = if let Some(start) = exp_ep.outro_start_sec {
                (
                    Some(finite_non_negative(start)),
                    exp_ep.outro_source.as_str(),
                )
            } else {
                (local_ep.outro_start_sec, local_ep.outro_source.as_str())
            };

            update_ep_stmt.execute(params![
                pos,
                dur,
                i64::from(exp_ep.watched),
                intro_start,
                intro_end,
                intro_source,
                outro_start,
                outro_source,
                ep_id,
                &local_show.id,
            ])?;
            updated += 1;
        }
    }

    if let Some(cur_id) = exp_show
        .current_episode_relative_path
        .as_deref()
        .and_then(|cur_rel| lookup.find_current_id(cur_rel))
    {
        tx.prepare_cached("UPDATE shows SET current_episode_id = ?1 WHERE id = ?2")?
            .execute(params![cur_id, &local_show.id])?;
    }

    if let Some(last_watched) = exp_show
        .last_watched_at
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        tx.prepare_cached("UPDATE shows SET last_watched_at = ?1 WHERE id = ?2")?
            .execute(params![last_watched, &local_show.id])?;
    }

    Ok((updated, skipped))
}

impl NotflixStore {
    /// Export all shows' watch progress, active episode pointers, and intro/outro markers
    /// as a portable JSON-serializable snapshot.
    pub fn export_watch_history(&self) -> Result<WatchHistoryExport> {
        let (shows, mut episodes_by_show) = {
            let conn = self.lock_conn()?;
            let mut show_stmt = conn.prepare_cached(SHOWS_ORDERED_SQL)?;
            let shows: Vec<ShowRecord> = show_stmt
                .query_map([], row_to_show)?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            let episodes_by_show = if shows.is_empty() {
                HashMap::new()
            } else {
                load_all_episodes_by_show(&conn)?
            };
            (shows, episodes_by_show)
        };

        let mut exported_shows = Vec::with_capacity(shows.len());
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
            version: WATCH_HISTORY_EXPORT_VERSION,
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
            .query_map([], row_to_show)?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let show_lookup = ShowLookup::from_shows(&local_shows);

        let mut matched_shows = 0usize;
        let mut unmatched_shows = 0usize;
        let mut updated_episodes = 0usize;
        let mut skipped_episodes = 0usize;

        for exp_show in &export.shows {
            let Some(idx) = show_lookup.find(exp_show) else {
                unmatched_shows += 1;
                skipped_episodes += exp_show.episodes.len();
                continue;
            };

            matched_shows += 1;
            let (updated, skipped) = import_show_history(&tx, &local_shows[idx], exp_show)?;
            updated_episodes += updated;
            skipped_episodes += skipped;
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
