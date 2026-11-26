//! Show management, episode navigation, intro detection, and filesystem browsing endpoints.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use axum::body::Bytes;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{NotflixError, Result};
use crate::intelligence::is_video_extension;
use crate::intro_detector::{
    build_fingerprint, detect_show_intros_server, find_shared_intros_default, EpisodeFingerprint,
    IntroSpan,
};
use crate::linked_list::EpisodeLinkedList;
use crate::storage::NotflixStore;
use crate::util::{canonicalize_clean, expand_tilde_str};

use super::{json_no_store, parse_json_object, run_blocking, value_as_trimmed_string, AppState};

// ---------------------------------------------------------------------------
// Filesystem Browser Endpoint
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
pub(super) struct BrowseQuery {
    path: Option<String>,
}

pub(super) async fn handle_fs_browse(Query(query): Query<BrowseQuery>) -> Result<Response> {
    let raw_path = query.path.as_deref().unwrap_or_default().trim();
    let target = if raw_path.is_empty() {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
    } else {
        expand_tilde_str(raw_path)
    };

    run_blocking(move || {
        let resolved = canonicalize_clean(&target).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                NotflixError::NotFound(format!("Directory not found: {}", target.display()))
            } else {
                NotflixError::BadRequest(format!("Cannot resolve path: {e}"))
            }
        })?;

        if !resolved.is_dir() {
            return Err(NotflixError::NotFound(format!(
                "Directory not found: {}",
                resolved.display()
            )));
        }

        let read_dir = std::fs::read_dir(&resolved).map_err(|e| {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                NotflixError::Forbidden("Permission denied reading directory".to_string())
            } else {
                NotflixError::Io(e)
            }
        })?;

        let mut dirs_out: Vec<(String, String, PathBuf)> = Vec::new();
        let mut direct_video_count = 0usize;

        for entry in read_dir.flatten() {
            let path = entry.path();
            let Some(name_os) = path.file_name() else {
                continue;
            };
            let name = name_os.to_string_lossy();
            if name.starts_with('.') {
                continue;
            }
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                let name_owned = name.into_owned();
                let sort_key = name_owned.to_lowercase();
                dirs_out.push((sort_key, name_owned, path));
            } else if meta.is_file()
                && path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(is_video_extension)
            {
                direct_video_count += 1;
            }
        }

        dirs_out.sort_by(|a, b| a.0.cmp(&b.0));
        let directories: Vec<Value> = dirs_out
            .into_iter()
            .map(|(_, name, path)| {
                json!({
                    "name": name,
                    "path": path.to_string_lossy().as_ref(),
                })
            })
            .collect();

        let parent = resolved
            .parent()
            .filter(|p| *p != resolved.as_path())
            .map(|p| p.to_string_lossy().into_owned());

        Ok(json_no_store(
            StatusCode::OK,
            json!({
                "path": resolved.to_string_lossy().as_ref(),
                "parent": parent,
                "directories": directories,
                "direct_video_count": direct_video_count,
            }),
        ))
    })
    .await?
}

// ---------------------------------------------------------------------------
// Shows Endpoints
// ---------------------------------------------------------------------------

pub(super) async fn handle_list_shows(State(state): State<AppState>) -> Result<Response> {
    let store = state.store.clone();
    let shows = run_blocking(move || store.list_shows()).await??;
    Ok(json_no_store(StatusCode::OK, json!({ "shows": shows })))
}

