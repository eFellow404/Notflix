//! Doubly linked list data structure for ordered TV show episodes.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::error::{NotflixError, Result};

#[inline]
fn round_decimals(value: f64, scale: f64) -> f64 {
    let rounded = (value * scale).round() / scale;
    if rounded == 0.0 {
        0.0
    } else {
        rounded
    }
}

/// A single episode node inside a TV show's doubly linked list.
#[derive(Debug, Clone)]
pub struct EpisodeNode {
    pub id: String,
    pub show_id: String,
    pub file_path: String,
    pub relative_path: String,
    pub filename: String,
    pub title: String,
    pub season: Option<i64>,
    pub episode_number: Option<i64>,
    pub disc: Option<i64>,
    pub is_extra: bool,
    pub confidence: f64,
    pub sort_reason: String,
    pub position_sec: f64,
    pub duration_sec: f64,
    pub watched: bool,
    pub file_size: u64,
    pub intro_start_sec: Option<f64>,
    pub intro_end_sec: Option<f64>,
    pub intro_confidence: f64,
    pub intro_source: String,
    pub outro_start_sec: Option<f64>,
    pub outro_source: String,
}

impl EpisodeNode {
    /// Compute playback progress percentage (0.0..=100.0), rounded to 1 decimal place.
    pub fn progress_percent(&self) -> f64 {
        if self.watched {
            return 100.0;
        }
        if self.duration_sec <= 0.0 {
            return 0.0;
        }
        let pct = ((self.position_sec / self.duration_sec) * 100.0).clamp(0.0, 100.0);
        round_decimals(pct, 10.0)
    }

    /// Format a short episode badge code (e.g., `"S01E02"`, `"D01·T03"`, `"EXTRA"`).
    pub fn display_code(&self) -> String {
        if self.is_extra {
            return "EXTRA".to_string();
        }
        if let (Some(season), Some(episode_number)) = (self.season, self.episode_number) {
            return format!("S{season:02}E{episode_number:02}");
        }
        if let (Some(disc), Some(episode_number)) = (self.disc, self.episode_number) {
            return format!("D{disc:02}·T{episode_number:02}");
        }
        if let Some(episode_number) = self.episode_number {
            return format!("EP {episode_number:02}");
        }
        if let Some(season) = self.season {
            return format!("S{season:02}");
        }
        if let Some(disc) = self.disc {
            return format!("DISC {disc:02}");
        }
        "EP".to_string()
    }

    /// Serialize this episode node into its API JSON representation.
    pub fn to_json(
        &self,
        prev_id: Option<String>,
        next_id: Option<String>,
        index: Option<usize>,
    ) -> EpisodeJson {
        EpisodeJson {
            id: self.id.clone(),
            show_id: self.show_id.clone(),
            file_path: self.file_path.clone(),
            relative_path: self.relative_path.clone(),
            filename: self.filename.clone(),
            title: self.title.clone(),
            season: self.season,
            episode_number: self.episode_number,
            disc: self.disc,
            is_extra: self.is_extra,
            confidence: round_decimals(self.confidence, 100.0),
            sort_reason: self.sort_reason.clone(),
            position_sec: round_decimals(self.position_sec, 100.0),
            duration_sec: round_decimals(self.duration_sec, 100.0),
            watched: self.watched,
            progress_percent: self.progress_percent(),
            file_size: self.file_size,
            intro_start_sec: self.intro_start_sec.map(|v| round_decimals(v, 100.0)),
            intro_end_sec: self.intro_end_sec.map(|v| round_decimals(v, 100.0)),
            intro_confidence: round_decimals(self.intro_confidence, 100.0),
            intro_source: self.intro_source.clone(),
            outro_start_sec: self.outro_start_sec.map(|v| round_decimals(v, 100.0)),
            outro_source: self.outro_source.clone(),
            display_code: self.display_code(),
            prev_id,
            next_id,
            index,
        }
    }
}

/// JSON representation of an [`EpisodeNode`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpisodeJson {
    pub id: String,
    pub show_id: String,
    pub file_path: String,
    pub relative_path: String,
    pub filename: String,
    pub title: String,
    pub season: Option<i64>,
    pub episode_number: Option<i64>,
    pub disc: Option<i64>,
    pub is_extra: bool,
    pub confidence: f64,
    pub sort_reason: String,
    pub position_sec: f64,
    pub duration_sec: f64,
    pub watched: bool,
    pub progress_percent: f64,
    pub file_size: u64,
    pub intro_start_sec: Option<f64>,
    pub intro_end_sec: Option<f64>,
    pub intro_confidence: f64,
    pub intro_source: String,
    pub outro_start_sec: Option<f64>,
    pub outro_source: String,
    pub display_code: String,
    pub prev_id: Option<String>,
    pub next_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
}

