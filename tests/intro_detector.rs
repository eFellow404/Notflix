//! Integration tests for cross-episode intro detection, container chapter parsing, and media helpers.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use axum::http::{header, StatusCode};
use http_body_util::BodyExt;
use notflix::chapters as chapters_mod;
use notflix::intro_detector::{
    self as detector_mod, build_fingerprint, detect_intro_from_chapters, detect_show_intros_server,
    extract_audio_fingerprint_from_pcm, extract_mkv_chapters, extract_mp4_chapters,
    find_shared_intros_default, ChapterMarker,
};
use notflix::media::{guess_video_mime, parse_range_header, stream_video_file};

/// Tiny deterministic SplitMix64 PRNG so tests need no external `rand` crate.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn uniform(&mut self, low: f64, high: f64) -> f64 {
        let unit = ((self.next_u64() >> 11) as f64) * (1.0 / ((1u64 << 53) as f64));
        low + unit * (high - low)
    }
}

fn rand_vec(rng: &mut Rng, dim: usize) -> Vec<f64> {
    (0..dim).map(|_| rng.uniform(20.0, 235.0)).collect()
}

fn perturb_vec(rng: &mut Rng, vec: &[f64], noise: f64) -> Vec<f64> {
    vec.iter()
        .map(|&v| (v + rng.uniform(-noise, noise)).clamp(0.0, 255.0))
        .collect()
}

fn make_episode_vectors(
    rng: &mut Rng,
    total_frames: usize,
    intro_template: Option<&[Vec<f64>]>,
    intro_offset: usize,
    noise: f64,
) -> Vec<Vec<f64>> {
    let mut frames: Vec<Vec<f64>> = (0..total_frames).map(|_| rand_vec(rng, 36)).collect();
    if let Some(template) = intro_template {
        for (idx, base) in template.iter().enumerate() {
            let pos = intro_offset + idx;
            if pos < total_frames {
                frames[pos] = perturb_vec(rng, base, noise);
            }
        }
    }
    frames
}

fn ebml_vint_size(n: usize) -> Vec<u8> {
    if n < 0x7f {
        vec![0x80 | (n as u8)]
    } else if n < 0x3fff {
        (0x4000u16 | (n as u16)).to_be_bytes().to_vec()
    } else {
        (0x1000_0000u32 | (n as u32)).to_be_bytes().to_vec()
    }
}

fn ebml_elem(eid_bytes: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(eid_bytes.len() + 4 + payload.len());
    out.extend_from_slice(eid_bytes);
    out.extend_from_slice(&ebml_vint_size(payload.len()));
    out.extend_from_slice(payload);
    out
}

fn build_minimal_mkv_with_chapters(chapters: &[(f64, f64, &str)]) -> Vec<u8> {
    let mut atoms = Vec::new();
    for &(start_s, end_s, title) in chapters {
        let start_ns = (start_s * 1_000_000_000.0) as u64;
        let end_ns = (end_s * 1_000_000_000.0) as u64;
        let disp = ebml_elem(&[0x80], &ebml_elem(&[0x85], title.as_bytes()));
        let mut body = Vec::new();
        body.extend_from_slice(&ebml_elem(&[0x91], &start_ns.to_be_bytes()));
        body.extend_from_slice(&ebml_elem(&[0x92], &end_ns.to_be_bytes()));
        body.extend_from_slice(&disp);
        atoms.extend_from_slice(&ebml_elem(&[0xb6], &body));
    }
    let edition = ebml_elem(&[0x45, 0xb9], &atoms);
    let chapters_el = ebml_elem(&[0x10, 0x43, 0xa7, 0x70], &edition);
    let ebml_header = ebml_elem(
        &[0x1a, 0x45, 0xdf, 0xa3],
        &ebml_elem(&[0x42, 0x82], b"matroska"),
    );
    let segment = ebml_elem(&[0x18, 0x53, 0x80, 0x67], &chapters_el);
    let mut out = ebml_header;
    out.extend_from_slice(&segment);
    out
}

