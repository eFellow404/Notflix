use std::fs;
use std::path::Path;

use tempfile::TempDir;

use notflix::media::{
    find_or_generate_episode_thumbnail, find_show_poster, guess_image_mime,
    is_browser_compatible_audio_codec, parse_ffprobe_audio_streams, AudioTrack,
};
use notflix::subtitles::{discover_subtitles, load_subtitle_as_vtt, srt_to_vtt, SubtitleTrack};

#[test]
fn srt_to_vtt_handles_bom_crlf_dialogue_commas_multiline_and_settings() {
    let srt = "\u{feff}1\r\n00:01:02,345 --> 00:01:05,678 line:85% position:50% align:center\r\nHello, world, how are you?\r\nSecond line, still intact!\r\n\r\n2\r\n00:02:10,000 --> 00:02:14,500\r\nOne, two, three.\r\n";

    let vtt = srt_to_vtt(srt);

    assert!(vtt.starts_with("WEBVTT\n\n"));
    assert!(!vtt.contains('\u{feff}'));
    assert!(!vtt.contains('\r'));
    assert!(vtt.contains("00:01:02.345 --> 00:01:05.678 line:85% position:50% align:center\n"));
    assert!(vtt.contains("Hello, world, how are you?\nSecond line, still intact!\n"));
    assert!(vtt.contains("00:02:10.000 --> 00:02:14.500\nOne, two, three.\n"));
}

#[test]
fn srt_to_vtt_handles_lone_cr_line_endings() {
    let srt = "1\r00:00:01,100 --> 00:00:02,200\rLine A, with comma\r\r";
    let vtt = srt_to_vtt(srt);
    assert_eq!(
        vtt,
        "WEBVTT\n\n1\n00:00:01.100 --> 00:00:02.200\nLine A, with comma\n\n"
    );
}

#[test]
fn load_subtitle_as_vtt_decodes_latin1_srt_and_normalizes_vtt() {
    let dir = TempDir::new().unwrap();

    // ISO-8859-1 bytes: "café, garçon!" (0xE9 = 'é', 0xE7 = 'ç')
    let srt_path = dir.path().join("episode.fr.srt");
    let raw_latin1: &[u8] =
        b"1\r\n00:00:01,250 --> 00:00:04,750\r\nBienvenue au caf\xe9, gar\xe7on!\r\n";
    fs::write(&srt_path, raw_latin1).unwrap();

    let srt_track = SubtitleTrack {
        index: 0,
        label: "French".to_string(),
        lang: "fr".to_string(),
        format: "srt".to_string(),
        filename: "episode.fr.srt".to_string(),
        path: srt_path,
    };
    let vtt_from_srt = load_subtitle_as_vtt(&srt_track).unwrap();
    assert!(vtt_from_srt.starts_with("WEBVTT\n\n"));
    assert!(vtt_from_srt.contains("00:00:01.250 --> 00:00:04.750\n"));
    assert!(vtt_from_srt.contains("Bienvenue au café, garçon!"));
    assert!(!vtt_from_srt.contains('\r'));

    // Native VTT with UTF-8 BOM and CRLF
    let vtt_path = dir.path().join("episode.en.vtt");
    fs::write(
        &vtt_path,
        "\u{feff}WEBVTT\r\n\r\n00:00:05.000 --> 00:00:08.000\r\nAlready VTT, nice.\r\n",
    )
    .unwrap();
    let vtt_track = SubtitleTrack {
        index: 1,
        label: "English".to_string(),
        lang: "en".to_string(),
        format: "vtt".to_string(),
        filename: "episode.en.vtt".to_string(),
        path: vtt_path,
    };
    let loaded_vtt = load_subtitle_as_vtt(&vtt_track).unwrap();
    assert!(loaded_vtt.starts_with("WEBVTT\n\n"));
    assert!(!loaded_vtt.contains('\u{feff}'));
    assert!(!loaded_vtt.contains('\r'));
    assert!(loaded_vtt.contains("Already VTT, nice."));

    // Native VTT missing WEBVTT header gets header prepended
    let bare_vtt_path = dir.path().join("bare.vtt");
    fs::write(&bare_vtt_path, "00:00:01.000 --> 00:00:02.000\nHi\n").unwrap();
    let bare_track = SubtitleTrack {
        index: 2,
        label: "Subtitles".to_string(),
        lang: "und".to_string(),
        format: "vtt".to_string(),
        filename: "bare.vtt".to_string(),
        path: bare_vtt_path,
    };
    let loaded_bare = load_subtitle_as_vtt(&bare_track).unwrap();
    assert!(loaded_bare.starts_with("WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nHi"));
}

