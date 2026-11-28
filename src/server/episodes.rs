//! Episode playback progress, manual intro override, and byte-range streaming endpoints.

use std::path::Path;

use axum::body::Bytes;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use crate::error::{NotflixError, Result};
use crate::linked_list::EpisodeNode;
use crate::media::{
    find_or_generate_episode_thumbnail, guess_image_mime, probe_audio_tracks, stream_remuxed_video,
    stream_video_file,
};
use crate::storage::NotflixStore;
use crate::subtitles::{discover_subtitles, load_subtitle_as_vtt};

use super::{
    binary_response, json_no_store, optional_bool_or, optional_f64_field, optional_tristate_bool,
    parse_json_object, read_file_or_not_found, require_episode, run_blocking, AppState,
};

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
    let watched = optional_tristate_bool(&map, "watched");

    if position_sec.is_none() && watched.is_none() {
        return Err(NotflixError::BadRequest(
            "position_sec or watched is required".to_string(),
        ));
    }

    let duration_sec = optional_f64_field(&map, "duration_sec", NUM_ERR)?;
    let set_current = optional_tristate_bool(&map, "set_current");
    let reset_position = optional_bool_or(&map, "reset_position", false);

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
    let clear = optional_bool_or(&map, "clear", false);

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

#[derive(Deserialize, Default)]
pub(super) struct StreamQuery {
    remux: Option<u8>,
    start: Option<f64>,
    audio: Option<usize>,
}

pub(super) async fn handle_episode_tracks(
    State(state): State<AppState>,
    AxumPath(episode_id): AxumPath<String>,
) -> Result<Response> {
    let store = state.store.clone();
    let payload = run_blocking(move || -> Result<_> {
        let ep = require_episode(&store, &episode_id)?;
        let ep_path = Path::new(&ep.file_path);
        let subtitles = discover_subtitles(ep_path);
        let audio = probe_audio_tracks(ep_path);
        let needs_remux_hint = !audio.is_empty() && audio.iter().all(|t| !t.browser_compatible);
        Ok(json!({
            "subtitles": subtitles,
            "audio": audio,
            "needs_remux_hint": needs_remux_hint,
        }))
    })
    .await??;

    Ok(json_no_store(StatusCode::OK, payload))
}

pub(super) async fn handle_episode_subtitle(
    State(state): State<AppState>,
    AxumPath((episode_id, sub_index)): AxumPath<(String, usize)>,
) -> Result<Response> {
    let store = state.store.clone();
    let vtt = run_blocking(move || {
        let ep = require_episode(&store, &episode_id)?;
        let tracks = discover_subtitles(Path::new(&ep.file_path));
        let Some(track) = tracks.get(sub_index) else {
            return Err(NotflixError::NotFound(
                "Subtitle track not found".to_string(),
            ));
        };
        load_subtitle_as_vtt(track)
    })
    .await??;

    Ok(binary_response(
        StatusCode::OK,
        "text/vtt; charset=utf-8",
        "no-store",
        vtt.into_bytes(),
    ))
}

pub(super) async fn handle_episode_thumbnail(
    State(state): State<AppState>,
    AxumPath(episode_id): AxumPath<String>,
) -> Result<Response> {
    let store = state.store.clone();
    let (bytes, mime) = run_blocking(move || {
        let ep = require_episode(&store, &episode_id)?;
        let cache_dir = store.db_dir().join("thumbs");
        let Some(thumb_path) =
            find_or_generate_episode_thumbnail(Path::new(&ep.file_path), &cache_dir, &episode_id)
        else {
            return Err(NotflixError::NotFound("Thumbnail not found".to_string()));
        };
        let data = read_file_or_not_found(&thumb_path, "Thumbnail not found")?;
        let mime = guess_image_mime(&thumb_path);
        Ok((data, mime))
    })
    .await??;

    Ok(binary_response(
        StatusCode::OK,
        mime,
        "public, max-age=86400",
        bytes,
    ))
}

pub(super) async fn handle_stream_get(
    State(state): State<AppState>,
    AxumPath(episode_id): AxumPath<String>,
    Query(query): Query<StreamQuery>,
    headers: HeaderMap,
) -> Response {
    if query.remux == Some(1) {
        let ep = match require_episode(&state.store, &episode_id) {
            Ok(ep) => ep,
            Err(e) => return e.into_response(),
        };
        let start_sec = query
            .start
            .filter(|v| v.is_finite() && *v >= 0.0)
            .unwrap_or(0.0);
        return stream_remuxed_video(Path::new(&ep.file_path), start_sec, query.audio).await;
    }
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
    let ep = match require_episode(&state.store, episode_id) {
        Ok(ep) => ep,
        Err(NotflixError::NotFound(_)) if head_only => {
            return StatusCode::NOT_FOUND.into_response();
        }
        Err(e) => return e.into_response(),
    };
    let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    stream_video_file(Path::new(&ep.file_path), range, head_only).await
}