fn build_mkv_with_seekhead_past_cluster(chapters: &[(f64, f64, &str)]) -> Vec<u8> {
    let mut atoms = Vec::new();
    for &(start_s, end_s, title) in chapters {
        let start_ns = (start_s * 1_000_000_000.0) as u64;
        let end_ns = (end_s * 1_000_000_000.0) as u64;
        let disp = ebml_elem(&[0x80], &ebml_elem(&[0x85], title.as_bytes()));
        let mut body = Vec::new();
        body.extend_from_slice(&ebml_elem(&[0x91], &start_ns.to_be_bytes()));
        body.extend_from_slice(&ebml_elem(&[0x92], &end_ns.to_be_bytes()));
        body.extend_from_slice(&disp);
        atoms.extend_from_slice(&ebml_elem(&[0xb6], &body));
    }
    let edition = ebml_elem(&[0x45, 0xb9], &atoms);
    let chapters_el = ebml_elem(&[0x10, 0x43, 0xa7, 0x70], &edition);
    let cluster_el = ebml_elem(&[0x1f, 0x43, 0xb6, 0x75], &[0u8; 64]);

    // Build SeekHead twice so we know its exact encoded size before computing Chapters offset
    let make_seekhead = |chapters_pos: u64| {
        let mut seek_body = Vec::new();
        seek_body.extend_from_slice(&ebml_elem(&[0x53, 0xab], &[0x10, 0x43, 0xa7, 0x70]));
        seek_body.extend_from_slice(&ebml_elem(&[0x53, 0xac], &chapters_pos.to_be_bytes()));
        let seek = ebml_elem(&[0x4d, 0xbb], &seek_body);
        ebml_elem(&[0x11, 0x4d, 0x9b, 0x74], &seek)
    };
    let dummy_seekhead = make_seekhead(0);
    let chapters_pos = (dummy_seekhead.len() + cluster_el.len()) as u64;
    let seekhead = make_seekhead(chapters_pos);

    let mut seg_body = Vec::new();
    seg_body.extend_from_slice(&seekhead);
    seg_body.extend_from_slice(&cluster_el);
    seg_body.extend_from_slice(&chapters_el);

    let ebml_header = ebml_elem(
        &[0x1a, 0x45, 0xdf, 0xa3],
        &ebml_elem(&[0x42, 0x82], b"matroska"),
    );
    let segment = ebml_elem(&[0x18, 0x53, 0x80, 0x67], &seg_body);
    let mut out = ebml_header;
    out.extend_from_slice(&segment);
    out
}

fn mp4_box(box_type: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
    out.extend_from_slice(box_type);
    out.extend_from_slice(payload);
    out
}

fn build_minimal_mp4_with_chpl(chapters: &[(f64, &str)]) -> Vec<u8> {
    let mut entries = Vec::new();
    entries.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]); // version 1 + flags
    entries.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // reserved
    entries.push(chapters.len() as u8);
    for &(start_s, title) in chapters {
        let t_100ns = (start_s * 10_000_000.0) as u64;
        let title_b = title.as_bytes();
        entries.extend_from_slice(&t_100ns.to_be_bytes());
        entries.push(title_b.len() as u8);
        entries.extend_from_slice(title_b);
    }
    let chpl = mp4_box(b"chpl", &entries);
    let udta = mp4_box(b"udta", &chpl);
    let moov = mp4_box(b"moov", &udta);
    let ftyp = mp4_box(b"ftyp", b"isom\x00\x00\x02\x00isom");
    let mut out = ftyp;
    out.extend_from_slice(&moov);
    out
}

#[test]
fn detects_shared_intro_with_variable_cold_opens() {
    let mut rng = Rng::new(42);
    let step = 1.5;
    // 30-frame intro = 45 seconds
    let intro_template: Vec<Vec<f64>> = (0..30).map(|_| rand_vec(&mut rng, 36)).collect();

    // Different cold-open offsets across 4 episodes:
    // Ep1: 15s (frame 10), Ep2: 60s (frame 40), Ep3: 0s (frame 0), Ep4: 90s (frame 60)
    let offsets = [("ep1", 10usize), ("ep2", 40), ("ep3", 0), ("ep4", 60)];
    let fps: Vec<_> = offsets
        .iter()
        .map(|&(ep_id, off)| {
            let raw = make_episode_vectors(&mut rng, 160, Some(&intro_template), off, 8.0);
            build_fingerprint(ep_id, step, &raw, None, 0.0, "visual", 8.0)
        })
        .collect();

    let detected = find_shared_intros_default(&fps);
    let actual_keys: HashSet<&str> = detected.keys().map(|s| s.as_str()).collect();
    let expected_keys: HashSet<&str> = offsets.iter().map(|&(k, _)| k).collect();
    assert_eq!(actual_keys, expected_keys);

    for &(ep_id, off) in &offsets {
        let span = &detected[ep_id];
        let expected_start = (off as f64) * step;
        let expected_end = ((off + 30) as f64) * step;
        assert!(
            (span.start_sec - expected_start).abs() <= 3.0,
            "{ep_id}: expected start ~{expected_start}, got {}",
            span.start_sec
        );
        assert!(
            (span.end_sec - expected_end).abs() <= 3.0,
            "{ep_id}: expected end ~{expected_end}, got {}",
            span.end_sec
        );
        assert!(span.confidence >= 0.80);
        assert_eq!(span.source, "visual");
    }
}

