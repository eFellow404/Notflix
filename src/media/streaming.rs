//! Video MIME detection, HTTP 206 byte-range streaming, and on-the-fly ffmpeg remuxing.

use std::io::SeekFrom;
use std::path::Path;
use std::pin::Pin;
use std::process::Stdio;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, ReadBuf};
use tokio_util::io::ReaderStream;

use super::CHUNK_SIZE;
use crate::ffmpeg_bin::resolve_ffmpeg;

/// 64 KiB chunks for live ffmpeg remux pipes.
const REMUX_CHUNK_SIZE: usize = 64 * 1024;

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

fn build_remux_command(
    ffmpeg_bin: &Path,
    file_path: &Path,
    start_sec: f64,
    audio_index: Option<usize>,
) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(ffmpeg_bin);
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
    cmd
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

    let Some(ffmpeg_bin) = resolve_ffmpeg() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "ffmpeg is not installed on this system" })),
        )
            .into_response();
    };

    let mut cmd = build_remux_command(&ffmpeg_bin, file_path, start_sec, audio_index);
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
