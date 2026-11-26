//! Media MIME helpers and HTTP 206 byte-range video streaming.

use std::io::SeekFrom;
use std::path::Path;

use axum::body::Body;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

/// 256 KiB chunks for smooth seeking and low memory footprint.
pub const CHUNK_SIZE: usize = 256 * 1024;

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
