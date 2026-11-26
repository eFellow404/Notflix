//! Health check and Git auto-updater endpoints.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{json, Map, Value};

use crate::error::Result;
use crate::media::detect_ffmpeg;
use crate::updater::{
    apply_update, check_for_updates, get_local_repo_info, UpdateResult, DEFAULT_FETCH_TIMEOUT_SEC,
};
use crate::VERSION;

use super::{json_no_store, parse_json_object, run_blocking, AppState};

fn extract_timeout(body: &Map<String, Value>) -> f64 {
    let raw = body
        .get("timeout_sec")
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite())
        .unwrap_or(DEFAULT_FETCH_TIMEOUT_SEC);
    raw.clamp(1.0, 30.0)
}

fn cache_and_respond(state: &AppState, res: UpdateResult) -> Response {
    if let Ok(mut last) = state.last_update_result.lock() {
        *last = Some(res.clone());
    }
    json_no_store(
        StatusCode::OK,
        serde_json::to_value(res).unwrap_or_default(),
    )
}

pub(super) async fn handle_health(State(state): State<AppState>) -> Response {
    let repo_dir = state.current_repo_dir();
    let (commit, ffmpeg_available) = run_blocking(move || {
        let info = get_local_repo_info(Some(&repo_dir));
        (info.local_short_sha, detect_ffmpeg())
    })
    .await
    .unwrap_or((None, false));

    json_no_store(
        StatusCode::OK,
        json!({
            "status": "ok",
            "version": VERSION,
            "commit": commit,
            "ffmpeg_available": ffmpeg_available,
        }),
    )
}

pub(super) async fn handle_update_status(State(state): State<AppState>) -> Response {
    let cached = state.last_update_result.lock().ok().and_then(|g| g.clone());
    let res = match cached {
        Some(r) => r,
        None => {
            let repo_dir = state.current_repo_dir();
            run_blocking(move || get_local_repo_info(Some(&repo_dir)))
                .await
                .unwrap_or_else(|_| get_local_repo_info(None))
        }
    };
    json_no_store(
        StatusCode::OK,
        serde_json::to_value(res).unwrap_or_default(),
    )
}

pub(super) async fn handle_update_check(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Response> {
    let map = parse_json_object(&body)?;
    let timeout_sec = extract_timeout(&map);
    let _guard = state.update_lock.lock().await;
    let repo_dir = state.current_repo_dir();
    let res = run_blocking(move || check_for_updates(Some(&repo_dir), timeout_sec)).await?;
    Ok(cache_and_respond(&state, res))
}

pub(super) async fn handle_update_apply(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Response> {
    let map = parse_json_object(&body)?;
    let timeout_sec = extract_timeout(&map);
    let _guard = state.update_lock.lock().await;
    let repo_dir = state.current_repo_dir();
    let db_path = state.store.db_path().to_path_buf();
    let res =
        run_blocking(move || apply_update(Some(&repo_dir), Some(&db_path), timeout_sec)).await?;
    Ok(cache_and_respond(&state, res))
}