#[test]
fn pilot_without_intro_not_falsely_tagged() {
    let mut rng = Rng::new(123);
    let step = 1.5;
    let intro_template: Vec<Vec<f64>> = (0..28).map(|_| rand_vec(&mut rng, 36)).collect(); // 42s intro

    let pilot_raw = make_episode_vectors(&mut rng, 140, None, 0, 8.0);
    let ep2_raw = make_episode_vectors(&mut rng, 140, Some(&intro_template), 12, 8.0);
    let ep3_raw = make_episode_vectors(&mut rng, 140, Some(&intro_template), 34, 8.0);
    let ep4_raw = make_episode_vectors(&mut rng, 140, Some(&intro_template), 4, 8.0);

    let fps = vec![
        build_fingerprint("pilot", step, &pilot_raw, None, 0.0, "visual", 0.0),
        build_fingerprint("ep2", step, &ep2_raw, None, 0.0, "visual", 0.0),
        build_fingerprint("ep3", step, &ep3_raw, None, 0.0, "visual", 0.0),
        build_fingerprint("ep4", step, &ep4_raw, None, 0.0, "visual", 0.0),
    ];

    let detected = find_shared_intros_default(&fps);
    assert!(!detected.contains_key("pilot"));
    assert!(detected.contains_key("ep2"));
    assert!(detected.contains_key("ep3"));
    assert!(detected.contains_key("ep4"));
    assert!((detected["ep2"].start_sec - 12.0 * step).abs() <= 3.0);
    assert!((detected["ep3"].start_sec - 34.0 * step).abs() <= 3.0);
    assert!((detected["ep4"].start_sec - 4.0 * step).abs() <= 3.0);
}

#[test]
fn black_flat_frames_do_not_produce_false_match() {
    let mut rng = Rng::new(99);
    let step = 1.5;
    let black = vec![2.0; 36];

    let mut make_with_black_gap = || {
        let mut frames: Vec<Vec<f64>> = (0..120).map(|_| rand_vec(&mut rng, 36)).collect();
        for frame in frames.iter_mut().take(60).skip(20) {
            *frame = black.clone();
        }
        frames
    };

    let ep1_raw = make_with_black_gap();
    let ep2_raw = make_with_black_gap();
    let fps = vec![
        build_fingerprint("ep1", step, &ep1_raw, None, 0.0, "visual", 8.0),
        build_fingerprint("ep2", step, &ep2_raw, None, 0.0, "visual", 8.0),
    ];

    let detected = find_shared_intros_default(&fps);
    assert!(detected.is_empty());
}

#[test]
fn mkv_and_mp4_chapter_extraction_and_server_detection() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path();

    let mkv_path = base.join("S01E01.mkv");
    fs::write(
        &mkv_path,
        build_minimal_mkv_with_chapters(&[
            (0.0, 25.0, "Cold Open"),
            (25.0, 88.0, "Opening Credits"),
            (88.0, 1200.0, "Act I"),
        ]),
    )
    .expect("write mkv");

    let mkv_chapters = extract_mkv_chapters(&mkv_path);
    assert_eq!(mkv_chapters.len(), 3);
    assert_eq!(mkv_chapters[1].title, "Opening Credits");
    let mkv_span = detect_intro_from_chapters(&mkv_chapters, 1200.0).expect("mkv intro span");
    assert!((mkv_span.start_sec - 25.0).abs() < 1e-6);
    assert!((mkv_span.end_sec - 88.0).abs() < 1e-6);
    assert_eq!(mkv_span.source, "chapters");

    let mp4_path = base.join("S01E02.mp4");
    fs::write(
        &mp4_path,
        build_minimal_mp4_with_chpl(&[(0.0, "Teaser"), (42.0, "Intro"), (102.0, "Main Episode")]),
    )
    .expect("write mp4");

    let mp4_chapters = extract_mp4_chapters(&mp4_path);
    assert_eq!(mp4_chapters.len(), 3);
    let mp4_span = detect_intro_from_chapters(&mp4_chapters, 1200.0).expect("mp4 intro span");
    assert!((mp4_span.start_sec - 42.0).abs() < 1e-6);
    assert!((mp4_span.end_sec - 102.0).abs() < 1e-6);

    let (detected, method, needs_visual) = detect_show_intros_server(
        &[
            ("ep1".to_string(), mkv_path, 1200.0),
            ("ep2".to_string(), mp4_path, 1200.0),
        ],
        false,
    );
    assert_eq!(method, "chapters");
    assert!(!needs_visual);
    let keys: HashSet<&str> = detected.keys().map(|k| k.as_str()).collect();
    assert_eq!(keys, HashSet::from(["ep1", "ep2"]));
}