#[test]
fn discover_subtitles_scans_parent_subs_and_nested_stem_folders_and_sorts() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    let video_path = root.join("Show.S01E01.mkv");
    fs::write(&video_path, b"video").unwrap();

    // Same folder matches
    fs::write(root.join("Show.S01E01.es.forced.vtt"), b"WEBVTT\n").unwrap();
    fs::write(root.join("Show.S01E01.en.hi.srt"), b"1\n").unwrap();
    fs::write(root.join("Show.S01E01.srt"), b"1\n").unwrap();
    fs::write(root.join("Show.S01E01.tr.srt"), b"1\n").unwrap();
    fs::write(root.join("Show.S01E01.hi.srt"), b"1\n").unwrap();

    // Unrelated files in same folder (including stem prefix without delimiter)
    fs::write(root.join("Show.S01E010.en.srt"), b"1\n").unwrap();
    fs::write(root.join("Show.S01E02.en.srt"), b"1\n").unwrap();
    fs::write(root.join("Show.S01E01.txt"), b"not a subtitle").unwrap();

    // Top-level Subs/ folder
    let subs_dir = root.join("Subs");
    fs::create_dir_all(&subs_dir).unwrap();
    fs::write(subs_dir.join("Show.S01E01_fr_sdh.srt"), b"1\n").unwrap();
    fs::write(subs_dir.join("Show.S01E01-en-forced.srt"), b"1\n").unwrap();
    fs::write(subs_dir.join("OtherEpisode.en.srt"), b"1\n").unwrap();

    // Nested Subs/<video_stem>/ folder (any .srt/.vtt matches)
    let nested_dir = subs_dir.join("show.s01e01");
    fs::create_dir_all(&nested_dir).unwrap();
    fs::write(nested_dir.join("2_English.srt"), b"1\n").unwrap();
    fs::write(nested_dir.join("3_German.commentary.vtt"), b"WEBVTT\n").unwrap();

    // Nested folder for another episode must be ignored
    let other_nested = subs_dir.join("Show.S01E02");
    fs::create_dir_all(&other_nested).unwrap();
    fs::write(other_nested.join("2_English.srt"), b"1\n").unwrap();

    let tracks = discover_subtitles(&video_path);

    // Ensure sequential indices 0..len
    for (i, t) in tracks.iter().enumerate() {
        assert_eq!(t.index, i);
    }

    // Ensure unrelated videos were ignored
    assert!(tracks.iter().all(|t| !t.filename.contains("S01E010")));
    assert!(tracks.iter().all(|t| !t.filename.contains("S01E02")));
    assert!(tracks.iter().all(|t| !t.filename.contains("OtherEpisode")));
    assert_eq!(tracks.len(), 9);

    // English non-forced tracks come first (alphabetical by filename)
    assert_eq!(tracks[0].filename, "2_English.srt");
    assert_eq!(tracks[0].lang, "en");
    assert_eq!(tracks[0].label, "English");

    assert_eq!(tracks[1].filename, "Show.S01E01.en.hi.srt");
    assert_eq!(tracks[1].lang, "en");
    assert_eq!(tracks[1].label, "English (SDH)");

    // English forced track comes after English non-forced, before non-English
    assert_eq!(tracks[2].filename, "Show.S01E01-en-forced.srt");
    assert_eq!(tracks[2].lang, "en");
    assert_eq!(tracks[2].label, "English (Forced)");

    // Non-English non-forced tracks follow (alphabetical by filename)
    let non_en_non_forced: Vec<(&str, &str, &str)> = tracks[3..8]
        .iter()
        .map(|t| (t.filename.as_str(), t.lang.as_str(), t.label.as_str()))
        .collect();
    assert_eq!(
        non_en_non_forced,
        vec![
            ("3_German.commentary.vtt", "de", "German (Commentary)"),
            ("Show.S01E01.hi.srt", "hi", "Hindi"),
            ("Show.S01E01.srt", "und", "Subtitles"),
            ("Show.S01E01.tr.srt", "tr", "Turkish"),
            ("Show.S01E01_fr_sdh.srt", "fr", "French (SDH)"),
        ]
    );

    // Non-English forced track comes last
    assert_eq!(tracks[8].filename, "Show.S01E01.es.forced.vtt");
    assert_eq!(tracks[8].lang, "es");
    assert_eq!(tracks[8].label, "Spanish (Forced)");
    assert_eq!(tracks[8].format, "vtt");
}

