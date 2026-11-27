//! Integration tests for the doubly linked list episode container.

use std::collections::HashMap;

use notflix::error::NotflixError;
use notflix::linked_list::{EpisodeLinkedList, EpisodeNode};

fn make_node(ep_id: &str, ep_num: i64) -> EpisodeNode {
    EpisodeNode {
        id: ep_id.to_string(),
        show_id: "show-1".to_string(),
        file_path: format!("/tv/ep{ep_num}.mp4"),
        relative_path: format!("ep{ep_num}.mp4"),
        filename: format!("ep{ep_num}.mp4"),
        title: format!("Episode {ep_num}"),
        season: Some(1),
        episode_number: Some(ep_num),
        disc: None,
        is_extra: false,
        confidence: 0.0,
        sort_reason: String::new(),
        position_sec: 0.0,
        duration_sec: 0.0,
        watched: false,
        file_size: 0,
        intro_start_sec: None,
        intro_end_sec: None,
        intro_confidence: 0.0,
        intro_source: String::new(),
        outro_start_sec: None,
        outro_source: String::new(),
    }
}

#[test]
fn test_append_and_pointer_traversal() {
    let mut ll = EpisodeLinkedList::new();
    assert!(ll.validate_invariants());
    assert_eq!(ll.len(), 0);
    assert!(ll.is_empty());

    ll.append(make_node("e1", 1)).unwrap();
    ll.append(make_node("e2", 2)).unwrap();
    ll.append(make_node("e3", 3)).unwrap();

    assert!(ll.validate_invariants());
    assert_eq!(ll.len(), 3);
    assert!(!ll.is_empty());
    assert_eq!(ll.head_id(), Some("e1"));
    assert_eq!(ll.tail_id(), Some("e3"));
    assert_eq!(ll.current_id(), Some("e1"));

    assert_eq!(ll.prev_id_of("e1"), None);
    assert_eq!(ll.next_id_of("e1"), Some("e2"));
    assert_eq!(ll.prev_id_of("e2"), Some("e1"));
    assert_eq!(ll.next_id_of("e2"), Some("e3"));
    assert_eq!(ll.prev_id_of("e3"), Some("e2"));
    assert_eq!(ll.next_id_of("e3"), None);

    // O(1) peek / next / prev traversal
    assert!(ll.peek_prev().is_none());
    assert_eq!(ll.peek_next().map(|n| n.id.as_str()), Some("e2"));

    assert_eq!(ll.advance_next().map(|n| n.id.as_str()), Some("e2"));
    assert_eq!(ll.current_id(), Some("e2"));
    assert_eq!(ll.advance_next().map(|n| n.id.as_str()), Some("e3"));
    assert!(ll.advance_next().is_none());
    assert_eq!(ll.current_id(), Some("e3"));

    assert_eq!(ll.step_prev().map(|n| n.id.as_str()), Some("e2"));
    assert_eq!(ll.step_prev().map(|n| n.id.as_str()), Some("e1"));
    assert!(ll.step_prev().is_none());

    let reversed: Vec<&str> = ll.reversed_iter().map(|n| n.id.as_str()).collect();
    assert_eq!(reversed, vec!["e3", "e2", "e1"]);
}

#[test]
fn test_move_node_relinks_pointers() {
    let mut ll = EpisodeLinkedList::from_ordered_nodes(
        vec![
            make_node("e1", 1),
            make_node("e2", 2),
            make_node("e3", 3),
            make_node("e4", 4),
        ],
        None,
    );

    assert!(ll.move_node("e4", 0));
    assert!(ll.validate_invariants());
    assert_eq!(
        ll.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
        vec!["e4", "e1", "e2", "e3"]
    );

    assert!(ll.move_node("e4", 2));
    assert!(ll.validate_invariants());
    assert_eq!(
        ll.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
        vec!["e1", "e2", "e4", "e3"]
    );

    assert!(ll.move_node("e1", 3));
    assert!(ll.validate_invariants());
    assert_eq!(
        ll.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
        vec!["e2", "e4", "e3", "e1"]
    );

    // Out-of-bounds target indices clamp cleanly
    assert!(ll.move_node("e1", -5));
    assert!(ll.validate_invariants());
    assert_eq!(ll.head_id(), Some("e1"));

    assert!(ll.move_node("e1", 99));
    assert!(ll.validate_invariants());
    assert_eq!(ll.tail_id(), Some("e1"));

    // Missing node returns false
    assert!(!ll.move_node("missing", 0));
}

#[test]
fn test_remove_updates_head_tail_and_current() {
    let mut ll = EpisodeLinkedList::from_ordered_nodes(
        vec![make_node("e1", 1), make_node("e2", 2), make_node("e3", 3)],
        Some("e2"),
    );

    let removed = ll.remove("e2");
    assert!(removed.is_some());
    assert!(ll.validate_invariants());
    assert_eq!(
        ll.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
        vec!["e1", "e3"]
    );
    assert_eq!(ll.current_id(), Some("e3"));

    ll.remove("e3");
    assert!(ll.validate_invariants());
    assert_eq!(ll.current_id(), Some("e1"));

    ll.remove("e1");
    assert!(ll.validate_invariants());
    assert_eq!(ll.len(), 0);
    assert!(ll.current().is_none());
}