/// JSON representation of an [`EpisodeLinkedList`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkedListJson {
    pub head_id: Option<String>,
    pub tail_id: Option<String>,
    pub current_id: Option<String>,
    pub length: usize,
    pub episodes: Vec<EpisodeJson>,
}

#[derive(Debug, Clone)]
struct NodeSlot {
    node: EpisodeNode,
    prev: Option<usize>,
    next: Option<usize>,
}

/// Arena-indexed doubly linked list storing a show's ordered episodes and active pointer.
///
/// Traversal to next/previous episodes is O(1) via arena indices.
/// Freed arena slots are recycled in O(1) via an internal free list.
/// An internal ID index enables O(1) jumping when selecting any episode by ID.
#[derive(Debug, Clone, Default)]
pub struct EpisodeLinkedList {
    slots: Vec<Option<NodeSlot>>,
    free_list: Vec<usize>,
    head: Option<usize>,
    tail: Option<usize>,
    current: Option<usize>,
    index: HashMap<String, usize>,
    length: usize,
}

impl EpisodeLinkedList {
    /// Create a new empty episode linked list.
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the number of episodes in the list.
    pub fn len(&self) -> usize {
        self.length
    }

    /// Return `true` if the list contains no episodes.
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    fn slot(&self, idx: usize) -> Option<&NodeSlot> {
        self.slots.get(idx).and_then(Option::as_ref)
    }

    fn slot_mut(&mut self, idx: usize) -> Option<&mut NodeSlot> {
        self.slots.get_mut(idx).and_then(Option::as_mut)
    }

    fn slot_id(&self, idx: Option<usize>) -> Option<&str> {
        self.slot(idx?).map(|s| s.node.id.as_str())
    }

    fn slot_to_json(&self, slot: &NodeSlot, index: Option<usize>) -> EpisodeJson {
        let prev_id = self.slot_id(slot.prev).map(str::to_string);
        let next_id = self.slot_id(slot.next).map(str::to_string);
        slot.node.to_json(prev_id, next_id, index)
    }

    /// Link an unlinked slot at `idx` between `prev` and `next`, updating `head` and `tail`.
    fn insert_slot_between(&mut self, idx: usize, prev: Option<usize>, next: Option<usize>) {
        if let Some(slot) = self.slot_mut(idx) {
            slot.prev = prev;
            slot.next = next;
        }
        if let Some(prev_idx) = prev {
            if let Some(prev_slot) = self.slot_mut(prev_idx) {
                prev_slot.next = Some(idx);
            }
        } else {
            self.head = Some(idx);
        }
        if let Some(next_idx) = next {
            if let Some(next_slot) = self.slot_mut(next_idx) {
                next_slot.prev = Some(idx);
            }
        } else {
            self.tail = Some(idx);
        }
    }

    /// Return a reference to the head episode node, if any.
    pub fn head(&self) -> Option<&EpisodeNode> {
        self.head.and_then(|i| self.slot(i)).map(|s| &s.node)
    }

    /// Return a reference to the tail episode node, if any.
    pub fn tail(&self) -> Option<&EpisodeNode> {
        self.tail.and_then(|i| self.slot(i)).map(|s| &s.node)
    }

    /// Return a reference to the current active episode node, if any.
    pub fn current(&self) -> Option<&EpisodeNode> {
        self.current.and_then(|i| self.slot(i)).map(|s| &s.node)
    }

    /// Return the ID of the head episode, if any.
    pub fn head_id(&self) -> Option<&str> {
        self.slot_id(self.head)
    }

    /// Return the ID of the tail episode, if any.
    pub fn tail_id(&self) -> Option<&str> {
        self.slot_id(self.tail)
    }

    /// Return the ID of the current active episode, if any.
    pub fn current_id(&self) -> Option<&str> {
        self.slot_id(self.current)
    }

    /// Return the ID of the episode immediately preceding `episode_id`, if any.
    pub fn prev_id_of(&self, episode_id: &str) -> Option<&str> {
        let idx = *self.index.get(episode_id)?;
        self.slot_id(self.slot(idx)?.prev)
    }

