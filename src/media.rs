//! Media MIME helpers, HTTP 206 byte-range video streaming, audio track probing,
//! on-the-fly ffmpeg remuxing, and local artwork discovery.

use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, ReadBuf};
use tokio_util::io::ReaderStream;

/// 256 KiB chunks for smooth seeking and low memory footprint.
pub const CHUNK_SIZE: usize = 256 * 1024;

/// 64 KiB chunks for live ffmpeg remux pipes.
const REMUX_CHUNK_SIZE: usize = 64 * 1024;

/// Return `true` if `ffmpeg` is available on `PATH`.
pub fn detect_ffmpeg() -> bool {
    which::which("ffmpeg").is_ok()
}

/// Match common video file extensions case-insensitively without allocating.
fn known_video_mime(ext: &str) -> Option<&'static str> {
    const TABLE: &[(&str, &str)] = &[
        ("mp4", "video/mp4"),
        ("m4v", "video/mp4"),
        ("webm", "video/webm"),
        ("mkv", "video/x-matroska"),
        ("mov", "video/quicktime"),
        ("avi", "video/x-msvideo"),
        ("ogv", "video/ogg"),
        ("mpg", "video/mpeg"),
        ("mpeg", "video/mpeg"),
        ("vob", "video/mpeg"),
        ("ts", "video/mp2t"),
        ("m2ts", "video/mp2t"),
        ("wmv", "video/x-ms-wmv"),
        ("flv", "video/x-flv"),
    ];
    TABLE
        .iter()
        .find(|&&(candidate, _)| ext.eq_ignore_ascii_case(candidate))
        .map(|&(_, mime)| mime)
}

/// Guess the MIME type for a video file using an explicit video table and `mime_guess` fallback.
pub fn guess_video_mime(file_path: &Path) -> String {
    if let Some(mime) = file_path
        .extension()
        .and_then(|e| e.to_str())
        .and_then(known_video_mime)
    {
        return mime.to_string();
    }

    mime_guess::from_path(file_path)
        .first_raw()
        .unwrap_or("application/octet-stream")
        .to_string()
}

/// Parse an HTTP `Range` header into an inclusive `(start, end)` byte tuple.
///
/// Returns `Ok(None)` if `range_header` is empty, or `Err` if the requested byte
/// range is malformed or unsatisfiable.
pub fn parse_range_header(
    range_header: &str,
    file_size: u64,
) -> std::result::Result<Option<(u64, u64)>, String> {
    if range_header.is_empty() {
        return Ok(None);
    }

    let spec = range_header.split(',').next().unwrap_or("").trim();
    let Some(rest) = spec.strip_prefix("bytes=") else {
        return Err(format!("Malformed Range header: {range_header}"));
    };
    let Some((start_str, end_str)) = rest.split_once('-') else {
        return Err(format!("Malformed Range header: {range_header}"));
    };
    if !start_str.bytes().all(|b| b.is_ascii_digit())
        || !end_str.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(format!("Malformed Range header: {range_header}"));
    }

    if start_str.is_empty() && end_str.is_empty() {
        return Err("Empty byte range".to_string());
    }
    if file_size == 0 {
        return Err("Cannot satisfy byte range for empty file".to_string());
    }

    if start_str.is_empty() {
        // Suffix byte range: bytes=-500 -> last 500 bytes
        let suffix: u64 = end_str
            .parse()
            .map_err(|_| format!("Malformed Range header: {range_header}"))?;
        if suffix == 0 {
            return Err("Invalid suffix byte range".to_string());
        }
        return Ok(Some((file_size.saturating_sub(suffix), file_size - 1)));
    }

    let start: u64 = start_str
        .parse()
        .map_err(|_| "Range start past end of file".to_string())?;
    if start >= file_size {
        return Err("Range start past end of file".to_string());
    }
    let end = if end_str.is_empty() {
        file_size - 1
    } else {
        let parsed_end = end_str.parse::<u64>().unwrap_or(u64::MAX);
        parsed_end.min(file_size - 1)
    };
    if end < start {
        return Err("Range end before start".to_string());
    }
    Ok(Some((start, end)))
}

