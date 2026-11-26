//! Integration tests for SQLite persistence, linked-list roundtrip, and rescan retention.

use std::collections::HashMap;
use std::fs;

use notflix::intro_detector::IntroSpan;
use notflix::storage::NotflixStore;
use rusqlite::Connection;
use tempfile::tempdir;

#[test]
fn add_persist_progress_and_rescan_preserves_timeplace() {
    let tmp = tempdir().expect("tempdir");
    let base = tmp.path();
    let show_dir = base.join("Firefly");
    fs::create_dir(&show_dir).expect("create show dir");
    for name in [
        "S01E02 - The Train Job.mp4",
        "S01E01 - Serenity.mp4",
        "S01E03 - Bushwhacked.mp4",
    ] {
        fs::write(show_dir.join(name), b"video").expect("write video");
    }

    let db_path = base.join("notflix.db");
    let store = NotflixStore::new(Some(db_path.clone())).expect("create store");

    let (show, ll) = store
        .add_show(show_dir.to_str().unwrap(), None)
        .expect("add show");
    assert_eq!(show.name, "Firefly");
    assert!(ll.validate_invariants());
    let filenames: Vec<&str> = ll.iter().map(|n| n.filename.as_str()).collect();
    assert_eq!(
        filenames,
        vec![
            "S01E01 - Serenity.mp4",
            "S01E02 - The Train Job.mp4",
            "S01E03 - Bushwhacked.mp4",
        ]
    );

    // Save timeplace on episode 1, then advance to episode 2 and save timeplace
    let ep1 = ll.head().expect("head episode");
    let updated1 = store
        .update_progress(&ep1.id, Some(142.5), Some(2500.0), None, None, false)
        .expect("update ep1")
        .expect("ep1 exists");
    assert!((updated1.position_sec - 142.5).abs() < 1e-6);
    assert!(!updated1.watched);

    let (ep2, _) = store
        .advance_next(&show.id)
        .expect("advance_next")
        .expect("ep2 exists");
    assert_eq!(ep2.filename, "S01E02 - The Train Job.mp4");
    store
        .update_progress(&ep2.id, Some(615.0), Some(2600.0), None, None, false)
        .expect("update ep2")
        .expect("ep2 updated");

    // Reopen fresh NotflixStore instance from disk and verify full linked-list recovery
    let store2 = NotflixStore::new(Some(db_path)).expect("reopen store");
    let (_, ll2) = store2
        .get_show(&show.id)
        .expect("get_show")
        .expect("show exists");
    assert!(ll2.validate_invariants());
    let cur = ll2.current().expect("current episode");
    assert_eq!(cur.filename, "S01E02 - The Train Job.mp4");
    assert!((cur.position_sec - 615.0).abs() < 1e-6);
    let prev_node = ll2
        .prev_id_of(&cur.id)
        .and_then(|id| ll2.get(id))
        .expect("prev episode");
    assert!((prev_node.position_sec - 142.5).abs() < 1e-6);

    // Add a new episode file and delete episode 3, then rescan
    fs::write(show_dir.join("S01E04 - Shindig.mp4"), b"video").expect("write ep4");
    fs::remove_file(show_dir.join("S01E03 - Bushwhacked.mp4")).expect("remove ep3");

    let (_, ll3, diff) = store2
        .rescan_show(&show.id, false)
        .expect("rescan")
        .expect("show exists");
    assert!(ll3.validate_invariants());
    assert_eq!(diff.added, 1);
    assert_eq!(diff.removed, 1);
    let filenames3: Vec<&str> = ll3.iter().map(|n| n.filename.as_str()).collect();
    assert_eq!(
        filenames3,
        vec![
            "S01E01 - Serenity.mp4",
            "S01E02 - The Train Job.mp4",
            "S01E04 - Shindig.mp4",
        ]
    );
    // Current episode and saved timestamps must survive rescan
    let cur3 = ll3.current().expect("current after rescan");
    assert_eq!(cur3.filename, "S01E02 - The Train Job.mp4");
    assert!((cur3.position_sec - 615.0).abs() < 1e-6);
}