#[test]
fn mkv_seekhead_jumps_past_cluster_and_corrupt_files_never_panic() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path();

    let seek_mkv = base.join("seekhead.mkv");
    fs::write(
        &seek_mkv,
        build_mkv_with_seekhead_past_cluster(&[
            (0.0, 10.0, "Cold Open"),
            (10.0, 55.0, "OP 1"),
            (55.0, 1400.0, "Part A"),
        ]),
    )
    .expect("write seekhead mkv");

    let chapters = extract_mkv_chapters(&seek_mkv);
    assert_eq!(chapters.len(), 3);
    assert_eq!(chapters[1].title, "OP 1");
    let span = detect_intro_from_chapters(&chapters, 1400.0).expect("seekhead intro");
    assert!((span.start_sec - 10.0).abs() < 1e-6);
    assert!((span.end_sec - 55.0).abs() < 1e-6);

    // Corrupt / truncated MKV and MP4 files must never panic
    let corrupt_mkv = base.join("corrupt.mkv");
    fs::write(
        &corrupt_mkv,
        b"\x1a\x45\xdf\xa3\x01\xff\xff\xff\xff\xff\xff\xff",
    )
    .expect("write");
    assert!(extract_mkv_chapters(&corrupt_mkv).is_empty());

    let corrupt_mp4 = base.join("corrupt.mp4");
    fs::write(
        &corrupt_mp4,
        b"\x00\x00\x00\x01moov\xff\xff\xff\xff\xff\xff\xff\xff",
    )
    .expect("write");
    assert!(extract_mp4_chapters(&corrupt_mp4).is_empty());
}

#[test]
fn chapter_bounds_reject_unreasonable_lengths() {
    let too_short = vec![
        ChapterMarker {
            start_sec: 0.0,
            end_sec: Some(5.0),
            title: "Intro".to_string(),
        },
        ChapterMarker {
            start_sec: 5.0,
            end_sec: Some(1000.0),
            title: "Episode".to_string(),
        },
    ];
    let too_long = vec![
        ChapterMarker {
            start_sec: 0.0,
            end_sec: Some(400.0),
            title: "Opening".to_string(),
        },
        ChapterMarker {
            start_sec: 400.0,
            end_sec: Some(1000.0),
            title: "Act 2".to_string(),
        },
    ];
    assert!(detect_intro_from_chapters(&too_short, 1000.0).is_none());
    assert!(detect_intro_from_chapters(&too_long, 1000.0).is_none());
}

#[test]
fn chapters_direct_import_and_intro_detector_reexports() {
    assert!(chapters_mod::is_intro_chapter_title("Opening Credits"));
    assert!(detector_mod::is_intro_chapter_title("Opening Credits"));
    assert!(detector_mod::is_intro_chapter_title("Main Title"));
    assert!(detector_mod::is_intro_chapter_title("OP1"));
    assert!(!detector_mod::is_intro_chapter_title("Episode 1"));

    let tmp = tempfile::tempdir().expect("tempdir");
    let mkv_path = tmp.path().join("ep.mkv");
    fs::write(
        &mkv_path,
        build_minimal_mkv_with_chapters(&[
            (0.0, 20.0, "Cold Open"),
            (20.0, 75.0, "Intro"),
            (75.0, 1200.0, "Act 1"),
        ]),
    )
    .expect("write mkv");

    let extracted_direct = chapters_mod::extract_chapters(&mkv_path);
    let extracted_reexport = detector_mod::extract_chapters(&mkv_path);
    assert_eq!(extracted_direct, extracted_reexport);

    let titles: Vec<&str> = extracted_direct.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(titles, vec!["Cold Open", "Intro", "Act 1"]);

    let span = chapters_mod::detect_intro_from_chapters(&extracted_direct, 1200.0)
        .expect("chapter intro span");
    assert!((span.start_sec - 20.0).abs() < 1e-6);
    assert!((span.end_sec - 75.0).abs() < 1e-6);
    assert!((span.duration_sec() - 55.0).abs() < 1e-6);
}

