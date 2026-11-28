//! Cross-episode TV intro detection engine.
//!
//! Combines three complementary strategies with zero mandatory external dependencies:
//! 1. Container chapter markers (pure-Rust MKV/WebM EBML and MP4 `chpl` parsing via [`crate::chapters`]).
//! 2. Server-side acoustic fingerprinting + cross-episode diagonal alignment when `ffmpeg` is available (system `PATH` or auto-downloaded).
//! 3. Shared fingerprint consensus matching that also accepts browser-extracted visual frame fingerprints.

mod audio;
mod matching;

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ffmpeg_bin::resolve_ffmpeg;

pub use crate::chapters::{
    detect_intro_from_chapters, detect_outro_from_chapters, extract_chapters, extract_mkv_chapters,
    extract_mp4_chapters, is_intro_chapter_title, is_outro_chapter_title, ChapterMarker,
};
pub use audio::{
    extract_audio_fingerprint, extract_audio_fingerprint_from_pcm, extract_tail_audio_fingerprint,
    DEFAULT_AUDIO_SCAN_SEC,
};
pub use matching::{
    discover_intro_template, discover_intro_template_default, find_shared_intros,
    find_shared_intros_default, find_shared_outros, match_episodes_against_template,
};

/// Minimum valid intro length in seconds.
pub const MIN_INTRO_SEC: f64 = 12.0;
/// Maximum valid intro length in seconds.
pub const MAX_INTRO_SEC: f64 = 180.0;

const MIN_STEP_SEC: f64 = 0.25;
const SHORT_EPISODE_CAP_MIN_DUR_SEC: f64 = 120.0;
const SHORT_EPISODE_SCAN_FRACTION: f64 = 0.55;
const OUTRO_TAIL_SCAN_SEC: f64 = 180.0;
const OUTRO_TAIL_STEP_SEC: f64 = 0.5;

/// Detected or manually assigned intro timestamp range for an episode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntroSpan {
    pub start_sec: f64,
    pub end_sec: f64,
    pub confidence: f64,
    pub source: String,
}

impl IntroSpan {
    /// Duration of the intro span in seconds, clamped to `>= 0.0`.
    pub fn duration_sec(&self) -> f64 {
        (self.end_sec - self.start_sec).max(0.0)
    }
}

/// Consensus intro feature template discovered from reference episodes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntroTemplate {
    pub step_sec: f64,
    pub vectors: Vec<Vec<f64>>,
    pub flat_flags: Vec<bool>,
    pub weights: Vec<f64>,
    pub sim_threshold: f64,
    pub source: String,
}

/// Time-series feature vectors for the opening or windowed portion of an episode.
#[derive(Debug, Clone)]
pub struct EpisodeFingerprint {
    pub episode_id: String,
    pub step_sec: f64,
    pub vectors: Vec<Vec<f64>>,
    pub flat_flags: Vec<bool>,
    pub duration_sec: f64,
    pub source: String,
    pub offset_sec: f64,
}

impl EpisodeFingerprint {
    /// Set the starting timestamp offset (seconds) for a windowed fingerprint.
    pub fn with_offset(mut self, offset_sec: f64) -> Self {
        self.offset_sec = if offset_sec.is_finite() {
            offset_sec.max(0.0)
        } else {
            0.0
        };
        self
    }
}

/// Zero-mean, unit-L2 normalize a feature vector in place in a single variance pass.
///
/// Near-constant vectors (black frames, solid fades, digital silence) or vectors whose
/// standard deviation falls below `min_std` are flagged as flat so they never produce
/// false matches across episodes.
fn normalize_vector_in_place(vec: &mut [f64], min_std: f64) -> bool {
    let n = vec.len();
    if n == 0 {
        return true;
    }
    let n_f64 = n as f64;
    let mean = vec.iter().sum::<f64>() / n_f64;
    let mut norm_sq = 0.0;
    for v in vec.iter_mut() {
        let c = *v - mean;
        norm_sq += c * c;
        *v = c;
    }
    if norm_sq <= 1e-8 {
        vec.fill(0.0);
        return true;
    }
    let inv_norm = 1.0 / norm_sq.sqrt();
    for c in vec.iter_mut() {
        *c *= inv_norm;
    }
    min_std > 0.0 && (norm_sq / n_f64).sqrt() < min_std
}

/// Normalize owned per-frame vectors in place into an [`EpisodeFingerprint`] without
/// double-allocating `Vec<Vec<f64>>`.
fn build_fingerprint_owned(
    episode_id: impl Into<String>,
    step_sec: f64,
    mut raw_vectors: Vec<Vec<f64>>,
    mut flat_flags: Vec<bool>,
    duration_sec: f64,
    source: impl Into<String>,
    min_std: f64,
) -> EpisodeFingerprint {
    let step = step_sec.max(MIN_STEP_SEC);
    flat_flags.resize(raw_vectors.len(), false);
    for (vec, user_flat) in raw_vectors.iter_mut().zip(flat_flags.iter_mut()) {
        let effective_min_std = if *user_flat { 0.0 } else { min_std };
        let vec_flat = normalize_vector_in_place(vec, effective_min_std);
        *user_flat = *user_flat || vec_flat;
    }

    EpisodeFingerprint {
        episode_id: episode_id.into(),
        step_sec: step,
        vectors: raw_vectors,
        flat_flags,
        duration_sec: duration_sec.max(0.0),
        source: source.into(),
        offset_sec: 0.0,
    }
}