    /// Return the ID of the episode immediately following `episode_id`, if any.
    pub fn next_id_of(&self, episode_id: &str) -> Option<&str> {
        let idx = *self.index.get(episode_id)?;
        self.slot_id(self.slot(idx)?.next)
    }

    /// Append an [`EpisodeNode`] to the tail of the doubly linked list.
    ///
    /// Reuses a previously freed arena slot in O(1) when available.
    /// Returns [`NotflixError::Conflict`] if an episode with the same ID already exists.
    pub fn append(&mut self, node: EpisodeNode) -> Result<&EpisodeNode> {
        if self.index.contains_key(&node.id) {
            return Err(NotflixError::Conflict(format!(
                "Episode node {:?} already exists in linked list",
                node.id
            )));
        }

        let id = node.id.clone();
        let new_slot = NodeSlot {
            node,
            prev: None,
            next: None,
        };
        let slot_idx = if let Some(reused_idx) = self.free_list.pop() {
            self.slots[reused_idx] = Some(new_slot);
            reused_idx
        } else {
            let idx = self.slots.len();
            self.slots.push(Some(new_slot));
            idx
        };

        let prev = self.tail;
        self.insert_slot_between(slot_idx, prev, None);
        self.index.insert(id, slot_idx);
        self.length += 1;

        if self.current.is_none() {
            self.current = self.head;
        }

        Ok(&self
            .slot(slot_idx)
            .expect("newly inserted slot must exist")
            .node)
    }

    /// Look up an episode node by ID in O(1).
    pub fn get(&self, episode_id: &str) -> Option<&EpisodeNode> {
        let idx = *self.index.get(episode_id)?;
        self.slot(idx).map(|s| &s.node)
    }

    /// Return the zero-based position of `episode_id` in the list, or `None` if absent.
    pub fn index_of(&self, episode_id: &str) -> Option<usize> {
        let target_idx = *self.index.get(episode_id)?;
        let mut cursor = self.head;
        let mut pos = 0;
        while let Some(slot_idx) = cursor {
            if pos >= self.length {
                break;
            }
            if slot_idx == target_idx {
                return Some(pos);
            }
            cursor = self.slot(slot_idx)?.next;
            pos += 1;
        }
        None
    }

    /// Set the active episode pointer to `episode_id` in O(1).
    pub fn set_current(&mut self, episode_id: &str) -> Option<&EpisodeNode> {
        let idx = *self.index.get(episode_id)?;
        self.current = Some(idx);
        self.slot(idx).map(|s| &s.node)
    }

    /// Peek at the next episode after the active pointer without moving it.
    pub fn peek_next(&self) -> Option<&EpisodeNode> {
        match self.current {
            None => self.head(),
            Some(cur_idx) => {
                let next_idx = self.slot(cur_idx)?.next?;
                self.slot(next_idx).map(|s| &s.node)
            }
        }
    }

    /// Peek at the previous episode before the active pointer without moving it.
    pub fn peek_prev(&self) -> Option<&EpisodeNode> {
        let cur_idx = self.current?;
        let prev_idx = self.slot(cur_idx)?.prev?;
        self.slot(prev_idx).map(|s| &s.node)
    }

    /// Move the active pointer forward by one node (O(1) pointer step).
    pub fn advance_next(&mut self) -> Option<&EpisodeNode> {
        if self.current.is_none() {
            self.current = self.head;
            return self.current();
        }
        let cur_idx = self.current?;
        let next_idx = self.slot(cur_idx)?.next?;
        self.current = Some(next_idx);
        self.current()
    }

    /// Move the active pointer backward by one node (O(1) pointer step).
    pub fn step_prev(&mut self) -> Option<&EpisodeNode> {
        let cur_idx = self.current?;
        let prev_idx = self.slot(cur_idx)?.prev?;
        self.current = Some(prev_idx);
        self.current()
    }

    /// Detach the slot at `idx` from its neighbors and clear its `prev`/`next` links.
    fn unlink(&mut self, idx: usize) {
        let Some((prev, next)) = self.slot(idx).map(|s| (s.prev, s.next)) else {
            return;
        };

        if let Some(prev_idx) = prev {
            if let Some(prev_slot) = self.slot_mut(prev_idx) {
                prev_slot.next = next;
            }
        } else {
            self.head = next;
        }

        if let Some(next_idx) = next {
            if let Some(next_slot) = self.slot_mut(next_idx) {
                next_slot.prev = prev;
            }
        } else {
            self.tail = prev;
        }

        if let Some(slot) = self.slot_mut(idx) {
            slot.prev = None;
            slot.next = None;
        }
    }