#[test]
fn media_parse_range_header_cases() {
    // Empty header -> None
    assert_eq!(parse_range_header("", 1000), Ok(None));

    // Standard closed range
    assert_eq!(parse_range_header("bytes=0-99", 1000), Ok(Some((0, 99))));
    assert_eq!(
        parse_range_header("bytes=100-500", 1000),
        Ok(Some((100, 500)))
    );

    // Open-ended range
    assert_eq!(parse_range_header("bytes=100-", 1000), Ok(Some((100, 999))));
    assert_eq!(parse_range_header("bytes=0-", 1000), Ok(Some((0, 999))));

    // Clamped end beyond EOF
    assert_eq!(
        parse_range_header("bytes=900-5000", 1000),
        Ok(Some((900, 999)))
    );

    // Suffix byte range
    assert_eq!(parse_range_header("bytes=-500", 1000), Ok(Some((500, 999))));
    assert_eq!(parse_range_header("bytes=-2000", 1000), Ok(Some((0, 999))));

    // Multi-range takes first spec
    assert_eq!(
        parse_range_header("bytes=10-20, 30-40", 1000),
        Ok(Some((10, 20)))
    );

    // Invalid / unsatisfiable ranges
    assert!(parse_range_header("items=0-10", 1000).is_err());
    assert!(parse_range_header("bytes=abc-def", 1000).is_err());
    assert!(parse_range_header("bytes=-", 1000).is_err());
    assert!(parse_range_header("bytes=-0", 1000).is_err());
    assert!(parse_range_header("bytes=0-10", 0).is_err());
    assert!(parse_range_header("bytes=1000-", 1000).is_err());
    assert!(parse_range_header("bytes=500-400", 1000).is_err());
}

#[test]
fn media_guess_video_mime_types() {
    assert_eq!(guess_video_mime(Path::new("show.mp4")), "video/mp4");
    assert_eq!(guess_video_mime(Path::new("show.M4V")), "video/mp4");
    assert_eq!(guess_video_mime(Path::new("show.webm")), "video/webm");
    assert_eq!(guess_video_mime(Path::new("show.mkv")), "video/x-matroska");
    assert_eq!(guess_video_mime(Path::new("show.mov")), "video/quicktime");
    assert_eq!(guess_video_mime(Path::new("show.avi")), "video/x-msvideo");
    assert_eq!(guess_video_mime(Path::new("show.ogv")), "video/ogg");
    assert_eq!(guess_video_mime(Path::new("show.mpg")), "video/mpeg");
    assert_eq!(guess_video_mime(Path::new("show.mpeg")), "video/mpeg");
    assert_eq!(guess_video_mime(Path::new("show.vob")), "video/mpeg");
    assert_eq!(guess_video_mime(Path::new("show.ts")), "video/mp2t");
    assert_eq!(guess_video_mime(Path::new("show.m2ts")), "video/mp2t");
    assert_eq!(guess_video_mime(Path::new("show.wmv")), "video/x-ms-wmv");
    assert_eq!(guess_video_mime(Path::new("show.flv")), "video/x-flv");
    assert_eq!(
        guess_video_mime(Path::new("file.unknownextxyz")),
        "application/octet-stream"
    );
}