#[test]
fn test_rehydrate_from_persisted_pointers() {
    let nodes = vec![make_node("e3", 3), make_node("e1", 1), make_node("e2", 2)];
    let mut next_map = HashMap::new();
    next_map.insert("e1".to_string(), Some("e2".to_string()));
    next_map.insert("e2".to_string(), Some("e3".to_string()));
    next_map.insert("e3".to_string(), None);

    let ll = EpisodeLinkedList::from_persisted(nodes, &next_map, Some("e1"), Some("e2"));
    assert!(ll.validate_invariants());
    assert_eq!(
        ll.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
        vec!["e1", "e2", "e3"]
    );
    assert_eq!(ll.current_id(), Some("e2"));
}

#[test]
fn test_rehydrate_recovers_orphaned_nodes() {
    let nodes = vec![
        make_node("e1", 1),
        make_node("e2", 2),
        make_node("orphan-a", 3),
        make_node("orphan-b", 4),
    ];
    let mut next_map = HashMap::new();
    next_map.insert("e1".to_string(), Some("e2".to_string()));
    next_map.insert("e2".to_string(), None);

    let ll = EpisodeLinkedList::from_persisted(nodes, &next_map, Some("e1"), None);
    assert!(ll.validate_invariants());
    assert_eq!(
        ll.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
        vec!["e1", "e2", "orphan-a", "orphan-b"]
    );
}

#[test]
fn test_duplicate_append_rejected_with_conflict() {
    let mut ll = EpisodeLinkedList::new();
    ll.append(make_node("e1", 1)).unwrap();

    let err = ll.append(make_node("e1", 1)).unwrap_err();
    assert!(matches!(err, NotflixError::Conflict(_)));
    assert!(ll.validate_invariants());
    assert_eq!(ll.len(), 1);
}

#[test]
fn test_display_code_formatting_variants() {
    let mut node = make_node("e1", 2);
    node.season = Some(1);
    node.episode_number = Some(2);
    assert_eq!(node.display_code(), "S01E02");

    node.is_extra = true;
    assert_eq!(node.display_code(), "EXTRA");

    node.is_extra = false;
    node.season = None;
    node.disc = Some(3);
    node.episode_number = Some(4);
    assert_eq!(node.display_code(), "D03·T04");

    node.disc = None;
    assert_eq!(node.display_code(), "EP 04");

    node.episode_number = None;
    node.season = Some(5);
    assert_eq!(node.display_code(), "S05");

    node.season = None;
    node.disc = Some(2);
    assert_eq!(node.display_code(), "DISC 02");

    node.disc = None;
    assert_eq!(node.display_code(), "EP");
}

#[test]
fn test_progress_percent_and_json_serialization() {
    let mut node = make_node("e1", 1);
    node.confidence = 0.9876;
    node.position_sec = 33.336;
    node.duration_sec = 100.0;
    node.intro_start_sec = Some(12.346);
    node.intro_end_sec = Some(85.678);
    node.intro_confidence = 0.912;
    node.outro_start_sec = Some(91.236);
    node.outro_source = "chapter".to_string();

    assert_eq!(node.progress_percent(), 33.3);

    let mut ll = EpisodeLinkedList::new();
    ll.append(node).unwrap();

    let json = ll.to_json();
    assert_eq!(json.head_id.as_deref(), Some("e1"));
    assert_eq!(json.tail_id.as_deref(), Some("e1"));
    assert_eq!(json.current_id.as_deref(), Some("e1"));
    assert_eq!(json.length, 1);
    assert_eq!(json.episodes.len(), 1);

    let ep = &json.episodes[0];
    assert_eq!(ep.index, Some(0));
    assert_eq!(ep.confidence, 0.99);
    assert_eq!(ep.position_sec, 33.34);
    assert_eq!(ep.duration_sec, 100.0);
    assert_eq!(ep.progress_percent, 33.3);
    assert_eq!(ep.intro_start_sec, Some(12.35));
    assert_eq!(ep.intro_end_sec, Some(85.68));
    assert_eq!(ep.intro_confidence, 0.91);
    assert_eq!(ep.outro_start_sec, Some(91.24));
    assert_eq!(ep.outro_source, "chapter");
    assert_eq!(ep.display_code, "S01E01");

    // Verify `index` is omitted when None
    let without_index = ll.node_to_json("e1", None).unwrap();
    let serialized = serde_json::to_value(&without_index).unwrap();
    assert!(serialized.get("index").is_none());

    // Watched overrides progress_percent to 100.0
    let mut watched_node = make_node("e2", 2);
    watched_node.watched = true;
    watched_node.duration_sec = 0.0;
    assert_eq!(watched_node.progress_percent(), 100.0);
}
