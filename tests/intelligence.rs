//! Integration tests for the intelligent DVD-rip episode ordering engine.

use std::fs;
use std::path::Path;

use notflix::error::NotflixError;
use notflix::intelligence::{
    discover_video_files, is_video_extension, natural_sort_key, scan_and_order_folder,
    VIDEO_EXTENSIONS,
};
use tempfile::TempDir;

fn populate(root: &Path, rel_paths: &[&str]) {
    for rel in rel_paths {
        let target = root.join(rel);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&target, b"fake-video-data").unwrap();
    }
}

#[test]
fn test_standard_sxxexx_and_noise_stripping() {
    let tmp = TempDir::new().unwrap();
    populate(
        tmp.path(),
        &[
            "Show.2004.1080p.x264.S01E10.Finale.mkv",
            "Show.2004.720p.DVDRip.S01E02.Second.mkv",
            "Show.2004.480p.AC3.S01E01.Pilot.mkv",
            "Show.2004.S02E01.Return.mkv",
            "Show.2004.1x03.Third.mp4",
        ],
    );

    let ordered = scan_and_order_folder(tmp.path()).unwrap();
    let filenames: Vec<&str> = ordered.iter().map(|e| e.filename.as_str()).collect();
    assert_eq!(
        filenames,
        vec![
            "Show.2004.480p.AC3.S01E01.Pilot.mkv",
            "Show.2004.720p.DVDRip.S01E02.Second.mkv",
            "Show.2004.1x03.Third.mp4",
            "Show.2004.1080p.x264.S01E10.Finale.mkv",
            "Show.2004.S02E01.Return.mkv",
        ]
    );
    assert_eq!(ordered[0].season, Some(1));
    assert_eq!(ordered[0].episode_number, Some(1));
    assert_eq!(ordered.last().unwrap().season, Some(2));
    assert_eq!(ordered.last().unwrap().episode_number, Some(1));
}

#[test]
fn test_makemkv_disc_folders() {
    let tmp = TempDir::new().unwrap();
    populate(
        tmp.path(),
        &[
            "Season 1/Disc 2/title_t01.mkv",
            "Season 1/Disc 1/title_t02.mkv",
            "Season 1/Disc 1/title_t00.mkv",
            "Season 1/Disc 2/title_t00.mkv",
            "Season 1/Disc 1/title_t01.mkv",
            "Season 1/Extras/Behind_the_Scenes.mkv",
        ],
    );

    let ordered = scan_and_order_folder(tmp.path()).unwrap();
    let rel_paths: Vec<&str> = ordered.iter().map(|e| e.relative_path.as_str()).collect();
    assert_eq!(
        rel_paths,
        vec![
            "Season 1/Disc 1/title_t00.mkv",
            "Season 1/Disc 1/title_t01.mkv",
            "Season 1/Disc 1/title_t02.mkv",
            "Season 1/Disc 2/title_t00.mkv",
            "Season 1/Disc 2/title_t01.mkv",
            "Season 1/Extras/Behind_the_Scenes.mkv",
        ]
    );
    assert!(ordered.last().unwrap().is_extra);
    assert!(!ordered[0].is_extra);
}

#[test]
fn test_natural_number_ordering_beats_lexicographic() {
    let tmp = TempDir::new().unwrap();
    populate(
        tmp.path(),
        &["ep10.mp4", "ep2.mp4", "ep1.mp4", "ep11.mp4", "ep3.mp4"],
    );

    let ordered = scan_and_order_folder(tmp.path()).unwrap();
    let filenames: Vec<&str> = ordered.iter().map(|e| e.filename.as_str()).collect();
    assert_eq!(
        filenames,
        vec!["ep1.mp4", "ep2.mp4", "ep3.mp4", "ep10.mp4", "ep11.mp4"]
    );
}

#[test]
fn test_three_digit_see_consensus() {
    let tmp = TempDir::new().unwrap();
    populate(
        tmp.path(),
        &[
            "202 - Winter.mp4",
            "102 - Gathering.mp4",
            "201 - New Dawn.mp4",
            "101 - Beginning.mp4",
        ],
    );

    let ordered = scan_and_order_folder(tmp.path()).unwrap();
    let summary: Vec<(Option<i64>, Option<i64>, &str)> = ordered
        .iter()
        .map(|e| (e.season, e.episode_number, e.filename.as_str()))
        .collect();
    assert_eq!(
        summary,
        vec![
            (Some(1), Some(1), "101 - Beginning.mp4"),
            (Some(1), Some(2), "102 - Gathering.mp4"),
            (Some(2), Some(1), "201 - New Dawn.mp4"),
            (Some(2), Some(2), "202 - Winter.mp4"),
        ]
    );
}