/// Serve a local video file with full HTTP 206 Partial Content support.
pub async fn stream_video_file(
    file_path: &Path,
    range_header: Option<&str>,
    head_only: bool,
) -> Response {
    let metadata = match tokio::fs::metadata(file_path).await {
        Ok(meta) if meta.is_file() => meta,
        _ => {
            return (StatusCode::NOT_FOUND, "Video file not found on disk").into_response();
        }
    };
    let file_size = metadata.len();

    let byte_range = match range_header {
        Some(hdr) if !hdr.is_empty() => match parse_range_header(hdr, file_size) {
            Ok(range) => range,
            Err(_) => {
                let mut resp = Response::new(Body::empty());
                *resp.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
                let headers = resp.headers_mut();
                if let Ok(val) = HeaderValue::from_str(&format!("bytes */{file_size}")) {
                    headers.insert(header::CONTENT_RANGE, val);
                }
                headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
                return resp;
            }
        },
        _ => None,
    };

    let (status, start, content_length, content_range) = match byte_range {
        Some((start, end)) => (
            StatusCode::PARTIAL_CONTENT,
            start,
            end - start + 1,
            Some(format!("bytes {start}-{end}/{file_size}")),
        ),
        None => (StatusCode::OK, 0, file_size, None),
    };

    let body = if head_only || content_length == 0 {
        Body::empty()
    } else {
        let mut file = match tokio::fs::File::open(file_path).await {
            Ok(f) => f,
            Err(_) => {
                return (StatusCode::NOT_FOUND, "Video file not found on disk").into_response();
            }
        };
        if start > 0 && file.seek(SeekFrom::Start(start)).await.is_err() {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to seek video file",
            )
                .into_response();
        }
        let limited = file.take(content_length);
        let stream = ReaderStream::with_capacity(limited, CHUNK_SIZE);
        Body::from_stream(stream)
    };

    let mut resp = Response::new(body);
    *resp.status_mut() = status;
    let headers = resp.headers_mut();
    if let Some(cr) = content_range {
        if let Ok(val) = HeaderValue::from_str(&cr) {
            headers.insert(header::CONTENT_RANGE, val);
        }
    }
    let mime = guess_video_mime(file_path);
    if let Ok(val) = HeaderValue::from_str(&mime) {
        headers.insert(header::CONTENT_TYPE, val);
    }
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(content_length));
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

/// Metadata for an audio stream probed from a local media container via `ffprobe`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AudioTrack {
    pub index: usize,
    pub codec: String,
    pub channels: u32,
    pub language: String,
    pub title: String,
    pub browser_compatible: bool,
}

/// Return `true` if the given `ffprobe` audio codec name is natively supported by
/// standard HTML5 `<video>` browsers without server-side transcoding.
pub fn is_browser_compatible_audio_codec(codec: &str) -> bool {
    const COMPATIBLE: &[&str] = &[
        "aac",
        "mp3",
        "opus",
        "vorbis",
        "flac",
        "mp2",
        "pcm_s16le",
        "pcm_s24le",
        "pcm_f32le",
        "pcm_u8",
        "alac",
    ];
    let trimmed = codec.trim();
    COMPATIBLE.iter().any(|c| trimmed.eq_ignore_ascii_case(c))
}

#[derive(Deserialize)]
struct FfprobeOutput {
    #[serde(default)]
    streams: Vec<FfprobeStream>,
}

#[derive(Deserialize)]
struct FfprobeStream {
    #[serde(default)]
    codec_name: String,
    channels: Option<u32>,
    tags: Option<serde_json::Map<String, serde_json::Value>>,
}

fn extract_tag_str(
    tags: Option<&serde_json::Map<String, serde_json::Value>>,
    key: &str,
) -> Option<String> {
    let map = tags?;
    if let Some(val) = map.get(key).and_then(|v| v.as_str()) {
        let trimmed = val.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    for (k, v) in map {
        if k.eq_ignore_ascii_case(key) {
            if let Some(val) = v.as_str() {
                let trimmed = val.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
    }
    None
}

/// Parse JSON output from
/// `ffprobe -v quiet -print_format json -show_streams -select_streams a <path>`
/// into ordered [`AudioTrack`] metadata entries.
pub fn parse_ffprobe_audio_streams(json_str: &str) -> Vec<AudioTrack> {
    let Ok(parsed) = serde_json::from_str::<FfprobeOutput>(json_str) else {
        return Vec::new();
    };

    parsed
        .streams
        .into_iter()
        .enumerate()
        .map(|(idx, stream)| {
            let codec = stream.codec_name.trim().to_ascii_lowercase();
            let channels = stream.channels.filter(|&c| c > 0).unwrap_or(2);
            let language = extract_tag_str(stream.tags.as_ref(), "language")
                .unwrap_or_else(|| "und".to_string());
            let title = extract_tag_str(stream.tags.as_ref(), "title").unwrap_or_default();
            let browser_compatible = is_browser_compatible_audio_codec(&codec);
            AudioTrack {
                index: idx,
                codec,
                channels,
                language,
                title,
                browser_compatible,
            }
        })
        .collect()
}

/// Probe audio streams for a local video file using `ffprobe` (5 s timeout).
///
/// Returns an empty list if `ffprobe` is unavailable, times out, or fails.
pub fn probe_audio_tracks(file_path: &Path) -> Vec<AudioTrack> {
    let mut cmd = std::process::Command::new("ffprobe");
    cmd.args([
        "-v",
        "quiet",
        "-print_format",
        "json",
        "-show_streams",
        "-select_streams",
        "a",
    ])
    .arg(file_path);

    let outcome = crate::util::run_command_with_timeout(&mut cmd, Duration::from_secs(5), false);
    if !outcome.success() {
        return Vec::new();
    }
    let stdout = String::from_utf8_lossy(&outcome.stdout);
    parse_ffprobe_audio_streams(&stdout)
}

/// Wraps a spawned `ffmpeg` child and its stdout pipe so dropping the HTTP response
/// stream immediately drops (and kills via `kill_on_drop(true)`) the child process.
struct ChildStdoutReader {
    _child: tokio::process::Child,
    stdout: tokio::process::ChildStdout,
}

impl AsyncRead for ChildStdoutReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stdout).poll_read(cx, buf)
    }
}

