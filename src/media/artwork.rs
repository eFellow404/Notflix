//! Local show poster discovery and episode thumbnail discovery/extraction.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::ffmpeg_bin::resolve_ffmpeg;

const THUMB_EXTRACT_TIMEOUT: Duration = Duration::from_secs(8);
const THUMB_PRIMARY_SEEK: &str = "00:01:30";
const THUMB_FALLBACK_SEEK: &str = "00:00:03";

/// Supported local artwork extensions in deterministic tie-break priority order.
const ARTWORK_IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "webp"];

/// Guess the MIME type for a local poster or thumbnail image file.
pub fn guess_image_mime(path: &Path) -> &'static str {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return "application/octet-stream";
    };
    if ext.eq_ignore_ascii_case("jpg") || ext.eq_ignore_ascii_case("jpeg") {
        "image/jpeg"
    } else if ext.eq_ignore_ascii_case("png") {
        "image/png"
    } else if ext.eq_ignore_ascii_case("webp") {
        "image/webp"
    } else if ext.eq_ignore_ascii_case("gif") {
        "image/gif"
    } else {
        "application/octet-stream"
    }
}

fn artwork_ext_rank(path: &Path) -> Option<usize> {
    let ext = path.extension()?.to_str()?;
    ARTWORK_IMAGE_EXTS
        .iter()
        .position(|candidate| ext.eq_ignore_ascii_case(candidate))
}

/// Scan `dir` for the highest-priority image file according to `stem_rank_fn`, breaking ties by
/// [`ARTWORK_IMAGE_EXTS`] order and then lowercase filename.
///
/// Checks extension and stem before falling back to symlink `stat` checks so non-matching entries
/// and regular files avoid redundant `stat` syscalls.
fn find_best_ranked_image<F>(dir: &Path, mut stem_rank_fn: F) -> Option<PathBuf>
where
    F: FnMut(&str) -> Option<usize>,
{
    let entries = std::fs::read_dir(dir).ok()?;
    let mut best: Option<(usize, usize, String, PathBuf)> = None;

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(ext_rank) = artwork_ext_rank(&path) else {
            continue;
        };
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(stem_rank) = stem_rank_fn(stem) else {
            continue;
        };
        let is_file = match entry.file_type() {
            Ok(ft) if ft.is_file() => true,
            Ok(ft) if ft.is_symlink() => path.is_file(),
            Ok(_) => false,
            Err(_) => path.is_file(),
        };
        if !is_file {
            continue;
        }

        let filename_lower = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_lowercase();

        let candidate = (stem_rank, ext_rank, filename_lower, path);
        let replace = match &best {
            None => true,
            Some(current) => {
                (candidate.0, candidate.1, &candidate.2) < (current.0, current.1, &current.2)
            }
        };
        if replace {
            best = Some(candidate);
        }
    }

    best.map(|(_, _, _, path)| path)
}

/// Search `folder_path` for a local show poster image matching priority stems
/// `poster`, `folder`, `cover`, `show`, `fanart`, `backdrop` (case-insensitively).
pub fn find_show_poster(folder_path: &Path) -> Option<PathBuf> {
    const POSTER_STEMS: &[&str] = &["poster", "folder", "cover", "show", "fanart", "backdrop"];

    find_best_ranked_image(folder_path, |stem| {
        POSTER_STEMS
            .iter()
            .position(|candidate| stem.eq_ignore_ascii_case(candidate))
    })
}

fn find_sidecar_thumbnail(video_path: &Path) -> Option<PathBuf> {
    let raw_parent = video_path.parent()?;
    let parent = if raw_parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        raw_parent
    };
    let video_stem = video_path.file_stem()?.to_str()?;
    if video_stem.is_empty() {
        return None;
    }
    let stem_lower = video_stem.to_lowercase();
    let thumb_stem_lower = format!("{stem_lower}-thumb");

    find_best_ranked_image(parent, |entry_stem| {
        let entry_stem_lower = entry_stem.to_lowercase();
        if entry_stem_lower == stem_lower {
            Some(0)
        } else if entry_stem_lower == thumb_stem_lower {
            Some(1)
        } else {
            None
        }
    })
}

fn sanitize_episode_id(episode_id: &str) -> String {
    let safe: String = episode_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() {
        "episode".to_string()
    } else {
        safe
    }
}

enum ExtractResult {
    Ok,
    Failed,
    FfmpegMissing,
}

fn try_extract_thumbnail(
    ffmpeg_bin: &Path,
    video_path: &Path,
    tmp_path: &Path,
    seek_ts: &str,
) -> ExtractResult {
    let mut cmd = std::process::Command::new(ffmpeg_bin);
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-ss",
        seek_ts,
        "-i",
    ])
    .arg(video_path)
    .args(["-frames:v", "1", "-vf", "scale=480:-2", "-q:v", "4"])
    .arg(tmp_path);

    let outcome = crate::util::run_command_with_timeout(&mut cmd, THUMB_EXTRACT_TIMEOUT, false);
    if outcome.spawn_error.is_some() {
        return ExtractResult::FfmpegMissing;
    }
    let non_empty = outcome.success()
        && std::fs::metadata(tmp_path)
            .map(|m| m.is_file() && m.len() > 0)
            .unwrap_or(false);
    if non_empty {
        ExtractResult::Ok
    } else {
        ExtractResult::Failed
    }
}

/// Return a sidecar episode thumbnail, a cached generated thumbnail, or extract a new
/// JPEG frame into `cache_dir` via `ffmpeg`.
pub fn find_or_generate_episode_thumbnail(
    video_path: &Path,
    cache_dir: &Path,
    episode_id: &str,
) -> Option<PathBuf> {
    // Step 1: Sidecar image alongside the video file.
    if let Some(sidecar) = find_sidecar_thumbnail(video_path) {
        return Some(sidecar);
    }

    // Step 2: Existing non-empty cached thumbnail.
    let safe_id = sanitize_episode_id(episode_id);
    let cached = cache_dir.join(format!("{safe_id}.jpg"));
    if std::fs::metadata(&cached)
        .map(|m| m.is_file() && m.len() > 0)
        .unwrap_or(false)
    {
        return Some(cached);
    }

    // Step 3: Extract frame with ffmpeg into a unique temp file and atomically rename.
    if !video_path.is_file() {
        return None;
    }
    let ffmpeg_bin = resolve_ffmpeg()?;
    std::fs::create_dir_all(cache_dir).ok()?;

    let tmp_path = cache_dir.join(format!(".{safe_id}.{}.tmp.jpg", uuid::Uuid::new_v4()));
    let extracted =
        match try_extract_thumbnail(&ffmpeg_bin, video_path, &tmp_path, THUMB_PRIMARY_SEEK) {
            ExtractResult::Ok => true,
            ExtractResult::FfmpegMissing => false,
            ExtractResult::Failed => {
                let _ = std::fs::remove_file(&tmp_path);
                matches!(
                    try_extract_thumbnail(&ffmpeg_bin, video_path, &tmp_path, THUMB_FALLBACK_SEEK),
                    ExtractResult::Ok
                )
            }
        };

    if !extracted {
        let _ = std::fs::remove_file(&tmp_path);
        return None;
    }

    if std::fs::rename(&tmp_path, &cached).is_ok() {
        return Some(cached);
    }

    // Handle concurrent writer race (particularly on Windows where rename does not overwrite).
    let _ = std::fs::remove_file(&tmp_path);
    if std::fs::metadata(&cached)
        .map(|m| m.is_file() && m.len() > 0)
        .unwrap_or(false)
    {
        Some(cached)
    } else {
        None
    }
}