    /// Unlink and remove a node by ID while keeping `head`, `tail`, and `current` valid.
    pub fn remove(&mut self, episode_id: &str) -> Option<EpisodeNode> {
        let idx = self.index.remove(episode_id)?;
        let (prev, next) = self.slot(idx).map(|s| (s.prev, s.next))?;

        if self.current == Some(idx) {
            self.current = next.or(prev);
        }

        self.unlink(idx);
        self.length -= 1;
        let removed = self.slots[idx].take()?.node;
        self.free_list.push(idx);
        Some(removed)
    }

    /// Relocate an existing node to `target_index` (clamped to valid bounds) by re-linking pointers.
    pub fn move_node(&mut self, episode_id: &str, target_index: isize) -> bool {
        let Some(&slot_idx) = self.index.get(episode_id) else {
            return false;
        };
        if self.length <= 1 {
            return false;
        }

        let max_idx = self.length - 1;
        let clamped = if target_index <= 0 {
            0
        } else {
            (target_index as usize).min(max_idx)
        };

        if self.index_of(episode_id) == Some(clamped) {
            return true;
        }

        self.unlink(slot_idx);

        let (prev, next) = if clamped == 0 {
            (None, self.head)
        } else if clamped >= max_idx {
            (self.tail, None)
        } else {
            let mut cursor = self.head;
            for _ in 0..clamped {
                cursor = cursor.and_then(|c| self.slot(c)?.next);
            }
            match cursor {
                Some(c) => (self.slot(c).and_then(|s| s.prev), Some(c)),
                None => (self.tail, None),
            }
        };

        self.insert_slot_between(slot_idx, prev, next);
        true
    }