#[test]
fn manual_reorder_persists_and_reset_restores_smart_order() {
    let tmp = tempdir().expect("tempdir");
    let base = tmp.path();
    let show_dir = base.join("MiniShow");
    fs::create_dir(&show_dir).expect("create show dir");
    for i in [1, 2, 3] {
        fs::write(show_dir.join(format!("Episode {i}.mp4")), b"video").expect("write video");
    }

    let store = NotflixStore::new(Some(base.join("db.sqlite"))).expect("create store");
    let (show, ll) = store
        .add_show(show_dir.to_str().unwrap(), None)
        .expect("add show");
    let ep3_id = ll.tail().expect("tail episode").id.clone();

    let reordered = store
        .reorder_episode(&show.id, &ep3_id, 0)
        .expect("reorder")
        .expect("reordered list");
    assert!(reordered.validate_invariants());
    let names: Vec<&str> = reordered.iter().map(|n| n.filename.as_str()).collect();
    assert_eq!(
        names,
        vec!["Episode 3.mp4", "Episode 1.mp4", "Episode 2.mp4"]
    );

    // Normal rescan preserves manual order
    let (_, ll_kept, _) = store
        .rescan_show(&show.id, false)
        .expect("rescan keep")
        .expect("show exists");
    let kept_names: Vec<&str> = ll_kept.iter().map(|n| n.filename.as_str()).collect();
    assert_eq!(
        kept_names,
        vec!["Episode 3.mp4", "Episode 1.mp4", "Episode 2.mp4"]
    );

    // Reset order restores smart sort
    let (_, ll_reset, _) = store
        .rescan_show(&show.id, true)
        .expect("rescan reset")
        .expect("show exists");
    let reset_names: Vec<&str> = ll_reset.iter().map(|n| n.filename.as_str()).collect();
    assert_eq!(
        reset_names,
        vec!["Episode 1.mp4", "Episode 2.mp4", "Episode 3.mp4"]
    );
}

#[test]
fn intro_persistence_migration_and_rescan_preservation() {
    let tmp = tempdir().expect("tempdir");
    let base = tmp.path();
    let db_path = base.join("legacy.db");

    // Simulate an older Notflix database created before intro columns existed
    {
        let conn = Connection::open(&db_path).expect("open legacy db");
        conn.execute_batch(
            r#"
            CREATE TABLE shows (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                folder_path TEXT NOT NULL UNIQUE,
                head_episode_id TEXT,
                tail_episode_id TEXT,
                current_episode_id TEXT,
                manual_order INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                last_scanned_at TEXT NOT NULL,
                last_watched_at TEXT
            );
            CREATE TABLE episodes (
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
                UNIQUE(show_id, relative_path)
            );
            "#,
        )
        .expect("create legacy schema");
    }

    let show_dir = base.join("IntroShow");
    fs::create_dir(&show_dir).expect("create show dir");
    fs::write(show_dir.join("S01E01.mp4"), b"video").expect("write ep1");
    fs::write(show_dir.join("S01E02.mp4"), b"video").expect("write ep2");

    let store = NotflixStore::new(Some(db_path)).expect("open migrated store");
    let (show, ll) = store
        .add_show(show_dir.to_str().unwrap(), None)
        .expect("add show");
    let eps = ll.to_vec();
    let (ep1, ep2) = (&eps[0], &eps[1]);

    // Manually set ep1 intro and auto-save ep2 intro
    let manual_ep = store
        .set_episode_intro(&ep1.id, Some(12.0), Some(72.0), 1.0, "manual")
        .expect("set manual intro")
        .expect("ep1 exists");
    assert!((manual_ep.intro_start_sec.unwrap_or(0.0) - 12.0).abs() < 1e-6);
    assert_eq!(manual_ep.intro_source, "manual");

    let mut intros = HashMap::new();
    intros.insert(
        ep1.id.clone(),
        IntroSpan {
            start_sec: 30.0,
            end_sec: 90.0,
            confidence: 0.9,
            source: "visual".to_string(),
        },
    );
    intros.insert(
        ep2.id.clone(),
        IntroSpan {
            start_sec: 45.0,
            end_sec: 105.0,
            confidence: 0.92,
            source: "visual".to_string(),
        },
    );

    let (updated_show, updated_ll) = store
        .save_show_intros(&show.id, &intros, true, true)
        .expect("save intros")
        .expect("show exists");
    assert!(updated_show.intro_scanned_at.is_some());
    // Manual intro on ep1 preserved; ep2 got auto-detected intro
    assert!(
        (updated_ll
            .get(&ep1.id)
            .expect("ep1")
            .intro_start_sec
            .unwrap_or(0.0)
            - 12.0)
            .abs()
            < 1e-6
    );
    assert!(
        (updated_ll
            .get(&ep2.id)
            .expect("ep2")
            .intro_start_sec
            .unwrap_or(0.0)
            - 45.0)
            .abs()
            < 1e-6
    );

    // Rescanning folder preserves intro timestamps
    let (_, rescanned_ll, _) = store
        .rescan_show(&show.id, false)
        .expect("rescan")
        .expect("show exists");
    assert!(
        (rescanned_ll
            .get(&ep1.id)
            .expect("ep1 after rescan")
            .intro_end_sec
            .unwrap_or(0.0)
            - 72.0)
            .abs()
            < 1e-6
    );
    assert!(
        (rescanned_ll
            .get(&ep2.id)
            .expect("ep2 after rescan")
            .intro_end_sec
            .unwrap_or(0.0)
            - 105.0)
            .abs()
            < 1e-6
    );
}