pub(super) async fn handle_create_show(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Response> {
    let map = parse_json_object(&body)?;
    let folder_path = value_as_trimmed_string(map.get("folder_path"));
    if folder_path.is_empty() {
        return Err(NotflixError::BadRequest(
            "folder_path is required".to_string(),
        ));
    }
    let name = map
        .get("name")
        .and_then(Value::as_str)
        .map(ToString::to_string);

    let store = state.store.clone();
    let (show, ll) = run_blocking(move || store.add_show(&folder_path, name.as_deref())).await??;

    Ok(json_no_store(
        StatusCode::CREATED,
        json!({ "show": show, "linked_list": ll.to_json() }),
    ))
}

#[derive(Deserialize, Default)]
pub(super) struct ShowDetailQuery {
    autosync: Option<String>,
}

pub(super) async fn handle_get_show(
    State(state): State<AppState>,
    AxumPath(show_id): AxumPath<String>,
    Query(query): Query<ShowDetailQuery>,
) -> Result<Response> {
    let autosync = matches!(query.autosync.as_deref(), Some("1" | "true" | "yes"));
    let store = state.store.clone();

    run_blocking(move || {
        if autosync {
            match store.rescan_show(&show_id, false) {
                Ok(Some((show, ll, diff))) => {
                    return Ok(json_no_store(
                        StatusCode::OK,
                        json!({
                            "show": show,
                            "linked_list": ll.to_json(),
                            "diff": diff,
                        }),
                    ));
                }
                Ok(None) | Err(NotflixError::NotFound(_)) => {}
                Err(e) => return Err(e),
            }
        }

        let Some((show, ll)) = store.get_show(&show_id)? else {
            return Err(NotflixError::NotFound("Show not found".to_string()));
        };
        Ok(json_no_store(
            StatusCode::OK,
            json!({ "show": show, "linked_list": ll.to_json() }),
        ))
    })
    .await?
}

pub(super) async fn handle_rename_show(
    State(state): State<AppState>,
    AxumPath(show_id): AxumPath<String>,
    body: Bytes,
) -> Result<Response> {
    let map = parse_json_object(&body)?;
    let name = value_as_trimmed_string(map.get("name"));
    if name.is_empty() {
        return Err(NotflixError::BadRequest("name is required".to_string()));
    }
    let Some(updated_show) = state.store.rename_show(&show_id, &name)? else {
        return Err(NotflixError::NotFound("Show not found".to_string()));
    };
    Ok(json_no_store(
        StatusCode::OK,
        json!({ "show": updated_show }),
    ))
}

pub(super) async fn handle_delete_show(
    State(state): State<AppState>,
    AxumPath(show_id): AxumPath<String>,
) -> Result<Response> {
    if !state.store.delete_show(&show_id)? {
        return Err(NotflixError::NotFound("Show not found".to_string()));
    }
    Ok(json_no_store(StatusCode::OK, json!({ "deleted": true })))
}

pub(super) async fn handle_rescan_show(
    State(state): State<AppState>,
    AxumPath(show_id): AxumPath<String>,
    body: Bytes,
) -> Result<Response> {
    let map = parse_json_object(&body)?;
    let reset_order = map
        .get("reset_order")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let store = state.store.clone();
    let res = run_blocking(move || store.rescan_show(&show_id, reset_order)).await??;
    let Some((show, ll, diff)) = res else {
        return Err(NotflixError::NotFound("Show not found".to_string()));
    };
    Ok(json_no_store(
        StatusCode::OK,
        json!({ "show": show, "linked_list": ll.to_json(), "diff": diff }),
    ))
}

pub(super) async fn handle_next_episode(
    State(state): State<AppState>,
    AxumPath(show_id): AxumPath<String>,
    body: Bytes,
) -> Result<Response> {
    let _ = parse_json_object(&body)?;
    handle_step(&state.store, &show_id, true)
}

pub(super) async fn handle_prev_episode(
    State(state): State<AppState>,
    AxumPath(show_id): AxumPath<String>,
    body: Bytes,
) -> Result<Response> {
    let _ = parse_json_object(&body)?;
    handle_step(&state.store, &show_id, false)
}

fn handle_step(store: &NotflixStore, show_id: &str, forward: bool) -> Result<Response> {
    let boundary_key = if forward { "at_end" } else { "at_start" };
    let res = if forward {
        store.advance_next(show_id)?
    } else {
        store.step_prev(show_id)?
    };

    let Some((node, ll)) = res else {
        let Some((_, ll)) = store.get_show(show_id)? else {
            return Err(NotflixError::NotFound("Show not found".to_string()));
        };
        return Ok(json_no_store(
            StatusCode::OK,
            json!({
                "episode": Value::Null,
                boundary_key: true,
                "linked_list": ll.to_json(),
            }),
        ));
    };

    let at_boundary = if forward {
        ll.next_id_of(&node.id).is_none()
    } else {
        ll.prev_id_of(&node.id).is_none()
    };
    let ep_json = ll.node_to_json(&node.id, ll.index_of(&node.id));

    Ok(json_no_store(
        StatusCode::OK,
        json!({
            "episode": ep_json,
            boundary_key: at_boundary,
            "linked_list": ll.to_json(),
        }),
    ))
}

pub(super) async fn handle_select_episode(
    State(state): State<AppState>,
    AxumPath(show_id): AxumPath<String>,
    body: Bytes,
) -> Result<Response> {
    let map = parse_json_object(&body)?;
    let episode_id = value_as_trimmed_string(map.get("episode_id"));
    if episode_id.is_empty() {
        return Err(NotflixError::BadRequest(
            "episode_id is required".to_string(),
        ));
    }
    let Some(node) = state.store.select_episode(&show_id, &episode_id)? else {
        return Err(NotflixError::NotFound(
            "Show or episode not found".to_string(),
        ));
    };
    let (_, ll) = state
        .store
        .get_show(&show_id)?
        .ok_or_else(|| NotflixError::NotFound("Show not found".to_string()))?;
    let ep_json = ll.node_to_json(&node.id, ll.index_of(&node.id));

    Ok(json_no_store(
        StatusCode::OK,
        json!({
            "episode": ep_json,
            "linked_list": ll.to_json(),
        }),
    ))
}

pub(super) async fn handle_reorder_episode(
    State(state): State<AppState>,
    AxumPath(show_id): AxumPath<String>,
    body: Bytes,
) -> Result<Response> {
    let map = parse_json_object(&body)?;
    let episode_id = value_as_trimmed_string(map.get("episode_id"));
    let target_idx = map.get("target_index").and_then(Value::as_i64);
    let (false, Some(target_idx)) = (episode_id.is_empty(), target_idx) else {
        return Err(NotflixError::BadRequest(
            "episode_id and integer target_index are required".to_string(),
        ));
    };
    let Some(ll) = state
        .store
        .reorder_episode(&show_id, &episode_id, target_idx as isize)?
    else {
        return Err(NotflixError::NotFound(
            "Show or episode not found".to_string(),
        ));
    };
    Ok(json_no_store(
        StatusCode::OK,
        json!({ "linked_list": ll.to_json() }),
    ))
}

pub(super) async fn handle_bulk_watched(
    State(state): State<AppState>,
    AxumPath(show_id): AxumPath<String>,
    body: Bytes,
) -> Result<Response> {
    let map = parse_json_object(&body)?;
    let Some(raw_ids) = map
        .get("episode_ids")
        .and_then(Value::as_array)
        .filter(|arr| !arr.is_empty())
    else {
        return Err(NotflixError::BadRequest(
            "episode_ids must be a non-empty list".to_string(),
        ));
    };
    let episode_ids: Vec<String> = raw_ids
        .iter()
        .map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .collect();
    let watched = map.get("watched").and_then(Value::as_bool).unwrap_or(true);
    let reset_position = map
        .get("reset_position")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let Some(ll) =
        state
            .store
            .set_episodes_watched(&show_id, &episode_ids, watched, reset_position)?
    else {
        return Err(NotflixError::NotFound("Show not found".to_string()));
    };
    Ok(json_no_store(
        StatusCode::OK,
        json!({ "linked_list": ll.to_json() }),
    ))
}

// ---------------------------------------------------------------------------
// Intro Detection Endpoint
// ---------------------------------------------------------------------------

struct IntroDetectionPass {
    detected: HashMap<String, IntroSpan>,
    method: String,
    needs_visual: bool,
    mark_scanned: bool,
}

fn optional_f64_or(val: Option<&Value>, default: f64) -> Option<f64> {
    match val {
        None | Some(Value::Null) => Some(default),
        Some(v) => v.as_f64(),
    }
}

fn parse_client_fingerprints(
    raw_fps: &[Value],
    valid_ids: &HashSet<&str>,
) -> Vec<EpisodeFingerprint> {
    let mut parsed = Vec::with_capacity(raw_fps.len());
    for item in raw_fps {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let ep_id = value_as_trimmed_string(obj.get("episode_id"));
        if !valid_ids.contains(ep_id.as_str()) {
            continue;
        }
        let Some(raw_vecs) = obj
            .get("vectors")
            .and_then(Value::as_array)
            .filter(|v| !v.is_empty())
        else {
            continue;
        };
        let Some(step_sec) = optional_f64_or(obj.get("step_sec"), 1.5) else {
            continue;
        };
        let Some(duration_sec) = optional_f64_or(obj.get("duration_sec"), 0.0) else {
            continue;
        };

        let vectors: Vec<Vec<f64>> = raw_vecs
            .iter()
            .map(|row| {
                row.as_array()
                    .map(|arr| arr.iter().filter_map(Value::as_f64).collect())
                    .unwrap_or_default()
            })
            .collect();

        let flat_flags: Option<Vec<bool>> = obj
            .get("flat_flags")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().map(|v| v.as_bool().unwrap_or(false)).collect());

        parsed.push(build_fingerprint(
            ep_id,
            step_sec,
            &vectors,
            flat_flags.as_deref(),
            duration_sec,
            "visual",
            8.0,
        ));
    }
    parsed
}

