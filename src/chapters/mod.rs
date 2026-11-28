//! Pure-Rust container chapter extraction (MKV/WebM EBML and MP4 `chpl`).

mod mkv;
mod mp4;

use std::io::Read;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use crate::intro_detector::{IntroSpan, MAX_INTRO_SEC, MIN_INTRO_SEC};

pub use mkv::extract_mkv_chapters;
pub use mp4::extract_mp4_chapters;

/// Maximum starting timestamp (seconds) for an intro chapter marker.
const MAX_INTRO_START_SEC: f64 = 600.0;
/// Maximum episode fraction where an intro chapter marker may begin.
const MAX_INTRO_START_FRACTION: f64 = 0.45;
/// Minimum episode fraction where an outro/credits chapter marker may begin.
const MIN_OUTRO_START_FRACTION: f64 = 0.55;
/// Trailing margin (seconds) before episode end excluded from outro chapter starts.
const OUTRO_END_MARGIN_SEC: f64 = 2.0;

static INTRO_CHAPTER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)",
        r"(?:^|[\s\-_:/(\[])",
        r"(?:intro(?:duction)?|opening(?:\s+(?:credits?|titles?|theme|sequence))?|",
        r"main\s+titles?|title\s+sequence|theme\s+song|op(?:\s*\d+)?)",
        r"(?:$|[\s\-_:/)\]])",
    ))
    .expect("INTRO_CHAPTER_RE must be a valid regular expression")
});

static OUTRO_CHAPTER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)",
        r"(?:^|[\s\-_:/(\[])",
        r"(?:(?:end|closing)[\s\-_]+(?:credits?|titles?|theme)|credits?|",
        r"ending(?:[\s\-_]+(?:credits?|titles?|theme|sequence))?|",
        r"closing(?:[\s\-_]+(?:credits?|titles?|theme|sequence))?|",
        r"outro|ed(?:\s*\d+)?|",
        r"next[\s\-_]+(?:time|episode)|preview|epilogue)",
        r"(?:$|[\s\-_:/)\]])",
    ))
    .expect("OUTRO_CHAPTER_RE must be a valid regular expression")
});

/// A chapter entry extracted from an MKV/WebM or MP4 container.
#[derive(Debug, Clone, PartialEq)]
pub struct ChapterMarker {
    pub start_sec: f64,
    pub end_sec: Option<f64>,
    pub title: String,
}

/// Return `true` if a chapter title explicitly labels an intro/opening sequence.
pub fn is_intro_chapter_title(title: &str) -> bool {
    !title.is_empty() && INTRO_CHAPTER_RE.is_match(title)
}

/// Return `true` if a chapter title explicitly labels an outro, end credits, or preview.
pub fn is_outro_chapter_title(title: &str) -> bool {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.contains("opening")
        || lower.contains("intro")
        || lower.contains("cold open")
        || lower.contains("cold-open")
        || lower.contains("cold_open")
    {
        return false;
    }
    OUTRO_CHAPTER_RE.is_match(trimmed)
}

/// Read until `buf` is full or EOF is reached, returning total bytes read.
fn read_exact_count<R: Read>(fp: &mut R, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match fp.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Read up to `limit` bytes from `fp` without preallocating unbounded memory.
fn read_up_to<R: Read>(fp: &mut R, limit: usize) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(limit.min(64 * 1024));
    fp.take(limit as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Extract container chapters for supported video formats (MKV, WebM, MP4, M4V, MOV).
pub fn extract_chapters(file_path: &Path) -> Vec<ChapterMarker> {
    let Some(ext) = file_path.extension().and_then(|e| e.to_str()) else {
        return Vec::new();
    };
    if ext.eq_ignore_ascii_case("mkv") || ext.eq_ignore_ascii_case("webm") {
        extract_mkv_chapters(file_path)
    } else if ext.eq_ignore_ascii_case("mp4")
        || ext.eq_ignore_ascii_case("m4v")
        || ext.eq_ignore_ascii_case("mov")
    {
        extract_mp4_chapters(file_path)
    } else {
        Vec::new()
    }
}

/// Return an `IntroSpan` if chapter markers explicitly label an opening/intro.
pub fn detect_intro_from_chapters(
    chapters: &[ChapterMarker],
    duration_sec: f64,
) -> Option<IntroSpan> {
    if chapters.is_empty() {
        return None;
    }

    for (idx, ch) in chapters.iter().enumerate() {
        if !is_intro_chapter_title(&ch.title) {
            continue;
        }
        let start = ch.start_sec.max(0.0);
        let end = ch
            .end_sec
            .or_else(|| chapters.get(idx + 1).map(|next| next.start_sec));
        let Some(end) = end else {
            continue;
        };
        let length = end - start;
        if !(MIN_INTRO_SEC..=MAX_INTRO_SEC).contains(&length) {
            continue;
        }
        if start > MAX_INTRO_START_SEC {
            continue;
        }
        if duration_sec > 0.0 && start > duration_sec * MAX_INTRO_START_FRACTION {
            continue;
        }
        return Some(IntroSpan {
            start_sec: start,
            end_sec: end,
            confidence: 0.95,
            // Preserve plural `"chapters"` verbatim for API/DB backwards compatibility.
            source: "chapters".to_string(),
        });
    }
    None
}

/// Return an `IntroSpan` if chapter markers explicitly label an outro/credits section
/// in the latter portion of the episode.
pub fn detect_outro_from_chapters(
    chapters: &[ChapterMarker],
    duration_sec: f64,
) -> Option<IntroSpan> {
    if chapters.is_empty() {
        return None;
    }

    let eff_dur = if duration_sec.is_finite() && duration_sec > 0.0 {
        duration_sec
    } else {
        chapters
            .iter()
            .filter_map(|ch| ch.end_sec)
            .filter(|v| v.is_finite() && *v > 0.0)
            .fold(0.0_f64, f64::max)
    };

    let earliest = chapters
        .iter()
        .filter(|ch| {
            if !is_outro_chapter_title(&ch.title) {
                return false;
            }
            if !ch.start_sec.is_finite() || ch.start_sec < 0.0 {
                return false;
            }
            if ch.start_sec < eff_dur * MIN_OUTRO_START_FRACTION {
                return false;
            }
            if eff_dur > 0.0 && ch.start_sec >= eff_dur - OUTRO_END_MARGIN_SEC {
                return false;
            }
            true
        })
        .min_by(|a, b| a.start_sec.total_cmp(&b.start_sec))?;

    let end_sec = eff_dur
        .max(earliest.end_sec.unwrap_or(0.0))
        .max(earliest.start_sec);

    Some(IntroSpan {
        start_sec: earliest.start_sec,
        end_sec,
        confidence: 0.96,
        // Preserve singular `"chapter"` verbatim for API/DB backwards compatibility.
        source: "chapter".to_string(),
    })
}