/// Stream a local video file remuxed on the fly into fragmented MP4 with AAC stereo
/// audio via `ffmpeg`, allowing browsers to play containers with AC3/DTS/TrueHD audio.
pub async fn stream_remuxed_video(
    file_path: &Path,
    start_sec: f64,
    audio_index: Option<usize>,
) -> Response {
    match tokio::fs::metadata(file_path).await {
        Ok(meta) if meta.is_file() => {}
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Video file not found on disk" })),
            )
                .into_response();
        }
    }

    let mut cmd = tokio::process::Command::new("ffmpeg");
    cmd.kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .args(["-hide_banner", "-loglevel", "error"]);

    if start_sec.is_finite() && start_sec > 0.5 {
        cmd.args(["-ss", &format!("{start_sec:.3}")]);
    }

    let audio_map = format!("0:a:{}?", audio_index.unwrap_or(0));
    cmd.arg("-i").arg(file_path).args([
        "-map",
        "0:v:0",
        "-map",
        &audio_map,
        "-c:v",
        "copy",
        "-c:a",
        "aac",
        "-ac",
        "2",
        "-b:a",
        "192k",
        "-movflags",
        "frag_keyframe+empty_moov+default_base_moof",
        "-f",
        "mp4",
        "pipe:1",
    ]);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return (
                StatusCode::NOT_IMPLEMENTED,
                Json(json!({ "error": "ffmpeg is not installed on this system" })),
            )
                .into_response();
        }
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to launch ffmpeg: {err}") })),
            )
                .into_response();
        }
    };

    let Some(stdout) = child.stdout.take() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "Failed to capture ffmpeg output pipe" })),
        )
            .into_response();
    };

    let reader = ChildStdoutReader {
        _child: child,
        stdout,
    };
    let stream = ReaderStream::with_capacity(reader, REMUX_CHUNK_SIZE);
    let mut resp = Response::new(Body::from_stream(stream));
    *resp.status_mut() = StatusCode::OK;
    let headers = resp.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("video/mp4"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

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

/// Supported local artwork extensions in deterministic tie-break priority order.
const ARTWORK_IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "webp"];

fn artwork_ext_rank(path: &Path) -> Option<usize> {
    let ext = path.extension()?.to_str()?;
    ARTWORK_IMAGE_EXTS
        .iter()
        .position(|candidate| ext.eq_ignore_ascii_case(candidate))
}

/// Search `folder_path` for a local show poster image matching priority stems
/// `poster`, `folder`, `cover`, `show`, `fanart`, `backdrop` (case-insensitively).
pub fn find_show_poster(folder_path: &Path) -> Option<PathBuf> {
    const POSTER_STEMS: &[&str] = &["poster", "folder", "cover", "show", "fanart", "backdrop"];

    let entries = std::fs::read_dir(folder_path).ok()?;
    let mut best: Option<(usize, usize, String, PathBuf)> = None;

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(ext_rank) = artwork_ext_rank(&path) else {
            continue;
        };
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(stem_rank) = POSTER_STEMS
            .iter()
            .position(|candidate| stem.eq_ignore_ascii_case(candidate))
        else {
            continue;
        };
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

    let entries = std::fs::read_dir(parent).ok()?;
    let mut best: Option<(usize, usize, String, PathBuf)> = None;

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(ext_rank) = artwork_ext_rank(&path) else {
            continue;
        };
        let Some(entry_stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let entry_stem_lower = entry_stem.to_lowercase();
        let kind_rank = if entry_stem_lower == stem_lower {
            0
        } else if entry_stem_lower == thumb_stem_lower {
            1
        } else {
            continue;
        };
        let filename_lower = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_lowercase();

        let candidate = (kind_rank, ext_rank, filename_lower, path);
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

fn try_extract_thumbnail(video_path: &Path, tmp_path: &Path, seek_ts: &str) -> ExtractResult {
    let mut cmd = std::process::Command::new("ffmpeg");
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

    let outcome = crate::util::run_command_with_timeout(&mut cmd, Duration::from_secs(8), false);
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
    std::fs::create_dir_all(cache_dir).ok()?;

    let tmp_path = cache_dir.join(format!(".{safe_id}.{}.tmp.jpg", uuid::Uuid::new_v4()));
    let extracted = match try_extract_thumbnail(video_path, &tmp_path, "00:01:30") {
        ExtractResult::Ok => true,
        ExtractResult::FfmpegMissing => false,
        ExtractResult::Failed => {
            let _ = std::fs::remove_file(&tmp_path);
            matches!(
                try_extract_thumbnail(video_path, &tmp_path, "00:00:03"),
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
