//! Health check and Git auto-updater endpoints.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{json, Map, Value};

use crate::error::Result;
use crate::media::detect_ffmpeg;
use crate::updater::{
    apply_update_with_fallback, check_for_updates_with_fallback, get_update_mode_info,
    UpdateResult, DEFAULT_FETCH_TIMEOUT_SEC,
};
use crate::{BUILD_SHA, VERSION};

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
    let allow_fallback = state.allow_release_fallback();
    let (commit, ffmpeg_available) = run_blocking(move || {
        let info = get_update_mode_info(&repo_dir, allow_fallback);
        (info.local_short_sha, detect_ffmpeg())
    })
    .await
    .unwrap_or((None, false));

    json_no_store(
        StatusCode::OK,
        json!({
            "status": "ok",
            "version": VERSION,
            "build_sha": BUILD_SHA,
            "commit": commit,
            "ffmpeg_available": ffmpeg_available,
        }),
    )
}

pub(super) async fn handle_update_status(State(state): State<AppState>) -> Response {
    let cached = state.last_update_result.lock().ok().and_then(|g| g.clone());
    let payload = match cached {
        Some(r) => serde_json::to_value(r).unwrap_or_default(),
        None => {
            let repo_dir = state.current_repo_dir();
            let allow_fallback = state.allow_release_fallback();
            run_blocking(move || {
                serde_json::to_value(get_update_mode_info(&repo_dir, allow_fallback))
                    .unwrap_or_default()
            })
            .await
            .unwrap_or_default()
        }
    };
    json_no_store(StatusCode::OK, payload)
}

pub(super) async fn handle_update_check(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Response> {
    let map = parse_json_object(&body)?;
    let timeout_sec = extract_timeout(&map);
    let _guard = state.update_lock.lock().await;
    let repo_dir = state.current_repo_dir();
    let allow_fallback = state.allow_release_fallback();
    let res = run_blocking(move || {
        check_for_updates_with_fallback(&repo_dir, allow_fallback, timeout_sec)
    })
    .await?;
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
    let allow_fallback = state.allow_release_fallback();
    let db_path = state.store.db_path().to_path_buf();
    let res = run_blocking(move || {
        apply_update_with_fallback(&repo_dir, allow_fallback, Some(&db_path), timeout_sec)
    })
    .await?;
    Ok(cache_and_respond(&state, res))
}
