//! Intro and outro/credits detection HTTP endpoint.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{json, Map, Value};

use crate::error::{NotflixError, Result};
use crate::intro_detector::{
    build_fingerprint, detect_show_intros_server, detect_show_outros_server,
    discover_intro_template_default, find_shared_outros, match_episodes_against_template,
    EpisodeFingerprint, IntroSpan, IntroTemplate,
};
use crate::linked_list::EpisodeLinkedList;

use super::{
    json_no_store, optional_bool_or, optional_f64_or, parse_json_object, run_blocking,
    value_as_trimmed_string, AppState,
};

const MIN_TEMPLATE_STEP_SEC: f64 = 0.25;

type EpisodeSpec = (String, PathBuf, f64);

struct IntroDetectionPass {
    detected: HashMap<String, IntroSpan>,
    method: String,
    needs_visual: bool,
    mark_scanned: bool,
    intro_template: Option<IntroTemplate>,
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
        let Some(offset_sec) =
            optional_f64_or(obj.get("offset_sec"), 0.0).filter(|v| v.is_finite() && *v >= 0.0)
        else {
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

        parsed.push(
            build_fingerprint(
                ep_id,
                step_sec,
                &vectors,
                flat_flags.as_deref(),
                duration_sec,
                "visual",
                8.0,
            )
            .with_offset(offset_sec),
        );
    }
    parsed
}

fn episode_detection_inputs(ll: &EpisodeLinkedList) -> Vec<EpisodeSpec> {
    let non_extras: Vec<_> = ll.iter().filter(|n| !n.is_extra).collect();
    let targets = if non_extras.len() >= 2 {
        non_extras
    } else {
        ll.iter().collect()
    };
    targets
        .into_iter()
        .map(|n| (n.id.clone(), PathBuf::from(&n.file_path), n.duration_sec))
        .collect()
}

fn parse_intro_template(map: &Map<String, Value>) -> Result<Option<IntroTemplate>> {
    match map.get("intro_template") {
        None | Some(Value::Null) => Ok(None),
        Some(tmpl_val) => {
            let tmpl: IntroTemplate = serde_json::from_value(tmpl_val.clone())
                .map_err(|_| NotflixError::BadRequest("Invalid intro_template".to_string()))?;
            if tmpl.vectors.is_empty()
                || tmpl.vectors.len() != tmpl.flat_flags.len()
                || !tmpl.step_sec.is_finite()
                || tmpl.step_sec < MIN_TEMPLATE_STEP_SEC
            {
                return Err(NotflixError::BadRequest(
                    "Invalid intro_template".to_string(),
                ));
            }
            Ok(Some(tmpl))
        }
    }
}

fn run_server_intro_pass(ep_specs: &[EpisodeSpec]) -> IntroDetectionPass {
    let (detected, method, needs_visual) = detect_show_intros_server(ep_specs, true);
    IntroDetectionPass {
        detected,
        method,
        needs_visual,
        mark_scanned: !needs_visual,
        intro_template: None,
    }
}

fn run_intro_pass(
    map: &Map<String, Value>,
    ll: &EpisodeLinkedList,
    valid_ids: &HashSet<&str>,
    request_template: Option<IntroTemplate>,
    ep_specs: &mut Option<Vec<EpisodeSpec>>,
) -> Result<IntroDetectionPass> {
    match map.get("fingerprints") {
        None | Some(Value::Null) => {
            let specs = ep_specs.get_or_insert_with(|| episode_detection_inputs(ll));
            Ok(run_server_intro_pass(specs))
        }
        Some(raw_fps_val) => {
            let Some(raw_fps) = raw_fps_val.as_array() else {
                return Err(NotflixError::BadRequest(
                    "fingerprints must be a list".to_string(),
                ));
            };
            let parsed_fps = parse_client_fingerprints(raw_fps, valid_ids);
            let (detected, intro_template) = if let Some(tmpl) = request_template {
                let detected = match_episodes_against_template(&parsed_fps, &tmpl);
                (detected, Some(tmpl))
            } else {
                let discovered_template = discover_intro_template_default(&parsed_fps);
                let detected = discovered_template
                    .as_ref()
                    .map(|t| match_episodes_against_template(&parsed_fps, t))
                    .unwrap_or_default();
                (detected, discovered_template)
            };
            let mark_scanned = optional_bool_or(map, "mark_scanned", true);
            Ok(IntroDetectionPass {
                detected,
                method: "visual".to_string(),
                needs_visual: false,
                mark_scanned,
                intro_template,
            })
        }
    }
}

