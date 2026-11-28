//! Integration tests for SQLite persistence, linked-list roundtrip, and rescan retention.

mod common;

use std::collections::HashMap;
use std::fs;

use common::create_show_folder;
use notflix::intro_detector::IntroSpan;
use notflix::storage::NotflixStore;
use rusqlite::Connection;
use tempfile::tempdir;

#[test]
fn add_persist_progress_and_rescan_preserves_timeplace() {
    let tmp = tempdir().expect("tempdir");
    let base = tmp.path();
    let show_dir = base.join("Firefly");
    create_show_folder(
        &show_dir,
        &[
            "S01E02 - The Train Job.mp4",
            "S01E01 - Serenity.mp4",
            "S01E03 - Bushwhacked.mp4",
        ],
    );

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
    create_show_folder(
        &show_dir,
        &["Episode 1.mp4", "Episode 2.mp4", "Episode 3.mp4"],
    );

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
    create_show_folder(&show_dir, &["S01E01.mp4", "S01E02.mp4"]);

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
    create_show_folder(
        &show_a_dir,
        &[
            "S01E01 - Pilot.mp4",
            "S01E02 - Second.mp4",
            "S02E01 - Return.mp4",
            "Behind The Scenes Extra.mp4",
        ],
    );

    let show_b_dir = base.join("ShowBeta");
    create_show_folder(&show_b_dir, &["S01E01.mp4", "S01E02.mp4"]);

    let show_c_dir = base.join("ShowEmpty");
    create_show_folder(&show_c_dir, &[]);

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

#[test]
fn outro_migration_save_preserve_manual_and_rescan() {
    let tmp = tempdir().expect("tempdir");
    let base = tmp.path();
    let db_path = base.join("legacy_outro.db");

    // Simulate a database created before outro_start_sec / outro_source existed
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
                last_watched_at TEXT,
                intro_scanned_at TEXT
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
                intro_start_sec REAL,
                intro_end_sec REAL,
                intro_confidence REAL NOT NULL DEFAULT 0.0,
                intro_source TEXT NOT NULL DEFAULT '',
                UNIQUE(show_id, relative_path)
            );
            INSERT INTO shows (
                id, name, folder_path, head_episode_id, tail_episode_id,
                current_episode_id, manual_order, created_at, last_scanned_at
            ) VALUES (
                'legacy-show', 'Legacy Show', '/legacy/path', 'legacy-ep1', 'legacy-ep1',
                'legacy-ep1', 0, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z'
            );
            INSERT INTO episodes (
                id, show_id, file_path, relative_path, filename, title,
                season, episode_number, position_sec, duration_sec, watched
            ) VALUES (
                'legacy-ep1', 'legacy-show', '/legacy/path/S01E01.mp4', 'S01E01.mp4',
                'S01E01.mp4', 'Pilot', 1, 1, 321.5, 1400.0, 0
            );
            "#,
        )
        .expect("seed legacy db");
    }

    let store = NotflixStore::new(Some(db_path)).expect("open migrated store");
    assert_eq!(store.db_dir(), base);

    // Existing row survived migration intact with default outro fields
    let (_, legacy_ll) = store
        .get_show("legacy-show")
        .expect("get legacy show")
        .expect("legacy show exists");
    let legacy_ep = legacy_ll.get("legacy-ep1").expect("legacy ep1");
    assert!((legacy_ep.position_sec - 321.5).abs() < 1e-6);
    assert!(legacy_ep.outro_start_sec.is_none());
    assert!(legacy_ep.outro_source.is_empty());

    // Add a real folder show and verify save_show_outros + preserve_manual + rescan preservation
    let show_dir = base.join("OutroShow");
    create_show_folder(&show_dir, &["S01E01.mp4", "S01E02.mp4"]);

    let (show, ll) = store
        .add_show(show_dir.to_str().unwrap(), None)
        .expect("add OutroShow");
    let eps = ll.to_vec();
    let (ep1, ep2) = (&eps[0], &eps[1]);

    // First save a manual outro on ep1
    let mut manual_map = HashMap::new();
    manual_map.insert(
        ep1.id.clone(),
        IntroSpan {
            start_sec: 1210.0,
            end_sec: 1320.0,
            confidence: 1.0,
            source: "manual".to_string(),
        },
    );
    store
        .save_show_outros(&show.id, &manual_map, false)
        .expect("save manual outro")
        .expect("show exists");

    // Auto-detected outros for both ep1 and ep2 with preserve_manual = true
    let mut auto_map = HashMap::new();
    auto_map.insert(
        ep1.id.clone(),
        IntroSpan {
            start_sec: 1180.0,
            end_sec: 1320.0,
            confidence: 0.96,
            source: "chapter".to_string(),
        },
    );
    auto_map.insert(
        ep2.id.clone(),
        IntroSpan {
            start_sec: 1245.5,
            end_sec: 1350.0,
            confidence: 0.91,
            source: "audio".to_string(),
        },
    );
    let (_, after_auto) = store
        .save_show_outros(&show.id, &auto_map, true)
        .expect("save auto outros")
        .expect("show exists");

    let ep1_after = after_auto.get(&ep1.id).expect("ep1");
    assert_eq!(ep1_after.outro_start_sec, Some(1210.0));
    assert_eq!(ep1_after.outro_source, "manual");

    let ep2_after = after_auto.get(&ep2.id).expect("ep2");
    assert_eq!(ep2_after.outro_start_sec, Some(1245.5));
    assert_eq!(ep2_after.outro_source, "audio");

    // Rescan preserves outro markers
    let (_, rescanned, _) = store
        .rescan_show(&show.id, false)
        .expect("rescan")
        .expect("show exists");
    assert_eq!(
        rescanned.get(&ep1.id).unwrap().outro_start_sec,
        Some(1210.0)
    );
    assert_eq!(rescanned.get(&ep1.id).unwrap().outro_source, "manual");
    assert_eq!(
        rescanned.get(&ep2.id).unwrap().outro_start_sec,
        Some(1245.5)
    );
    assert_eq!(rescanned.get(&ep2.id).unwrap().outro_source, "audio");

    // preserve_manual = false overwrites manual outro
    let (_, after_force) = store
        .save_show_outros(&show.id, &auto_map, false)
        .expect("force save outros")
        .expect("show exists");
    assert_eq!(
        after_force.get(&ep1.id).unwrap().outro_start_sec,
        Some(1180.0)
    );
    assert_eq!(after_force.get(&ep1.id).unwrap().outro_source, "chapter");
}