/// Normalize raw per-frame vectors into an [`EpisodeFingerprint`].
pub fn build_fingerprint(
    episode_id: impl Into<String>,
    step_sec: f64,
    raw_vectors: &[Vec<f64>],
    flat_flags: Option<&[bool]>,
    duration_sec: f64,
    source: impl Into<String>,
    min_std: f64,
) -> EpisodeFingerprint {
    let owned_vecs = raw_vectors.to_vec();
    let owned_flats = flat_flags.map_or_else(Vec::new, |flags| {
        flags.iter().take(raw_vectors.len()).copied().collect()
    });
    build_fingerprint_owned(
        episode_id,
        step_sec,
        owned_vecs,
        owned_flats,
        duration_sec,
        source,
        min_std,
    )
}

/// Run server-side intro detection for a show's episodes.
///
/// `episodes` is a slice of `(episode_id, file_path, duration_sec)` tuples.
/// Returns `(detected_map, method_used, needs_client_visual_scan)`.
pub fn detect_show_intros_server(
    episodes: &[(String, PathBuf, f64)],
    use_ffmpeg: bool,
) -> (HashMap<String, IntroSpan>, String, bool) {
    let mut detected: HashMap<String, IntroSpan> = HashMap::new();

    // Layer 1: Fast container chapter markers (works with zero external dependencies)
    for (ep_id, path, dur) in episodes {
        if !path.exists() {
            continue;
        }
        let chapters = extract_chapters(path);
        if let Some(span) = detect_intro_from_chapters(&chapters, *dur) {
            detected.insert(ep_id.clone(), span);
        }
    }

    if !detected.is_empty() && detected.len() >= (episodes.len() / 2).max(1) {
        return (detected, "chapters".to_string(), false);
    }

    if episodes.len() < 2 {
        let method = if detected.is_empty() {
            "insufficient_episodes"
        } else {
            "chapters"
        };
        return (detected, method.to_string(), false);
    }

    // Layer 2: FFmpeg acoustic fingerprinting when installed
    if use_ffmpeg && resolve_ffmpeg().is_some() {
        let mut fps = Vec::new();
        for (ep_id, path, dur) in episodes {
            let max_sec = if dur.is_finite() && *dur > SHORT_EPISODE_CAP_MIN_DUR_SEC {
                (*dur * SHORT_EPISODE_SCAN_FRACTION).min(DEFAULT_AUDIO_SCAN_SEC)
            } else {
                DEFAULT_AUDIO_SCAN_SEC
            };
            if let Some(fp) = extract_audio_fingerprint(path, ep_id, max_sec, 1.0) {
                fps.push(fp);
            }
        }
        if fps.len() >= 2 {
            let audio_matches = find_shared_intros_default(&fps);
            if !audio_matches.is_empty() {
                for (ep_id, span) in audio_matches {
                    // Keep explicit named chapter matches if already found
                    detected.entry(ep_id).or_insert(span);
                }
                return (detected, "audio".to_string(), false);
            }
        }
    }

    // Layer 3: Signal browser to run zero-dependency visual frame fingerprinting
    let method = if detected.is_empty() {
        "visual_needed"
    } else {
        "chapters"
    };
    (detected, method.to_string(), true)
}

/// Run server-side outro/credits detection for a show's episodes.
///
/// Tier 1 parses container chapter markers (MKV/WebM/MP4) with zero external dependencies.
/// Tier 2 optionally extracts tail audio fingerprints via `ffmpeg` and matches repeating
/// ending sequences across episodes not already detected from chapters.
pub fn detect_show_outros_server(
    episodes: &[(String, PathBuf, f64)],
    use_ffmpeg: bool,
) -> HashMap<String, IntroSpan> {
    let mut results: HashMap<String, IntroSpan> = HashMap::new();

    // Tier 1: Container chapter markers
    for (ep_id, path, dur) in episodes {
        if !path.exists() {
            continue;
        }
        let chapters = extract_chapters(path);
        if let Some(span) = detect_outro_from_chapters(&chapters, *dur) {
            results.insert(ep_id.clone(), span);
        }
    }

    // Tier 2: Optional FFmpeg tail acoustic fingerprinting for remaining episodes
    if use_ffmpeg
        && episodes.len() >= 2
        && results.len() < episodes.len()
        && resolve_ffmpeg().is_some()
    {
        let mut fps = Vec::new();
        for (ep_id, path, dur) in episodes {
            if !path.exists() {
                continue;
            }
            if !dur.is_finite() || *dur < 0.0 || (*dur > 0.0 && *dur < OUTRO_TAIL_SCAN_SEC) {
                continue;
            }
            if let Some(fp) = extract_tail_audio_fingerprint(
                path,
                ep_id,
                *dur,
                OUTRO_TAIL_SCAN_SEC,
                OUTRO_TAIL_STEP_SEC,
            ) {
                fps.push(fp);
            }
        }
        if fps.len() >= 2 {
            let audio_matches = find_shared_outros(&fps);
            for (ep_id, span) in audio_matches {
                results.entry(ep_id).or_insert(span);
            }
        }
    }

    results
}