    /// Iterate over episodes in forward order from `head` to `tail`.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &EpisodeNode> {
        ListIter {
            list: self,
            cursor: self.head,
            remaining: self.length,
            reverse: false,
        }
    }

    /// Iterate over episodes in reverse order from `tail` to `head`.
    pub fn reversed_iter(&self) -> impl ExactSizeIterator<Item = &EpisodeNode> {
        ListIter {
            list: self,
            cursor: self.tail,
            remaining: self.length,
            reverse: true,
        }
    }

    /// Iterate over episodes in forward order alongside their `prev_id` and `next_id` links
    /// read directly from arena slots without hash lookups.
    pub(crate) fn iter_with_links(
        &self,
    ) -> impl Iterator<Item = (&EpisodeNode, Option<&str>, Option<&str>)> {
        LinkedIter {
            list: self,
            cursor: self.head,
            remaining: self.length,
        }
    }

    /// Clone all episodes into a `Vec` in forward order.
    pub fn to_vec(&self) -> Vec<EpisodeNode> {
        self.iter().cloned().collect()
    }

    /// Convert a single node in the list to [`EpisodeJson`], populated with its neighbor IDs.
    pub fn node_to_json(&self, episode_id: &str, index: Option<usize>) -> Option<EpisodeJson> {
        let idx = *self.index.get(episode_id)?;
        let slot = self.slot(idx)?;
        Some(self.slot_to_json(slot, index))
    }

    /// Serialize the full linked list and its ordered episodes to [`LinkedListJson`].
    pub fn to_json(&self) -> LinkedListJson {
        let mut episodes = Vec::with_capacity(self.length);
        let mut cursor = self.head;
        let mut idx = 0;

        while let Some(slot_idx) = cursor {
            if idx >= self.length {
                break;
            }
            let Some(slot) = self.slot(slot_idx) else {
                break;
            };
            episodes.push(self.slot_to_json(slot, Some(idx)));
            cursor = slot.next;
            idx += 1;
        }

        LinkedListJson {
            head_id: self.head_id().map(str::to_string),
            tail_id: self.tail_id().map(str::to_string),
            current_id: self.current_id().map(str::to_string),
            length: self.length,
            episodes,
        }
    }

    /// Verify structural integrity of the doubly linked list and arena free list.
    pub fn validate_invariants(&self) -> bool {
        let mut free_seen = HashSet::with_capacity(self.free_list.len());
        for &free_idx in &self.free_list {
            if free_idx >= self.slots.len()
                || self.slots[free_idx].is_some()
                || !free_seen.insert(free_idx)
            {
                return false;
            }
        }
        if self.length + self.free_list.len() != self.slots.len() {
            return false;
        }

        if self.length == 0 {
            return self.head.is_none()
                && self.tail.is_none()
                && self.current.is_none()
                && self.index.is_empty();
        }

        let (Some(head_idx), Some(tail_idx)) = (self.head, self.tail) else {
            return false;
        };
        let (Some(head_slot), Some(tail_slot)) = (self.slot(head_idx), self.slot(tail_idx)) else {
            return false;
        };
        if head_slot.prev.is_some() || tail_slot.next.is_some() {
            return false;
        }

        let mut seen = HashSet::with_capacity(self.length);
        let mut cursor = Some(head_idx);
        let mut prev_idx: Option<usize> = None;

        while let Some(cur_idx) = cursor {
            let Some(slot) = self.slot(cur_idx) else {
                return false;
            };
            if slot.prev != prev_idx {
                return false;
            }
            if self.index.get(&slot.node.id).copied() != Some(cur_idx) {
                return false;
            }
            if !seen.insert(slot.node.id.as_str()) {
                return false;
            }
            prev_idx = Some(cur_idx);
            cursor = slot.next;
        }

        if prev_idx != Some(tail_idx) {
            return false;
        }
        if seen.len() != self.length || self.index.len() != self.length {
            return false;
        }
        if let Some(cur_idx) = self.current {
            let Some(cur_slot) = self.slot(cur_idx) else {
                return false;
            };
            if !seen.contains(cur_slot.node.id.as_str()) {
                return false;
            }
        }
        true
    }

    /// Construct a linked list from an already-ordered sequence of nodes.
    pub fn from_ordered_nodes(nodes: Vec<EpisodeNode>, current_id: Option<&str>) -> Self {
        let mut list = Self {
            slots: Vec::with_capacity(nodes.len()),
            index: HashMap::with_capacity(nodes.len()),
            ..Self::default()
        };
        for node in nodes {
            let _ = list.append(node);
        }
        if let Some(cid) = current_id {
            list.set_current(cid);
        }
        list
    }

    /// Rehydrate the doubly linked list by walking persisted `head -> next` pointers.
    ///
    /// Any orphaned nodes not reachable from `head_id` are appended deterministically
    /// in their original input order.
    pub fn from_persisted(
        nodes: Vec<EpisodeNode>,
        next_map: &HashMap<String, Option<String>>,
        head_id: Option<&str>,
        current_id: Option<&str>,
    ) -> Self {
        let mut original_order = Vec::with_capacity(nodes.len());
        let mut by_id = HashMap::with_capacity(nodes.len());
        for node in nodes {
            original_order.push(node.id.clone());
            by_id.insert(node.id.clone(), node);
        }

        let mut ordered = Vec::with_capacity(by_id.len());
        let mut cursor_id: Option<&str> = head_id;

        while let Some(cid) = cursor_id {
            let Some(node) = by_id.remove(cid) else {
                break;
            };
            ordered.push(node);
            cursor_id = next_map.get(cid).and_then(Option::as_deref);
        }

        // Recover any orphaned nodes deterministically
        if !by_id.is_empty() {
            for id in original_order {
                if let Some(node) = by_id.remove(&id) {
                    ordered.push(node);
                }
            }
        }

        Self::from_ordered_nodes(ordered, current_id)
    }
}

struct ListIter<'a> {
    list: &'a EpisodeLinkedList,
    cursor: Option<usize>,
    remaining: usize,
    reverse: bool,
}

impl<'a> Iterator for ListIter<'a> {
    type Item = &'a EpisodeNode;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let idx = self.cursor?;
        let slot = self.list.slot(idx)?;
        self.cursor = if self.reverse { slot.prev } else { slot.next };
        self.remaining -= 1;
        Some(&slot.node)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for ListIter<'_> {}

struct LinkedIter<'a> {
    list: &'a EpisodeLinkedList,
    cursor: Option<usize>,
    remaining: usize,
}

impl<'a> Iterator for LinkedIter<'a> {
    type Item = (&'a EpisodeNode, Option<&'a str>, Option<&'a str>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let idx = self.cursor?;
        let slot = self.list.slot(idx)?;
        self.cursor = slot.next;
        self.remaining -= 1;
        Some((
            &slot.node,
            self.list.slot_id(slot.prev),
            self.list.slot_id(slot.next),
        ))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for LinkedIter<'_> {}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_node(id: &str) -> EpisodeNode {
        EpisodeNode {
            id: id.to_string(),
            show_id: "show-1".to_string(),
            file_path: format!("/videos/{id}.mp4"),
            relative_path: format!("{id}.mp4"),
            filename: format!("{id}.mp4"),
            title: id.to_string(),
            season: Some(1),
            episode_number: None,
            disc: None,
            is_extra: false,
            confidence: 0.95,
            sort_reason: "test".to_string(),
            position_sec: 0.0,
            duration_sec: 1200.0,
            watched: false,
            file_size: 1024,
            intro_start_sec: None,
            intro_end_sec: None,
            intro_confidence: 0.0,
            intro_source: String::new(),
            outro_start_sec: None,
            outro_source: String::new(),
        }
    }