#[tokio::test]
async fn media_stream_video_file_responses() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let video_path = tmp.path().join("clip.mp4");
    let payload = b"0123456789abcdefghijklmnopqrstuvwxyz";
    fs::write(&video_path, payload).expect("write clip");

    // 200 OK full file
    let resp = stream_video_file(&video_path, None, false).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()[header::CONTENT_TYPE], "video/mp4");
    assert_eq!(resp.headers()[header::ACCEPT_RANGES], "bytes");
    assert_eq!(resp.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(
        resp.headers()[header::CONTENT_LENGTH],
        payload.len().to_string()
    );
    let body = resp.into_body().collect().await.expect("body").to_bytes();
    assert_eq!(&body[..], payload);

    // 206 Partial Content
    let resp = stream_video_file(&video_path, Some("bytes=10-19"), false).await;
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        resp.headers()[header::CONTENT_RANGE],
        format!("bytes 10-19/{}", payload.len())
    );
    assert_eq!(resp.headers()[header::CONTENT_LENGTH], "10");
    let body = resp.into_body().collect().await.expect("body").to_bytes();
    assert_eq!(&body[..], b"abcdefghij");

    // HEAD returns empty body with headers intact
    let resp = stream_video_file(&video_path, Some("bytes=0-9"), true).await;
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(resp.headers()[header::CONTENT_LENGTH], "10");
    let body = resp.into_body().collect().await.expect("body").to_bytes();
    assert!(body.is_empty());

    // 416 Range Not Satisfiable
    let resp = stream_video_file(&video_path, Some("bytes=999-"), false).await;
    assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(
        resp.headers()[header::CONTENT_RANGE],
        format!("bytes */{}", payload.len())
    );
    assert_eq!(resp.headers()[header::CONTENT_LENGTH], "0");

    // 404 Not Found
    let missing = tmp.path().join("nope.mp4");
    let resp = stream_video_file(&missing, None, false).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

fn synth_speech_like_pcm(rng: &mut Rng, seconds: usize, sample_rate: usize) -> Vec<i16> {
    // Low-pass filtered noise mimics natural 1/f speech/dialogue spectral tilt
    let total = seconds * sample_rate;
    let mut pcm = Vec::with_capacity(total);
    let mut lp = 0.0_f64;
    for sec in 0..seconds {
        let formant = rng.uniform(110.0, 420.0);
        let amp = rng.uniform(1200.0, 3200.0);
        for s in 0..sample_rate {
            let t = ((sec * sample_rate + s) as f64) / (sample_rate as f64);
            let white = rng.uniform(-1.0, 1.0);
            lp = 0.85 * lp + 0.15 * white;
            let voice = (2.0 * std::f64::consts::PI * formant * t).sin() * 0.35;
            let val = ((lp * 1.8 + voice) * amp).clamp(-30000.0, 30000.0);
            pcm.push(val as i16);
        }
    }
    pcm
}

fn synth_theme_song_pcm(seconds: usize, sample_rate: usize) -> Vec<i16> {
    let notes = [
        220.0, 330.0, 440.0, 587.3, 659.3, 493.9, 784.0, 523.3, 392.0, 880.0,
    ];
    let mut pcm = Vec::with_capacity(seconds * sample_rate);
    for sec in 0..seconds {
        let f1 = notes[sec % notes.len()];
        let f2 = notes[(sec * 3 + 2) % notes.len()] * 1.5;
        let f3 = 95.0 + ((sec % 4) as f64) * 35.0;
        for s in 0..sample_rate {
            let t = ((sec * sample_rate + s) as f64) / (sample_rate as f64);
            let sig = 0.45 * (2.0 * std::f64::consts::PI * f1 * t).sin()
                + 0.35 * (2.0 * std::f64::consts::PI * f2 * t).sin()
                + 0.25 * (2.0 * std::f64::consts::PI * f3 * t).sin();
            pcm.push((sig * 9000.0).clamp(-30000.0, 30000.0) as i16);
        }
    }
    pcm
}

#[test]
fn audio_fingerprint_does_not_match_unrelated_cold_open_before_intro() {
    let sample_rate = 4000usize;
    let mut rng = Rng::new(777);

    let mut ep1_pcm = synth_speech_like_pcm(&mut rng, 150, sample_rate);
    let mut ep2_pcm = synth_speech_like_pcm(&mut rng, 150, sample_rate);
    let theme = synth_theme_song_pcm(36, sample_rate);

    // Ep 1: 25s cold open -> 36s theme (25..61)
    ep1_pcm[25 * sample_rate..61 * sample_rate].copy_from_slice(&theme);
    // Ep 2: 55s cold open -> 36s theme (55..91)
    ep2_pcm[55 * sample_rate..91 * sample_rate].copy_from_slice(&theme);

    let fp1 = extract_audio_fingerprint_from_pcm(&ep1_pcm, "ep1", 1.0).expect("fp1");
    let fp2 = extract_audio_fingerprint_from_pcm(&ep2_pcm, "ep2", 1.0).expect("fp2");

    let detected = find_shared_intros_default(&[fp1, fp2]);
    assert_eq!(detected.len(), 2);

    let s1 = &detected["ep1"];
    let s2 = &detected["ep2"];
    assert!(
        (s1.start_sec - 25.0).abs() <= 2.0,
        "ep1 intro started too early/late: got {:.2}s (expected ~25.0s)",
        s1.start_sec
    );
    assert!(
        (s1.end_sec - 61.0).abs() <= 2.0,
        "ep1 intro ended off target: got {:.2}s (expected ~61.0s)",
        s1.end_sec
    );
    assert!(
        (s2.start_sec - 55.0).abs() <= 2.0,
        "ep2 intro started too early/late: got {:.2}s (expected ~55.0s)",
        s2.start_sec
    );
    assert!(
        (s2.end_sec - 91.0).abs() <= 2.0,
        "ep2 intro ended off target: got {:.2}s (expected ~91.0s)",
        s2.end_sec
    );
}

