//! Cross-episode TV intro detection engine.
//!
//! Combines three complementary strategies with zero mandatory external dependencies:
//! 1. Container chapter markers (pure-Rust MKV/WebM EBML and MP4 `chpl` parsing via [`crate::chapters`]).
//! 2. Server-side acoustic fingerprinting + cross-episode diagonal alignment when `ffmpeg` is on `PATH`.
//! 3. Shared fingerprint consensus matching that also accepts browser-extracted visual frame fingerprints.

mod audio;
mod matching;

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub use crate::chapters::{
    detect_intro_from_chapters, extract_chapters, extract_mkv_chapters, extract_mp4_chapters,
    is_intro_chapter_title, ChapterMarker,
};
pub use audio::{
    extract_audio_fingerprint, extract_audio_fingerprint_from_pcm, DEFAULT_AUDIO_SCAN_SEC,
};
pub use matching::{find_shared_intros, find_shared_intros_default};

/// Minimum valid intro length in seconds.
pub const MIN_INTRO_SEC: f64 = 12.0;
/// Maximum valid intro length in seconds.
pub const MAX_INTRO_SEC: f64 = 180.0;

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

/// Time-series feature vectors for the opening portion of an episode.
#[derive(Debug, Clone)]
pub struct EpisodeFingerprint {
    pub episode_id: String,
    pub step_sec: f64,
    pub vectors: Vec<Vec<f64>>,
    pub flat_flags: Vec<bool>,
    pub duration_sec: f64,
    pub source: String,
}

/// Zero-mean, unit-L2 normalize a feature vector in a single variance pass.
///
/// Near-constant vectors (black frames, solid fades, digital silence) or vectors whose
/// standard deviation falls below `min_std` are flagged as flat so they never produce
/// false matches across episodes.
fn normalize_vector(raw: &[f64], min_std: f64) -> (Vec<f64>, bool) {
    let n = raw.len();
    if n == 0 {
        return (Vec::new(), true);
    }
    let n_f64 = n as f64;
    let mean = raw.iter().sum::<f64>() / n_f64;
    let mut centered = Vec::with_capacity(n);
    let mut norm_sq = 0.0;
    for &v in raw {
        let c = v - mean;
        norm_sq += c * c;
        centered.push(c);
    }
    if norm_sq <= 1e-8 {
        centered.fill(0.0);
        return (centered, true);
    }
    let inv_norm = 1.0 / norm_sq.sqrt();
    for c in &mut centered {
        *c *= inv_norm;
    }
    let below_min_std = min_std > 0.0 && (norm_sq / n_f64).sqrt() < min_std;
    (centered, below_min_std)
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
    let step = step_sec.max(0.25);
    let mut norm_vecs = Vec::with_capacity(raw_vectors.len());
    let mut flats = Vec::with_capacity(raw_vectors.len());

    for (idx, raw) in raw_vectors.iter().enumerate() {
        let user_flat = flat_flags
            .and_then(|flags| flags.get(idx).copied())
            .unwrap_or(false);
        let effective_min_std = if user_flat { 0.0 } else { min_std };
        let (vec, vec_flat) = normalize_vector(raw, effective_min_std);
        norm_vecs.push(vec);
        flats.push(user_flat || vec_flat);
    }

    EpisodeFingerprint {
        episode_id: episode_id.into(),
        step_sec: step,
        vectors: norm_vecs,
        flat_flags: flats,
        duration_sec: duration_sec.max(0.0),
        source: source.into(),
    }
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
    if use_ffmpeg && which::which("ffmpeg").is_ok() {
        let mut fps = Vec::new();
        for (ep_id, path, _) in episodes {
            if let Some(fp) = extract_audio_fingerprint(path, ep_id, DEFAULT_AUDIO_SCAN_SEC, 1.0) {
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
