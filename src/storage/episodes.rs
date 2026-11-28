//! Episode traversal, reordering, progress tracking, and intro/outro marker persistence.

use std::collections::HashMap;

use rusqlite::{params, OptionalExtension};

use crate::error::{NotflixError, Result};
use crate::intro_detector::IntroSpan;
use crate::linked_list::{EpisodeJson, EpisodeLinkedList, EpisodeNode};
use crate::util::{finite_non_negative, finite_unit, utc_now};

use super::schema::{
    fetch_episode, fetch_show, load_linked_list, persist_linked_list, row_to_node,
    set_current_episode_pointer, MIN_INTRO_DURATION_SEC, WATCHED_THRESHOLD,
};
use super::shows::ShowRecord;
use super::NotflixStore;

impl NotflixStore {
    /// Set the active episode pointer for a show and update `last_watched_at`.
    pub fn select_episode(&self, show_id: &str, episode_id: &str) -> Result<Option<EpisodeNode>> {
        let conn = self.lock_conn()?;
        let Some(node) = fetch_episode(&conn, episode_id)?.filter(|ep| ep.show_id == show_id)
        else {
            return Ok(None);
        };
        set_current_episode_pointer(&conn, show_id, &node.id)?;
        Ok(Some(node))
    }

    fn step_episode(
        &self,
        show_id: &str,
        forward: bool,
    ) -> Result<Option<(EpisodeNode, EpisodeLinkedList)>> {
        let conn = self.lock_conn()?;
        let Some(show) = fetch_show(&conn, show_id)? else {
            return Ok(None);
        };
        let mut ll = load_linked_list(&conn, &show)?;
        let maybe_node = if forward {
            ll.advance_next()
        } else {
            ll.step_prev()
        };
        let Some(node) = maybe_node.cloned() else {
            return Ok(None);
        };
        set_current_episode_pointer(&conn, show_id, &node.id)?;
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

        let Some(show) = fetch_show(&tx, show_id)? else {
            return Ok(None);
        };
        let mut ll = load_linked_list(&tx, &show)?;
        if !ll.move_node(episode_id, target_index) {
            return Ok(None);
        }
        persist_linked_list(&tx, show_id, &ll, Some(true), None)?;
        tx.commit()?;
        Ok(Some(ll))
    }

    /// Fetch a single episode node by ID.
    pub fn get_episode(&self, episode_id: &str) -> Result<Option<EpisodeNode>> {
        let conn = self.lock_conn()?;
        fetch_episode(&conn, episode_id)
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
                let node = row_to_node(row)?;
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

        let Some(mut existing) = fetch_episode(&tx, episode_id)? else {
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
            set_current_episode_pointer(&tx, &existing.show_id, episode_id)?;
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

        let Some(show) = fetch_show(&tx, show_id)? else {
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

        let ll = load_linked_list(&tx, &show)?;
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
        let Some(mut node) = fetch_episode(&conn, episode_id)? else {
            return Ok(None);
        };

        match (intro_start_sec, intro_end_sec) {
            (Some(start_raw), Some(end_raw)) => {
                let start = finite_non_negative(start_raw);
                let end = finite_non_negative(end_raw);
                if end <= start + MIN_INTRO_DURATION_SEC {
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
        let result = {
            let mut conn = self.lock_conn()?;
            let tx = conn.transaction()?;

            if fetch_show(&tx, show_id)?.is_none() {
                return Ok(None);
            }

            if !intros.is_empty() {
                let sql = if preserve_manual {
                    "UPDATE episodes
                     SET intro_start_sec = ?1, intro_end_sec = ?2,
                         intro_confidence = ?3, intro_source = ?4
                     WHERE id = ?5 AND show_id = ?6 AND COALESCE(intro_source, '') != 'manual'"
                } else {
                    "UPDATE episodes
                     SET intro_start_sec = ?1, intro_end_sec = ?2,
                         intro_confidence = ?3, intro_source = ?4
                     WHERE id = ?5 AND show_id = ?6"
                };
                let mut update_stmt = tx.prepare_cached(sql)?;

                for (ep_id, span) in intros {
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

            let updated_show = fetch_show(&tx, show_id)?.ok_or_else(|| {
                NotflixError::Internal("Updated show could not be loaded".to_string())
            })?;
            let ll = load_linked_list(&tx, &updated_show)?;
            tx.commit()?;
            (updated_show, ll)
        };

        let (mut updated_show, ll) = result;
        updated_show.populate_folder_exists();
        Ok(Some((updated_show, ll)))
    }

    /// Persist detected outro/credits timestamps for episodes in a show.
    pub fn save_show_outros(
        &self,
        show_id: &str,
        outros: &HashMap<String, IntroSpan>,
        preserve_manual: bool,
    ) -> Result<Option<(ShowRecord, EpisodeLinkedList)>> {
        let result = {
            let mut conn = self.lock_conn()?;
            let tx = conn.transaction()?;

            if fetch_show(&tx, show_id)?.is_none() {
                return Ok(None);
            }

            if !outros.is_empty() {
                let sql = if preserve_manual {
                    "UPDATE episodes
                     SET outro_start_sec = ?1, outro_source = ?2
                     WHERE id = ?3 AND show_id = ?4 AND COALESCE(outro_source, '') != 'manual'"
                } else {
                    "UPDATE episodes
                     SET outro_start_sec = ?1, outro_source = ?2
                     WHERE id = ?3 AND show_id = ?4"
                };
                let mut update_stmt = tx.prepare_cached(sql)?;

                for (ep_id, span) in outros {
                    let start = finite_non_negative(span.start_sec);
                    update_stmt.execute(params![start, &span.source, ep_id, show_id])?;
                }
            }

            let updated_show = fetch_show(&tx, show_id)?.ok_or_else(|| {
                NotflixError::Internal("Updated show could not be loaded".to_string())
            })?;
            let ll = load_linked_list(&tx, &updated_show)?;
            tx.commit()?;
            (updated_show, ll)
        };

        let (mut updated_show, ll) = result;
        updated_show.populate_folder_exists();
        Ok(Some((updated_show, ll)))
    }
}
