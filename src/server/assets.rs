//! Embedded and disk-overridden static asset and SPA fallback serving.

use std::borrow::Cow;
use std::path::{Component, Path};

use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{header, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

use crate::error::NotflixError;
use crate::util::canonicalize_clean;

use super::AppState;

#[derive(RustEmbed)]
#[folder = "static/"]
struct EmbeddedStatic;

enum StaticLookup {
    Found(Cow<'static, [u8]>),
    NotFound,
    Forbidden,
}

fn static_mime_type(rel_path: &str) -> Option<&'static str> {
    let ext = Path::new(rel_path).extension()?.to_str()?;
    if ext.eq_ignore_ascii_case("js") || ext.eq_ignore_ascii_case("mjs") {
        Some("application/javascript; charset=utf-8")
    } else if ext.eq_ignore_ascii_case("css") {
        Some("text/css; charset=utf-8")
    } else if ext.eq_ignore_ascii_case("html") {
        Some("text/html; charset=utf-8")
    } else if ext.eq_ignore_ascii_case("svg") {
        Some("image/svg+xml")
    } else if ext.eq_ignore_ascii_case("json") {
        Some("application/json; charset=utf-8")
    } else {
        None
    }
}

fn sanitize_static_path(rel_path: &str) -> Option<String> {
    if rel_path.contains('\0') || rel_path.contains('\\') {
        return None;
    }
    let trimmed = rel_path.trim_start_matches('/');
    let clean = if trimmed.is_empty() {
        "index.html"
    } else {
        trimmed
    };

    let mut parts = Vec::new();
    for comp in Path::new(clean).components() {
        match comp {
            Component::Normal(seg) => parts.push(seg.to_str()?),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if parts.is_empty() {
        Some("index.html".to_string())
    } else {
        Some(parts.join("/"))
    }
}

fn read_static_bytes(static_dir: Option<&Path>, clean_path: &str) -> StaticLookup {
    if let Some(root) = static_dir {
        let candidate = root.join(clean_path);
        let Ok(canonical) = canonicalize_clean(&candidate) else {
            return StaticLookup::NotFound;
        };
        if !canonical.starts_with(root) {
            return StaticLookup::Forbidden;
        }
        if !canonical.is_file() {
            return StaticLookup::NotFound;
        }
        return match std::fs::read(canonical) {
            Ok(bytes) => StaticLookup::Found(Cow::Owned(bytes)),
            Err(_) => StaticLookup::NotFound,
        };
    }
    match EmbeddedStatic::get(clean_path) {
        Some(file) => StaticLookup::Found(file.data),
        None => StaticLookup::NotFound,
    }
}

fn serve_static(state: &AppState, rel_path: &str, head_only: bool, spa_fallback: bool) -> Response {
    let Some(clean) = sanitize_static_path(rel_path) else {
        return NotflixError::Forbidden("Forbidden path".to_string()).into_response();
    };

    let (resolved_name, content) = match read_static_bytes(state.static_dir.as_deref(), &clean) {
        StaticLookup::Found(bytes) => (clean, bytes),
        StaticLookup::Forbidden => {
            return NotflixError::Forbidden("Forbidden path".to_string()).into_response();
        }
        StaticLookup::NotFound if spa_fallback && !clean.starts_with("api/") => {
            match read_static_bytes(state.static_dir.as_deref(), "index.html") {
                StaticLookup::Found(bytes) => ("index.html".to_string(), bytes),
                StaticLookup::Forbidden => {
                    return NotflixError::Forbidden("Forbidden path".to_string()).into_response();
                }
                StaticLookup::NotFound => {
                    return NotflixError::NotFound("Not found".to_string()).into_response();
                }
            }
        }
        StaticLookup::NotFound => {
            return NotflixError::NotFound("Not found".to_string()).into_response();
        }
    };

    let content_type = match static_mime_type(&resolved_name) {
        Some(mime) => HeaderValue::from_static(mime),
        None => {
            let guessed = mime_guess::from_path(&resolved_name).first_or_octet_stream();
            HeaderValue::from_str(guessed.essence_str())
                .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"))
        }
    };

    let len = content.len();
    let body = if head_only {
        Body::empty()
    } else {
        match content {
            Cow::Borrowed(slice) => Body::from(slice),
            Cow::Owned(vec) => Body::from(vec),
        }
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, HeaderValue::from(len))
        .header(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"))
        .body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

pub(super) async fn handle_index_get(State(state): State<AppState>) -> Response {
    serve_static(&state, "index.html", false, false)
}

pub(super) async fn handle_index_head(State(state): State<AppState>) -> Response {
    serve_static(&state, "index.html", true, false)
}

pub(super) async fn handle_static_get(
    State(state): State<AppState>,
    AxumPath(path): AxumPath<String>,
) -> Response {
    serve_static(&state, &path, false, false)
}

pub(super) async fn handle_static_head(
    State(state): State<AppState>,
    AxumPath(path): AxumPath<String>,
) -> Response {
    serve_static(&state, &path, true, false)
}

pub(super) async fn handle_fallback(State(state): State<AppState>, uri: Uri) -> Response {
    let raw_path = uri.path();
    // Preserve explicit 403 for raw path-traversal attempts under /static/ or elsewhere
    if raw_path.contains("..") || raw_path.contains('\\') || raw_path.contains('\0') {
        return NotflixError::Forbidden("Forbidden path".to_string()).into_response();
    }
    if raw_path.starts_with("/api/") {
        return NotflixError::NotFound("Endpoint not found".to_string()).into_response();
    }
    serve_static(&state, raw_path, false, true)
}
