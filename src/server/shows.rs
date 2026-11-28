//! Show management, episode navigation, poster/export/import, and filesystem browsing endpoints.

use std::path::{Path, PathBuf};

use axum::body::Bytes;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::Response;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{NotflixError, Result};
use crate::intelligence::is_video_extension;
use crate::media::{find_show_poster, guess_image_mime};
use crate::storage::{NotflixStore, WatchHistoryExport};
use crate::util::{canonicalize_clean, expand_tilde_str};

use super::{
    binary_response, json_no_store, optional_bool_or, parse_json_object, read_file_or_not_found,
    required_trimmed_string, run_blocking, value_as_trimmed_string, AppState, MAX_JSON_BODY_BYTES,
};

// ---------------------------------------------------------------------------
// Filesystem Browser Endpoint
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
pub(super) struct BrowseQuery {
    path: Option<String>,
}

fn browse_directory(target: &Path) -> Result<Value> {
    let resolved = canonicalize_clean(target).map_err(|e| {
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

    Ok(json!({
        "path": resolved.to_string_lossy().as_ref(),
        "parent": parent,
        "directories": directories,
        "direct_video_count": direct_video_count,
    }))
}

pub(super) async fn handle_fs_browse(Query(query): Query<BrowseQuery>) -> Result<Response> {
    let raw_path = query.path.as_deref().unwrap_or_default().trim();
    let target = if raw_path.is_empty() {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
    } else {
        expand_tilde_str(raw_path)
    };

    let payload = run_blocking(move || browse_directory(&target)).await??;
    Ok(json_no_store(StatusCode::OK, payload))
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
    let folder_path = required_trimmed_string(&map, "folder_path")?;
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
    let name = required_trimmed_string(&map, "name")?;
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
    let reset_order = optional_bool_or(&map, "reset_order", false);
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
    let episode_id = required_trimmed_string(&map, "episode_id")?;
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
    let watched = optional_bool_or(&map, "watched", true);
    let reset_position = optional_bool_or(&map, "reset_position", false);

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
// Show Poster & Watch History Export/Import Endpoints
// ---------------------------------------------------------------------------

pub(super) async fn handle_show_poster(
    State(state): State<AppState>,
    AxumPath(show_id): AxumPath<String>,
) -> Result<Response> {
    let store = state.store.clone();
    let (bytes, mime) = run_blocking(move || {
        let Some((show, _)) = store.get_show(&show_id)? else {
            return Err(NotflixError::NotFound("Show not found".to_string()));
        };
        let Some(poster_path) = find_show_poster(Path::new(&show.folder_path)) else {
            return Err(NotflixError::NotFound("Poster not found".to_string()));
        };
        let data = read_file_or_not_found(&poster_path, "Poster not found")?;
        let mime = guess_image_mime(&poster_path);
        Ok((data, mime))
    })
    .await??;

    Ok(binary_response(
        StatusCode::OK,
        mime,
        "public, max-age=3600",
        bytes,
    ))
}

pub(super) async fn handle_export_library(State(state): State<AppState>) -> Result<Response> {
    let store = state.store.clone();
    let export = run_blocking(move || store.export_watch_history()).await??;
    let payload = serde_json::to_value(export)
        .map_err(|e| NotflixError::Internal(format!("Failed to serialize export: {e}")))?;
    let mut resp = json_no_store(StatusCode::OK, payload);
    resp.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=\"notflix-watch-history.json\""),
    );
    Ok(resp)
}

pub(super) async fn handle_import_library(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Response> {
    if body.len() > MAX_JSON_BODY_BYTES {
        return Err(NotflixError::BadRequest(
            "Invalid JSON body: Request body too large".to_string(),
        ));
    }
    let export: WatchHistoryExport = serde_json::from_slice(&body)
        .map_err(|e| NotflixError::BadRequest(format!("Invalid watch history export JSON: {e}")))?;
    let store = state.store.clone();
    let summary = run_blocking(move || store.import_watch_history(&export)).await??;
    Ok(json_no_store(
        StatusCode::OK,
        json!({ "ok": true, "summary": summary }),
    ))
}