#[test]
fn test_numeric_show_title_ignored_via_sibling_consensus() {
    let tmp = TempDir::new().unwrap();
    populate(
        tmp.path(),
        &[
            "24 - 10 - Ten AM.mp4",
            "24 - 02 - Two AM.mp4",
            "24 - 01 - One AM.mp4",
            "24 - 03 - Three AM.mp4",
        ],
    );

    let ordered = scan_and_order_folder(tmp.path()).unwrap();
    let filenames: Vec<&str> = ordered.iter().map(|e| e.filename.as_str()).collect();
    assert_eq!(
        filenames,
        vec![
            "24 - 01 - One AM.mp4",
            "24 - 02 - Two AM.mp4",
            "24 - 03 - Three AM.mp4",
            "24 - 10 - Ten AM.mp4",
        ]
    );
    let ep_nums: Vec<Option<i64>> = ordered.iter().map(|e| e.episode_number).collect();
    assert_eq!(ep_nums, vec![Some(1), Some(2), Some(3), Some(10)]);
}

#[test]
fn test_roman_numerals_pilot_and_bonus_separation() {
    let tmp = TempDir::new().unwrap();
    populate(
        tmp.path(),
        &[
            "Deleted Scenes.mp4",
            "Miniseries Part III.mp4",
            "Miniseries Part I.mp4",
            "Miniseries Part II.mp4",
            "Pilot.mp4",
        ],
    );

    let ordered = scan_and_order_folder(tmp.path()).unwrap();
    let filenames: Vec<&str> = ordered.iter().map(|e| e.filename.as_str()).collect();
    assert_eq!(
        filenames,
        vec![
            "Pilot.mp4",
            "Miniseries Part I.mp4",
            "Miniseries Part II.mp4",
            "Miniseries Part III.mp4",
            "Deleted Scenes.mp4",
        ]
    );
    assert!(ordered.last().unwrap().is_extra);
}

#[test]
fn test_empty_folder_and_missing_folder() {
    let tmp = TempDir::new().unwrap();
    let ordered = scan_and_order_folder(tmp.path()).unwrap();
    assert!(ordered.is_empty());

    let missing = tmp.path().join("does-not-exist");
    let err = discover_video_files(&missing).unwrap_err();
    assert!(matches!(err, NotflixError::NotFound(_)));
}

#[test]
fn test_hidden_and_ignored_entries_and_posix_paths() {
    let tmp = TempDir::new().unwrap();
    populate(
        tmp.path(),
        &[
            "S01E01.mp4",
            ".hidden.mp4",
            "__MACOSX/._S01E01.mp4",
            ".git/ignored.mp4",
            "AUDIO_TS/track.vob",
            "Season 1/S01E02.MKV",
            "notes.txt",
        ],
    );

    let ordered = scan_and_order_folder(tmp.path()).unwrap();
    assert_eq!(ordered.len(), 2);

    assert_eq!(ordered[0].relative_path, "S01E01.mp4");
    assert_eq!(ordered[0].parent_rel, ".");
    assert_eq!(ordered[1].relative_path, "Season 1/S01E02.MKV");
    assert_eq!(ordered[1].parent_rel, "Season 1");
    assert!(!ordered[1].relative_path.contains('\\'));
}

#[test]
fn test_video_extensions_and_natural_sort_huge_digits() {
    assert_eq!(VIDEO_EXTENSIONS.len(), 14);
    assert!(is_video_extension(".mp4"));
    assert!(is_video_extension("MP4"));
    assert!(is_video_extension(".m2ts"));
    assert!(is_video_extension("MKV"));
    assert!(!is_video_extension(".txt"));
    assert!(!is_video_extension(""));

    // Huge digit sequences must not panic
    let k1 = natural_sort_key("ep_99999999999999999999999999999999.mp4");
    let k2 = natural_sort_key("ep_2.mp4");
    assert_ne!(k1, k2);
    assert!(natural_sort_key("Episode 2") < natural_sort_key("episode 10"));
}