#[test]
fn consensus_trims_pre_intro_shared_by_subset_of_episodes() {
    let mut rng = Rng::new(2026);
    let step = 1.0;
    let recap_prefix: Vec<Vec<f64>> = (0..15).map(|_| rand_vec(&mut rng, 72)).collect();
    let true_intro: Vec<Vec<f64>> = (0..35).map(|_| rand_vec(&mut rng, 72)).collect();

    // Ep1 & Ep2 share a 15s recap directly before the 35s intro; Ep3 & Ep4 only have the 35s intro
    let mut ep1_raw = make_episode_vectors(&mut rng, 150, Some(&true_intro), 30, 6.0);
    let mut ep2_raw = make_episode_vectors(&mut rng, 150, Some(&true_intro), 50, 6.0);
    for (idx, v) in recap_prefix.iter().enumerate() {
        ep1_raw[15 + idx] = perturb_vec(&mut rng, v, 6.0);
        ep2_raw[35 + idx] = perturb_vec(&mut rng, v, 6.0);
    }
    let ep3_raw = make_episode_vectors(&mut rng, 150, Some(&true_intro), 20, 6.0);
    let ep4_raw = make_episode_vectors(&mut rng, 150, Some(&true_intro), 60, 6.0);

    let fps = vec![
        build_fingerprint("ep1", step, &ep1_raw, None, 0.0, "visual", 8.0),
        build_fingerprint("ep2", step, &ep2_raw, None, 0.0, "visual", 8.0),
        build_fingerprint("ep3", step, &ep3_raw, None, 0.0, "visual", 8.0),
        build_fingerprint("ep4", step, &ep4_raw, None, 0.0, "visual", 8.0),
    ];

    let detected = find_shared_intros_default(&fps);
    for (id, expected_start) in [("ep1", 30.0), ("ep2", 50.0), ("ep3", 20.0), ("ep4", 60.0)] {
        let span = &detected[id];
        assert!(
            (span.start_sec - expected_start).abs() <= 2.0,
            "{id}: expected intro start ~{expected_start}s (after recap), got {:.2}s",
            span.start_sec
        );
    }
}

#[test]
fn stray_pre_intro_frames_and_fade_do_not_shift_start_early() {
    let mut rng = Rng::new(31415);
    let step = 1.0;
    let true_intro: Vec<Vec<f64>> = (0..35).map(|_| rand_vec(&mut rng, 72)).collect();
    let stray: Vec<Vec<f64>> = (0..2).map(|_| rand_vec(&mut rng, 72)).collect();
    let black = vec![2.0; 72];

    let mut ep1_raw = make_episode_vectors(&mut rng, 140, Some(&true_intro), 40, 5.0);
    let mut ep2_raw = make_episode_vectors(&mut rng, 140, Some(&true_intro), 60, 5.0);

    // Put 2 stray matching frames + 4 black fade frames directly before the true intro
    ep1_raw[34..36].clone_from_slice(&stray);
    ep2_raw[54..56].clone_from_slice(&stray);
    for k in 0..4 {
        ep1_raw[36 + k] = black.clone();
        ep2_raw[56 + k] = black.clone();
    }

    let fps = vec![
        build_fingerprint("ep1", step, &ep1_raw, None, 0.0, "visual", 8.0),
        build_fingerprint("ep2", step, &ep2_raw, None, 0.0, "visual", 8.0),
    ];

    let detected = find_shared_intros_default(&fps);
    assert!(
        (detected["ep1"].start_sec - 40.0).abs() <= 2.0,
        "ep1 start bled early: {:.2}s",
        detected["ep1"].start_sec
    );
    assert!(
        (detected["ep2"].start_sec - 60.0).abs() <= 2.0,
        "ep2 start bled early: {:.2}s",
        detected["ep2"].start_sec
    );
}

