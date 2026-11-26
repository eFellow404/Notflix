//! Episode playback progress, manual intro override, and byte-range streaming endpoints.

use std::path::Path;

use axum::body::Bytes;
use axum::extract::{Path as AxumPath, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Map, Value};

use crate::error::{NotflixError, Result};
use crate::linked_list::EpisodeNode;
use crate::media::stream_video_file;
use crate::storage::NotflixStore;

use super::{json_no_store, parse_json_object, AppState};

fn optional_f64_field(map: &Map<String, Value>, key: &str, err_msg: &str) -> Result<Option<f64>> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .map(Some)
            .ok_or_else(|| NotflixError::BadRequest(err_msg.to_string())),
    }
}

fn episode_or_404(store: &NotflixStore, node: Option<EpisodeNode>) -> Result<Response> {
    let Some(node) = node else {
        return Err(NotflixError::NotFound("Episode not found".to_string()));
    };
    let ep_json = store.episode_to_json_with_links(&node)?;
    Ok(json_no_store(StatusCode::OK, json!({ "episode": ep_json })))
}

pub(super) async fn handle_progress_update(
    State(state): State<AppState>,
    AxumPath(episode_id): AxumPath<String>,
    body: Bytes,
) -> Result<Response> {
    const NUM_ERR: &str = "Invalid numeric playback values";
    let map = parse_json_object(&body)?;

    let position_sec = optional_f64_field(&map, "position_sec", NUM_ERR)?;
    let watched = match map.get("watched") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_bool().unwrap_or(false)),
    };

    if position_sec.is_none() && watched.is_none() {
        return Err(NotflixError::BadRequest(
            "position_sec or watched is required".to_string(),
        ));
    }

    let duration_sec = optional_f64_field(&map, "duration_sec", NUM_ERR)?;
    let set_current = match map.get("set_current") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_bool().unwrap_or(false)),
    };
    let reset_position = map
        .get("reset_position")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let updated = state.store.update_progress(
        &episode_id,
        position_sec,
        duration_sec,
        watched,
        set_current,
        reset_position,
    )?;
    episode_or_404(&state.store, updated)
}

pub(super) async fn handle_intro_update(
    State(state): State<AppState>,
    AxumPath(episode_id): AxumPath<String>,
    body: Bytes,
) -> Result<Response> {
    const INTRO_ERR: &str = "intro_start_sec and intro_end_sec must be numbers";
    let map = parse_json_object(&body)?;
    let clear = map.get("clear").and_then(Value::as_bool).unwrap_or(false);

    let updated = if clear {
        state
            .store
            .set_episode_intro(&episode_id, None, None, 1.0, "manual")?
    } else {
        let start_sec = optional_f64_field(&map, "intro_start_sec", INTRO_ERR)?;
        let end_sec = optional_f64_field(&map, "intro_end_sec", INTRO_ERR)?;
        match (start_sec, end_sec) {
            (None, None) => {
                state
                    .store
                    .set_episode_intro(&episode_id, None, None, 1.0, "manual")?
            }
            (Some(start), Some(end)) => {
                state
                    .store
                    .set_episode_intro(&episode_id, Some(start), Some(end), 1.0, "manual")?
            }
            _ => return Err(NotflixError::BadRequest(INTRO_ERR.to_string())),
        }
    };
    episode_or_404(&state.store, updated)
}

pub(super) async fn handle_stream_get(
    State(state): State<AppState>,
    AxumPath(episode_id): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    handle_stream_impl(&state, &episode_id, &headers, false).await
}

pub(super) async fn handle_stream_head(
    State(state): State<AppState>,
    AxumPath(episode_id): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    handle_stream_impl(&state, &episode_id, &headers, true).await
}

async fn handle_stream_impl(
    state: &AppState,
    episode_id: &str,
    headers: &HeaderMap,
    head_only: bool,
) -> Response {
    let ep = match state.store.get_episode(episode_id) {
        Ok(Some(ep)) => ep,
        Ok(None) => {
            if head_only {
                return StatusCode::NOT_FOUND.into_response();
            }
            return NotflixError::NotFound("Episode not found".to_string()).into_response();
        }
        Err(e) => return e.into_response(),
    };
    let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    stream_video_file(Path::new(&ep.file_path), range, head_only).await
}