fn run_outro_pass(
    map: &Map<String, Value>,
    ll: &EpisodeLinkedList,
    valid_ids: &HashSet<&str>,
    ep_specs: &mut Option<Vec<EpisodeSpec>>,
) -> Result<HashMap<String, IntroSpan>> {
    match map.get("tail_fingerprints") {
        None | Some(Value::Null) => {
            let specs = ep_specs.get_or_insert_with(|| episode_detection_inputs(ll));
            Ok(detect_show_outros_server(specs, true))
        }
        Some(raw_tails_val) => {
            let Some(raw_tails) = raw_tails_val.as_array() else {
                return Err(NotflixError::BadRequest(
                    "tail_fingerprints must be a list".to_string(),
                ));
            };
            let parsed_tails = parse_client_fingerprints(raw_tails, valid_ids);
            if parsed_tails.len() >= 2 {
                Ok(find_shared_outros(&parsed_tails))
            } else {
                let specs = ep_specs.get_or_insert_with(|| episode_detection_inputs(ll));
                Ok(detect_show_outros_server(specs, true))
            }
        }
    }
}

pub(super) async fn handle_detect_intros(
    State(state): State<AppState>,
    AxumPath(show_id): AxumPath<String>,
    body: Bytes,
) -> Result<Response> {
    let map = parse_json_object(&body)?;
    let store = state.store.clone();
    let ffmpeg_manager = Arc::clone(&state.ffmpeg_manager);

    run_blocking(move || {
        let Some((_, ll)) = store.get_show(&show_id)? else {
            return Err(NotflixError::NotFound("Show not found".to_string()));
        };
        let force = optional_bool_or(&map, "force", false);
        let valid_ids: HashSet<&str> = ll.iter().map(|n| n.id.as_str()).collect();
        let request_template = parse_intro_template(&map)?;

        let mut ep_specs = None;
        let pass = run_intro_pass(&map, &ll, &valid_ids, request_template, &mut ep_specs)?;
        let outros = run_outro_pass(&map, &ll, &valid_ids, &mut ep_specs)?;

        let detected_count = pass.detected.len();
        let (mut updated_show, mut updated_ll) = store
            .save_show_intros(&show_id, &pass.detected, !force, pass.mark_scanned)?
            .ok_or_else(|| NotflixError::NotFound("Show not found".to_string()))?;

        let outro_count = outros.len();
        if !outros.is_empty() {
            if let Some((show_after, ll_after)) = store.save_show_outros(&show_id, &outros, true)? {
                updated_show = show_after;
                updated_ll = ll_after;
            }
        }

        let (ffmpeg_downloading, ffmpeg_progress) =
            if pass.needs_visual && ffmpeg_manager.is_downloading() {
                let ff_snap = ffmpeg_manager.snapshot();
                (true, ff_snap.progress_pct)
            } else {
                (false, None)
            };

        Ok(json_no_store(
            StatusCode::OK,
            json!({
                "show": updated_show,
                "linked_list": updated_ll.to_json(),
                "detected_count": detected_count,
                "outro_detected_count": outro_count,
                "method": pass.method,
                "needs_client_visual_scan": pass.needs_visual,
                "intro_template": pass.intro_template,
                "ffmpeg_downloading": ffmpeg_downloading,
                "ffmpeg_progress": ffmpeg_progress,
            }),
        ))
    })
    .await?
}