#[test]
fn watch_history_export_and_cross_os_import_roundtrip() {
    let tmp = tempdir().expect("tempdir");
    let base = tmp.path();

    // Source library on machine A
    let src_dir = base.join("machine_a").join("Cowboy Bebop");
    create_show_folder(
        &src_dir,
        &[
            "Season 1/S01E01.mp4",
            "Season 1/S01E02.mp4",
            "Season 1/S01E03.mp4",
        ],
    );

    let store_a = NotflixStore::new(Some(base.join("a.db"))).expect("create store A");
    let (show_a, ll_a) = store_a
        .add_show(src_dir.to_str().unwrap(), Some("Cowboy Bebop"))
        .expect("add show A");
    let eps_a = ll_a.to_vec();

    store_a
        .update_progress(
            &eps_a[0].id,
            Some(1400.0),
            Some(1440.0),
            Some(true),
            Some(false),
            false,
        )
        .expect("update ep1");
    store_a
        .set_episode_intro(&eps_a[0].id, Some(15.0), Some(105.0), 0.95, "chapters")
        .expect("set ep1 intro");

    let mut outros = HashMap::new();
    outros.insert(
        eps_a[0].id.clone(),
        IntroSpan {
            start_sec: 1320.0,
            end_sec: 1440.0,
            confidence: 0.96,
            source: "chapter".to_string(),
        },
    );
    store_a
        .save_show_outros(&show_a.id, &outros, false)
        .expect("save ep1 outro");

    store_a
        .update_progress(
            &eps_a[1].id,
            Some(412.25),
            Some(1450.0),
            Some(false),
            Some(true),
            false,
        )
        .expect("update ep2");

    // Export watch history and serialize/deserialize through JSON
    let exported = store_a.export_watch_history().expect("export history");
    assert_eq!(exported.version, 1);
    assert!(!exported.exported_at.is_empty());
    assert_eq!(exported.shows.len(), 1);
    assert_eq!(exported.shows[0].name, "Cowboy Bebop");
    assert_eq!(
        exported.shows[0]
            .current_episode_relative_path
            .as_deref()
            .map(|p| p.replace('\\', "/"))
            .as_deref(),
        Some("Season 1/S01E02.mp4")
    );
    assert!(exported.shows[0].last_watched_at.is_some());

    // Simulate cross-OS backup: rewrite folder_path and relative_path to Windows style,
    // plus add an unmatched show and an extra unmatched episode.
    let json_text = serde_json::to_string(&exported).expect("serialize export");
    let mut imported_payload: notflix::storage::WatchHistoryExport =
        serde_json::from_str(&json_text).expect("deserialize export");
    imported_payload.shows[0].folder_path = r"C:\Shows\Cowboy Bebop".to_string();
    imported_payload.shows[0].current_episode_relative_path =
        Some(r"Season 1\S01E02.mp4".to_string());
    for ep in &mut imported_payload.shows[0].episodes {
        ep.relative_path = ep.relative_path.replace('/', "\\");
    }
    imported_payload.shows[0]
        .episodes
        .push(notflix::storage::ExportEpisode {
            relative_path: r"Season 1\S01E99.mp4".to_string(),
            filename: "S01E99.mp4".to_string(),
            position_sec: 10.0,
            duration_sec: 100.0,
            watched: false,
            intro_start_sec: None,
            intro_end_sec: None,
            intro_source: String::new(),
            outro_start_sec: None,
            outro_source: String::new(),
        });
    imported_payload.shows.push(notflix::storage::ExportShow {
        name: "Nonexistent Show".to_string(),
        folder_path: r"D:\Missing\Show".to_string(),
        current_episode_relative_path: None,
        last_watched_at: None,
        episodes: vec![notflix::storage::ExportEpisode {
            relative_path: "S01E01.mp4".to_string(),
            filename: "S01E01.mp4".to_string(),
            position_sec: 5.0,
            duration_sec: 100.0,
            watched: false,
            intro_start_sec: None,
            intro_end_sec: None,
            intro_source: String::new(),
            outro_start_sec: None,
            outro_source: String::new(),
        }],
    });

    // Target library on machine B with completely different path
    let dst_dir = base.join("machine_b").join("anime_folder");
    create_show_folder(
        &dst_dir,
        &[
            "season 1/S01E01.mp4",
            "season 1/S01E02.mp4",
            "season 1/S01E03.mp4",
        ],
    );

    let store_b = NotflixStore::new(Some(base.join("b.db"))).expect("create store B");
    let (show_b, _) = store_b
        .add_show(dst_dir.to_str().unwrap(), Some("cowboy bebop"))
        .expect("add show B");

    let summary = store_b
        .import_watch_history(&imported_payload)
        .expect("import history");
    assert_eq!(summary.matched_shows, 1);
    assert_eq!(summary.unmatched_shows, 1);
    assert_eq!(summary.updated_episodes, 3);
    assert_eq!(summary.skipped_episodes, 2);

    let (reloaded_show, reloaded_ll) = store_b
        .get_show(&show_b.id)
        .expect("get show B")
        .expect("show B exists");
    assert!(reloaded_show.last_watched_at.is_some());
    assert_eq!(
        reloaded_ll.current().map(|n| n.filename.as_str()),
        Some("S01E02.mp4")
    );

    let reloaded_eps = reloaded_ll.to_vec();
    assert!(reloaded_eps[0].watched);
    assert!((reloaded_eps[0].position_sec - 1400.0).abs() < 1e-6);
    assert!((reloaded_eps[0].duration_sec - 1440.0).abs() < 1e-6);
    assert_eq!(reloaded_eps[0].intro_start_sec, Some(15.0));
    assert_eq!(reloaded_eps[0].intro_end_sec, Some(105.0));
    assert_eq!(reloaded_eps[0].intro_source, "chapters");
    assert_eq!(reloaded_eps[0].outro_start_sec, Some(1320.0));
    assert_eq!(reloaded_eps[0].outro_source, "chapter");

    assert!(!reloaded_eps[1].watched);
    assert!((reloaded_eps[1].position_sec - 412.25).abs() < 1e-6);
    assert!((reloaded_eps[1].duration_sec - 1450.0).abs() < 1e-6);
}