#[test]
fn parse_ffprobe_audio_streams_detects_compatibility_and_indices() {
    let ffprobe_json = r#"{
        "streams": [
            {
                "index": 1,
                "codec_name": "ac3",
                "channels": 6,
                "tags": { "language": "eng", "title": "English 5.1 Surround" }
            },
            {
                "index": 2,
                "codec_name": "dts",
                "channels": 6,
                "tags": { "language": "jpn" }
            },
            {
                "index": 3,
                "codec_name": "aac",
                "channels": 2,
                "tags": { "language": "eng", "title": "Stereo Commentary" }
            },
            {
                "index": 4,
                "codec_name": "opus"
            }
        ]
    }"#;

    let tracks = parse_ffprobe_audio_streams(ffprobe_json);
    assert_eq!(
        tracks,
        vec![
            AudioTrack {
                index: 0,
                codec: "ac3".to_string(),
                channels: 6,
                language: "eng".to_string(),
                title: "English 5.1 Surround".to_string(),
                browser_compatible: false,
            },
            AudioTrack {
                index: 1,
                codec: "dts".to_string(),
                channels: 6,
                language: "jpn".to_string(),
                title: String::new(),
                browser_compatible: false,
            },
            AudioTrack {
                index: 2,
                codec: "aac".to_string(),
                channels: 2,
                language: "eng".to_string(),
                title: "Stereo Commentary".to_string(),
                browser_compatible: true,
            },
            AudioTrack {
                index: 3,
                codec: "opus".to_string(),
                channels: 2,
                language: "und".to_string(),
                title: String::new(),
                browser_compatible: true,
            },
        ]
    );

    assert!(is_browser_compatible_audio_codec("aac"));
    assert!(is_browser_compatible_audio_codec("MP3"));
    assert!(is_browser_compatible_audio_codec("opus"));
    assert!(is_browser_compatible_audio_codec("flac"));
    assert!(is_browser_compatible_audio_codec("pcm_s16le"));
    assert!(!is_browser_compatible_audio_codec("ac3"));
    assert!(!is_browser_compatible_audio_codec("eac3"));
    assert!(!is_browser_compatible_audio_codec("dts"));
    assert!(!is_browser_compatible_audio_codec("truehd"));
    assert!(!is_browser_compatible_audio_codec("wma"));

    assert!(parse_ffprobe_audio_streams("not json").is_empty());
}

#[test]
fn guess_image_mime_maps_common_extensions() {
    assert_eq!(guess_image_mime(Path::new("poster.jpg")), "image/jpeg");
    assert_eq!(guess_image_mime(Path::new("POSTER.JPEG")), "image/jpeg");
    assert_eq!(guess_image_mime(Path::new("cover.png")), "image/png");
    assert_eq!(guess_image_mime(Path::new("thumb.WebP")), "image/webp");
    assert_eq!(guess_image_mime(Path::new("anim.gif")), "image/gif");
    assert_eq!(
        guess_image_mime(Path::new("unknown.bmp")),
        "application/octet-stream"
    );
}

#[test]
fn find_show_poster_respects_priority_and_case_insensitivity() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    assert_eq!(find_show_poster(root), None);

    // Ignore non-image files and subdirectories with poster names
    fs::write(root.join("poster.txt"), b"nope").unwrap();
    fs::create_dir(root.join("poster.png")).unwrap();
    assert_eq!(find_show_poster(root), None);

    let cover = root.join("Cover.webp");
    fs::write(&cover, b"webp").unwrap();
    assert_eq!(find_show_poster(root), Some(cover));

    let folder = root.join("FOLDER.PNG");
    fs::write(&folder, b"png").unwrap();
    assert_eq!(find_show_poster(root), Some(folder));

    let poster = root.join("PoStEr.JPG");
    fs::write(&poster, b"jpg").unwrap();
    assert_eq!(find_show_poster(root), Some(poster));
}

#[test]
fn find_or_generate_episode_thumbnail_returns_sidecar_and_cached_without_ffmpeg() {
    let dir = TempDir::new().unwrap();
    let video_dir = dir.path().join("show");
    let cache_dir = dir.path().join("cache");
    fs::create_dir_all(&video_dir).unwrap();
    fs::create_dir_all(&cache_dir).unwrap();

    let video_path = video_dir.join("Show.S01E01.mkv");
    fs::write(&video_path, b"not a real video").unwrap();

    // 1. Pre-populated cache hit (with sanitized episode_id)
    let cached = cache_dir.join("ep_01_test.jpg");
    fs::write(&cached, b"fake-jpeg-bytes").unwrap();
    assert_eq!(
        find_or_generate_episode_thumbnail(&video_path, &cache_dir, "ep/01:test"),
        Some(cached)
    );

    // 2. Sidecar `-thumb` takes priority over cache
    let thumb_sidecar = video_dir.join("show.s01e01-THUMB.webp");
    fs::write(&thumb_sidecar, b"fake-webp").unwrap();
    assert_eq!(
        find_or_generate_episode_thumbnail(&video_path, &cache_dir, "ep/01:test"),
        Some(thumb_sidecar)
    );

    // 3. Direct `<stem>.jpg` sidecar takes priority over `-thumb`
    let direct_sidecar = video_dir.join("Show.S01E01.jpg");
    fs::write(&direct_sidecar, b"fake-jpg").unwrap();
    assert_eq!(
        find_or_generate_episode_thumbnail(&video_path, &cache_dir, "ep/01:test"),
        Some(direct_sidecar)
    );

    // 4. Empty 0-byte cache file is ignored (not returned as valid cached thumbnail)
    let video2 = video_dir.join("Show.S01E02.mkv");
    fs::write(&video2, b"not a real video").unwrap();
    let empty_cached = cache_dir.join("ep_empty.jpg");
    fs::write(&empty_cached, b"").unwrap();
    assert_eq!(
        find_or_generate_episode_thumbnail(&video2, &cache_dir, "ep_empty"),
        None
    );
}