    #[test]
    fn free_list_reuses_slots_and_preserves_invariants() {
        let mut list = EpisodeLinkedList::new();
        for i in 1..=5 {
            list.append(make_node(&format!("ep{i}"))).unwrap();
        }
        assert_eq!(list.slots.len(), 5);
        assert!(list.free_list.is_empty());
        assert!(list.validate_invariants());

        // Remove head, middle, and tail
        assert!(list.remove("ep1").is_some());
        assert!(list.remove("ep3").is_some());
        assert!(list.remove("ep5").is_some());
        assert_eq!(list.len(), 2);
        assert_eq!(list.slots.len(), 5);
        assert_eq!(list.free_list.len(), 3);
        assert!(list.validate_invariants());

        // Appending 3 nodes must reuse the 3 freed slots without growing the arena
        for i in 6..=8 {
            list.append(make_node(&format!("ep{i}"))).unwrap();
        }
        assert_eq!(list.len(), 5);
        assert_eq!(list.slots.len(), 5);
        assert!(list.free_list.is_empty());
        assert!(list.validate_invariants());

        let forward: Vec<&str> = list.iter().map(|n| n.id.as_str()).collect();
        let reverse: Vec<&str> = list.reversed_iter().map(|n| n.id.as_str()).collect();
        assert_eq!(forward, vec!["ep2", "ep4", "ep6", "ep7", "ep8"]);
        assert_eq!(reverse, vec!["ep8", "ep7", "ep6", "ep4", "ep2"]);
        assert_eq!(list.iter().len(), 5);
        assert_eq!(list.reversed_iter().len(), 5);
        assert_eq!(list.iter().size_hint(), (5, Some(5)));

        // Repeated remove/append cycles keep arena size strictly bounded
        for cycle in 0..50 {
            let remove_id = list.head_id().unwrap().to_string();
            assert!(list.remove(&remove_id).is_some());
            list.append(make_node(&format!("cyc-{cycle}"))).unwrap();
            assert_eq!(list.len(), 5);
            assert_eq!(list.slots.len(), 5);
            assert!(list.free_list.is_empty());
            assert!(list.validate_invariants());
        }

        // Drain completely and repopulate from the free list
        let all_ids: Vec<String> = list.iter().map(|n| n.id.clone()).collect();
        for id in all_ids {
            assert!(list.remove(&id).is_some());
            assert!(list.validate_invariants());
        }
        assert!(list.is_empty());
        assert_eq!(list.slots.len(), 5);
        assert_eq!(list.free_list.len(), 5);

        list.append(make_node("fresh-1")).unwrap();
        list.append(make_node("fresh-2")).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list.slots.len(), 5);
        assert_eq!(list.free_list.len(), 3);
        assert_eq!(list.current_id(), Some("fresh-1"));
        assert!(list.validate_invariants());
    }

    #[test]
    fn iter_with_links_index_of_and_from_persisted_match_structure() {
        let mut next_map = HashMap::new();
        next_map.insert("ep2".to_string(), Some("ep1".to_string()));
        next_map.insert("ep1".to_string(), None);

        let list = EpisodeLinkedList::from_persisted(
            vec![make_node("ep1"), make_node("ep2"), make_node("orphan")],
            &next_map,
            Some("ep2"),
            Some("ep1"),
        );

        let links: Vec<(&str, Option<&str>, Option<&str>)> = list
            .iter_with_links()
            .map(|(n, prev, next)| (n.id.as_str(), prev, next))
            .collect();
        assert_eq!(
            links,
            vec![
                ("ep2", None, Some("ep1")),
                ("ep1", Some("ep2"), Some("orphan")),
                ("orphan", Some("ep1"), None),
            ]
        );
        assert_eq!(list.index_of("ep2"), Some(0));
        assert_eq!(list.index_of("ep1"), Some(1));
        assert_eq!(list.index_of("orphan"), Some(2));
        assert_eq!(list.index_of("missing"), None);
    }
}