#[test]
fn opening_logo_and_intro_never_bridge_real_content_gap() {
    let mut rng = Rng::new(9001);
    let step = 1.0;

    // Every episode begins with an identical 14s studio/network bumper at 0..14s,
    // followed by real unique cold-open content (16s), then the real 45s intro at 30..75s!
    // Even on diagonal d = 0 (where both bumper and intro align identically across episodes)
    // and with a couple of accidental matching frames + fades inside the cold open,
    // the detector must NEVER connect 0..14s and 30..75s across the real content.
    let opening_bumper: Vec<Vec<f64>> = (0..14).map(|_| rand_vec(&mut rng, 72)).collect();
    let true_intro: Vec<Vec<f64>> = (0..45).map(|_| rand_vec(&mut rng, 72)).collect();
    let lucky_frame = rand_vec(&mut rng, 72);
    let black = vec![2.0; 72];

    let mut fps = Vec::new();
    for (id, intro_start) in [("ep1", 30), ("ep2", 30), ("ep3", 30), ("ep4", 42)] {
        let mut frames = make_episode_vectors(&mut rng, 140, Some(&true_intro), intro_start, 5.0);
        for (k, v) in opening_bumper.iter().enumerate() {
            frames[k] = perturb_vec(&mut rng, v, 5.0);
        }
        // Inject a fade and an isolated lucky matching frame inside the real cold open
        frames[14] = black.clone();
        frames[19] = perturb_vec(&mut rng, &lucky_frame, 4.0);
        frames[29] = black.clone();
        fps.push(build_fingerprint(
            id, step, &frames, None, 0.0, "visual", 8.0,
        ));
    }

    let detected = find_shared_intros_default(&fps);
    assert_eq!(detected.len(), 4);
    for (id, expected_start) in [("ep1", 30.0), ("ep2", 30.0), ("ep3", 30.0), ("ep4", 42.0)] {
        let span = &detected[id];
        assert!(
            (span.start_sec - expected_start).abs() <= 2.0,
            "{id}: bridged opening bumper across real content! got {:.2}s..{:.2}s (expected ~{expected_start}s..{}s)",
            span.start_sec,
            span.end_sec,
            expected_start + 45.0,
        );
        assert!(
            (span.end_sec - (expected_start + 45.0)).abs() <= 2.0,
            "{id}: end off target: got {:.2}s (expected ~{}s)",
            span.end_sec,
            expected_start + 45.0,
        );
    }
}

#[test]
fn opening_fanfare_and_audio_theme_never_bridge_cold_open() {
    let sample_rate = 4000usize;
    let mut rng = Rng::new(4242);

    // 10s shared studio fanfare at 0..10s, 14s unique dialogue cold open at 10..24s,
    // then 38s shared theme song at 24..62s on the exact same diagonal (d = 0).
    let fanfare = synth_theme_song_pcm(10, sample_rate);
    let theme = synth_theme_song_pcm(38, sample_rate);

    let mut ep1_pcm = synth_speech_like_pcm(&mut rng, 130, sample_rate);
    let mut ep2_pcm = synth_speech_like_pcm(&mut rng, 130, sample_rate);
    ep1_pcm[0..10 * sample_rate].copy_from_slice(&fanfare);
    ep2_pcm[0..10 * sample_rate].copy_from_slice(&fanfare);
    ep1_pcm[24 * sample_rate..62 * sample_rate].copy_from_slice(&theme);
    ep2_pcm[24 * sample_rate..62 * sample_rate].copy_from_slice(&theme);

    let fp1 = extract_audio_fingerprint_from_pcm(&ep1_pcm, "ep1", 1.0).expect("fp1");
    let fp2 = extract_audio_fingerprint_from_pcm(&ep2_pcm, "ep2", 1.0).expect("fp2");

    let detected = find_shared_intros_default(&[fp1, fp2]);
    assert_eq!(detected.len(), 2);
    for id in ["ep1", "ep2"] {
        let span = &detected[id];
        assert!(
            (span.start_sec - 24.0).abs() <= 2.0,
            "{id}: audio bridged opening fanfare across cold open! got {:.2}s..{:.2}s (expected ~24..62s)",
            span.start_sec,
            span.end_sec
        );
        assert!(
            (span.end_sec - 62.0).abs() <= 2.0,
            "{id}: audio end off target: got {:.2}s (expected ~62s)",
            span.end_sec
        );
    }
}