#[test]
fn list_shows_returns_accurate_stats_across_multiple_shows() {
    let tmp = tempdir().expect("tempdir");
    let base = tmp.path();

    let show_a_dir = base.join("ShowAlpha");
    fs::create_dir(&show_a_dir).expect("create ShowAlpha");
    for name in [
        "S01E01 - Pilot.mp4",
        "S01E02 - Second.mp4",
        "S02E01 - Return.mp4",
        "Behind The Scenes Extra.mp4",
    ] {
        fs::write(show_a_dir.join(name), b"video").expect("write video");
    }

    let show_b_dir = base.join("ShowBeta");
    fs::create_dir(&show_b_dir).expect("create ShowBeta");
    for name in ["S01E01.mp4", "S01E02.mp4"] {
        fs::write(show_b_dir.join(name), b"video").expect("write video");
    }

    let show_c_dir = base.join("ShowEmpty");
    fs::create_dir(&show_c_dir).expect("create ShowEmpty");

    let store = NotflixStore::new(Some(base.join("multi.db"))).expect("create store");
    let (show_a, ll_a) = store
        .add_show(show_a_dir.to_str().unwrap(), None)
        .expect("add ShowAlpha");
    let (show_b, ll_b) = store
        .add_show(show_b_dir.to_str().unwrap(), None)
        .expect("add ShowBeta");
    let (show_c, _) = store
        .add_show(show_c_dir.to_str().unwrap(), None)
        .expect("add ShowEmpty");

    let eps_a = ll_a.to_vec();
    store
        .update_progress(
            &eps_a[0].id,
            Some(1190.0),
            Some(1200.0),
            Some(true),
            None,
            false,
        )
        .expect("update eps_a[0]");
    store
        .set_episode_intro(&eps_a[0].id, Some(10.0), Some(65.0), 1.0, "manual")
        .expect("set intro");
    store.advance_next(&show_a.id).expect("advance ShowAlpha");

    let eps_b = ll_b.to_vec();
    store
        .update_progress(&eps_b[0].id, Some(50.0), Some(1000.0), None, None, false)
        .expect("update eps_b[0]");

    let listed = store.list_shows().expect("list_shows");
    assert_eq!(listed.len(), 3);
    let by_id: HashMap<&str, _> = listed.iter().map(|s| (s.show.id.as_str(), s)).collect();

    let a = by_id[show_a.id.as_str()];
    assert_eq!(a.episode_count, 4);
    assert_eq!(a.main_episode_count, 3);
    assert_eq!(a.extra_count, 1);
    assert_eq!(a.season_count, 2);
    assert_eq!(a.watched_count, 1);
    assert_eq!(a.intros_detected_count, 1);
    assert_eq!(a.current_episode.as_ref().unwrap().id, eps_a[1].id);
    assert_eq!(a.next_episode.as_ref().unwrap().id, eps_a[2].id);

    let b = by_id[show_b.id.as_str()];
    assert_eq!(b.episode_count, 2);
    assert_eq!(b.main_episode_count, 2);
    assert_eq!(b.extra_count, 0);
    assert_eq!(b.season_count, 1);
    assert_eq!(b.watched_count, 0);
    assert_eq!(b.intros_detected_count, 0);
    assert_eq!(b.current_episode.as_ref().unwrap().id, eps_b[0].id);
    assert_eq!(b.next_episode.as_ref().unwrap().id, eps_b[1].id);

    let c = by_id[show_c.id.as_str()];
    assert_eq!(c.episode_count, 0);
    assert_eq!(c.main_episode_count, 0);
    assert_eq!(c.season_count, 0);
    assert!(c.current_episode.is_none());
    assert!(c.next_episode.is_none());
}