fn run_server_intro_pass(ll: &EpisodeLinkedList) -> IntroDetectionPass {
    let non_extras: Vec<_> = ll.iter().filter(|n| !n.is_extra).collect();
    let targets = if non_extras.len() >= 2 {
        non_extras
    } else {
        ll.iter().collect()
    };
    let ep_specs: Vec<(String, PathBuf, f64)> = targets
        .into_iter()
        .map(|n| (n.id.clone(), PathBuf::from(&n.file_path), n.duration_sec))
        .collect();
    let (detected, method, needs_visual) = detect_show_intros_server(&ep_specs, true);
    IntroDetectionPass {
        detected,
        method,
        needs_visual,
        mark_scanned: !needs_visual,
    }
}

pub(super) async fn handle_detect_intros(
    State(state): State<AppState>,
    AxumPath(show_id): AxumPath<String>,
    body: Bytes,
) -> Result<Response> {
    let map = parse_json_object(&body)?;
    let store = state.store.clone();

    run_blocking(move || {
        let Some((_, ll)) = store.get_show(&show_id)? else {
            return Err(NotflixError::NotFound("Show not found".to_string()));
        };
        let force = map.get("force").and_then(Value::as_bool).unwrap_or(false);

        let pass = match map.get("fingerprints") {
            None | Some(Value::Null) => run_server_intro_pass(&ll),
            Some(raw_fps_val) => {
                let Some(raw_fps) = raw_fps_val.as_array() else {
                    return Err(NotflixError::BadRequest(
                        "fingerprints must be a list".to_string(),
                    ));
                };
                let valid_ids: HashSet<&str> = ll.iter().map(|n| n.id.as_str()).collect();
                let parsed_fps = parse_client_fingerprints(raw_fps, &valid_ids);
                let detected = find_shared_intros_default(&parsed_fps);
                let mark_scanned = map
                    .get("mark_scanned")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                IntroDetectionPass {
                    detected,
                    method: "visual".to_string(),
                    needs_visual: false,
                    mark_scanned,
                }
            }
        };

        let detected_count = pass.detected.len();
        let (updated_show, updated_ll) = store
            .save_show_intros(&show_id, &pass.detected, !force, pass.mark_scanned)?
            .ok_or_else(|| NotflixError::NotFound("Show not found".to_string()))?;

        Ok(json_no_store(
            StatusCode::OK,
            json!({
                "show": updated_show,
                "linked_list": updated_ll.to_json(),
                "detected_count": detected_count,
                "method": pass.method,
                "needs_client_visual_scan": pass.needs_visual,
            }),
        ))
    })
    .await?
}
