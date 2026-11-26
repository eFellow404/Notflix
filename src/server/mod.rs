//! Async Axum HTTP server, REST API, byte-range streaming, and embedded static frontend host.

mod assets;
mod episodes;
mod shows;
mod system;

use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde_json::{Map, Value};
use tokio::sync::Mutex as AsyncMutex;

use crate::error::{NotflixError, Result};
use crate::storage::NotflixStore;
use crate::updater::{default_repo_dir, UpdateResult};
use crate::util::canonicalize_clean;

/// Maximum allowed JSON request payload (12 MiB, covers large visual fingerprint batches).
pub const MAX_JSON_BODY_BYTES: usize = 12 * 1024 * 1024;

/// Shared server state passed to all Axum handlers.
#[derive(Clone)]
pub struct AppState {
    pub store: NotflixStore,
    pub static_dir: Option<PathBuf>,
    pub repo_dir: Arc<StdMutex<PathBuf>>,
    pub update_lock: Arc<AsyncMutex<()>>,
    pub last_update_result: Arc<StdMutex<Option<UpdateResult>>>,
    pub quiet: bool,
}

impl AppState {
    pub fn new(
        store: NotflixStore,
        static_dir: Option<PathBuf>,
        repo_dir: Option<PathBuf>,
        initial_update_result: Option<UpdateResult>,
        quiet: bool,
    ) -> Self {
        let candidate_repo = repo_dir.unwrap_or_else(default_repo_dir);
        let resolved_repo =
            canonicalize_clean(&candidate_repo).unwrap_or_else(|_| default_repo_dir());
        let resolved_static = static_dir.map(|p| canonicalize_clean(&p).unwrap_or(p));
        Self {
            store,
            static_dir: resolved_static,
            repo_dir: Arc::new(StdMutex::new(resolved_repo)),
            update_lock: Arc::new(AsyncMutex::new(())),
            last_update_result: Arc::new(StdMutex::new(initial_update_result)),
            quiet,
        }
    }

    pub fn current_repo_dir(&self) -> PathBuf {
        self.repo_dir
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn set_repo_dir(&self, dir: PathBuf) {
        let mut guard = self
            .repo_dir
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = dir;
    }
}

/// Build the complete Axum router for Notflix.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/api/health", get(system::handle_health))
        .route("/api/update/status", get(system::handle_update_status))
        .route("/api/update/check", post(system::handle_update_check))
        .route("/api/update/apply", post(system::handle_update_apply))
        .route("/api/fs/browse", get(shows::handle_fs_browse))
        .route(
            "/api/shows",
            get(shows::handle_list_shows).post(shows::handle_create_show),
        )
        .route(
            "/api/shows/{id}",
            get(shows::handle_get_show)
                .patch(shows::handle_rename_show)
                .post(shows::handle_rename_show)
                .delete(shows::handle_delete_show),
        )
        .route("/api/shows/{id}/rescan", post(shows::handle_rescan_show))
        .route("/api/shows/{id}/next", post(shows::handle_next_episode))
        .route("/api/shows/{id}/prev", post(shows::handle_prev_episode))
        .route("/api/shows/{id}/select", post(shows::handle_select_episode))
        .route(
            "/api/shows/{id}/reorder",
            post(shows::handle_reorder_episode),
        )
        .route(
            "/api/shows/{id}/detect-intros",
            post(shows::handle_detect_intros),
        )
        .route("/api/shows/{id}/watched", post(shows::handle_bulk_watched))
        .route(
            "/api/episodes/{id}/progress",
            patch(episodes::handle_progress_update).post(episodes::handle_progress_update),
        )
        .route(
            "/api/episodes/{id}/intro",
            patch(episodes::handle_intro_update).post(episodes::handle_intro_update),
        )
        .route(
            "/api/episodes/{id}/stream",
            get(episodes::handle_stream_get).head(episodes::handle_stream_head),
        )
        .route(
            "/",
            get(assets::handle_index_get).head(assets::handle_index_head),
        )
        .route(
            "/static/{*path}",
            get(assets::handle_static_get).head(assets::handle_static_head),
        )
        .fallback(assets::handle_fallback)
        .layer(DefaultBodyLimit::max(MAX_JSON_BODY_BYTES))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            access_log_middleware,
        ))
        .with_state(state)
}

async fn access_log_middleware(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    if state.quiet {
        return next.run(req).await;
    }
    let method = req.method().clone();
    let uri = req.uri().clone();
    let start = Instant::now();
    let response = next.run(req).await;
    let status = response.status().as_u16();
    let ms = start.elapsed().as_millis();
    tracing::info!("{method} {uri} -> {status} ({ms}ms)");
    response
}

pub(super) fn json_no_store(status: StatusCode, payload: Value) -> Response {
    let mut resp = (status, Json(payload)).into_response();
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

pub(super) fn parse_json_object(body: &Bytes) -> Result<Map<String, Value>> {
    if body.len() > MAX_JSON_BODY_BYTES {
        return Err(NotflixError::BadRequest(
            "Invalid JSON body: Request body too large".to_string(),
        ));
    }
    let text = std::str::from_utf8(body)
        .map_err(|e| NotflixError::BadRequest(format!("Invalid JSON body: {e}")))?;
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    let val: Value = serde_json::from_str(text)
        .map_err(|e| NotflixError::BadRequest(format!("Invalid JSON body: {e}")))?;
    match val {
        Value::Object(map) => Ok(map),
        _ => Err(NotflixError::BadRequest(
            "Invalid JSON body: Expected JSON object".to_string(),
        )),
    }
}

pub(super) fn value_as_trimmed_string(val: Option<&Value>) -> String {
    match val {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

pub(super) async fn run_blocking<F, T>(f: F) -> Result<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| NotflixError::Internal(e.to_string()))
}
