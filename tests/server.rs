//! Integration tests for the Notflix HTTP server, REST API, static assets, and byte-range streaming.

use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::Request;
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::net::{TcpListener, TcpStream};

use notflix::server::{build_router, AppState};
use notflix::storage::NotflixStore;

struct TestServer {
    addr: SocketAddr,
    state: AppState,
    _tmp: TempDir,
    show_dir: std::path::PathBuf,
    base_dir: std::path::PathBuf,
}

impl TestServer {
    async fn start() -> Self {
        let tmp = TempDir::new().unwrap();
        let base_dir = tmp.path().to_path_buf();
        let show_dir = base_dir.join("SampleTV");
        fs::create_dir_all(&show_dir).unwrap();
        fs::write(
            show_dir.join("S01E02.mp4"),
            b"SECOND-EPISODE-BYTES-0123456789",
        )
        .unwrap();
        fs::write(
            show_dir.join("S01E01.mp4"),
            b"FIRST-EPISODE-BYTES-ABCDEFGHIJ",
        )
        .unwrap();

        let store = NotflixStore::new(Some(base_dir.join("test.db"))).unwrap();
        let state = AppState::new(store, None, None, None, true);
        let app = build_router(state.clone());

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Self {
            addr,
            state,
            _tmp: tmp,
            show_dir,
            base_dir,
        }
    }

    async fn raw_request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<Vec<u8>>,
    ) -> (u16, HashMap<String, String>, Vec<u8>) {
        let stream = TcpStream::connect(self.addr).await.unwrap();
        let io = TokioIo::new(stream);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();
        tokio::spawn(async move {
            let _ = conn.await;
        });

        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header("Host", format!("127.0.0.1:{}", self.addr.port()));

        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }

        let payload = body.unwrap_or_default();
        let req = builder.body(Full::new(Bytes::from(payload))).unwrap();
        let resp = sender.send_request(req).await.unwrap();

        let status = resp.status().as_u16();
        let mut resp_headers = HashMap::new();
        for (k, v) in resp.headers() {
            if let Ok(val) = v.to_str() {
                resp_headers.insert(k.as_str().to_ascii_lowercase(), val.to_string());
            }
        }
        let body_bytes = resp
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        (status, resp_headers, body_bytes)
    }

    async fn req_json(&self, method: &str, path: &str, payload: Option<Value>) -> (u16, Value) {
        let (headers, body) = match payload {
            Some(v) => (
                vec![("Content-Type", "application/json")],
                Some(serde_json::to_vec(&v).unwrap()),
            ),
            None => (vec![], None),
        };
        let (status, _, bytes) = self.raw_request(method, path, &headers, body).await;
        let json: Value = serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            panic!(
                "Invalid JSON response (status {status}): {e} — body: {}",
                String::from_utf8_lossy(&bytes)
            )
        });
        (status, json)
    }
}

struct SimpleRng(u64);

impl SimpleRng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_f64(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 11) as f64) / ((1u64 << 53) as f64)
    }

    fn uniform(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.next_f64()
    }
}

fn encode_query_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[tokio::test]
async fn test_full_api_flow_and_range_streaming() {
    let srv = TestServer::start().await;

    // 1. Health check & UI shell
    let (status, health) = srv.req_json("GET", "/api/health", None).await;
    assert_eq!(status, 200);
    assert_eq!(health["status"], "ok");
    assert!(health["build_sha"].is_string());
    assert!(!health["build_sha"].as_str().unwrap().is_empty());

    let (status, _, html_bytes) = srv.raw_request("GET", "/", &[], None).await;
    assert_eq!(status, 200);
    let html = String::from_utf8(html_bytes).unwrap();
    assert!(html.contains("NOTFLIX"));

    // 2. Browse filesystem endpoint
    let browse_url = format!(
        "/api/fs/browse?path={}",
        encode_query_component(&srv.show_dir.to_string_lossy())
    );
    let (status, browse) = srv.req_json("GET", &browse_url, None).await;
    assert_eq!(status, 200);
    assert_eq!(browse["direct_video_count"], 2);

    // 3. Add TV folder -> intelligently ordered linked list
    let (status, created) = srv
        .req_json(
            "POST",
            "/api/shows",
            Some(json!({
                "folder_path": srv.show_dir.to_string_lossy(),
                "name": "Sample Show"
            })),
        )
        .await;
    assert_eq!(status, 201);
    let show_id = created["show"]["id"].as_str().unwrap().to_string();
    let eps = created["linked_list"]["episodes"].as_array().unwrap();
    let filenames: Vec<&str> = eps
        .iter()
        .map(|e| e["filename"].as_str().unwrap())
        .collect();
    assert_eq!(filenames, vec!["S01E01.mp4", "S01E02.mp4"]);
    assert_eq!(eps[0]["next_id"], eps[1]["id"]);
    assert_eq!(eps[1]["prev_id"], eps[0]["id"]);

    let ep1_id = eps[0]["id"].as_str().unwrap().to_string();
    let ep2_id = eps[1]["id"].as_str().unwrap().to_string();

    // 4. Save playback timeplace via PATCH and POST (sendBeacon compatibility)
    let (status, prog) = srv
        .req_json(
            "PATCH",
            &format!("/api/episodes/{ep1_id}/progress"),
            Some(json!({ "position_sec": 42.75, "duration_sec": 1200.0 })),
        )
        .await;
    assert_eq!(status, 200);
    assert!((prog["episode"]["position_sec"].as_f64().unwrap() - 42.75).abs() < 1e-6);

    let (status, prog2) = srv
        .req_json(
            "POST",
            &format!("/api/episodes/{ep1_id}/progress"),
            Some(json!({ "position_sec": 88.5, "duration_sec": 1200.0 })),
        )
        .await;
    assert_eq!(status, 200);
    assert!((prog2["episode"]["position_sec"].as_f64().unwrap() - 88.5).abs() < 1e-6);

    // 5. Click Next Episode -> advances linked list current pointer
    let (status, nxt) = srv
        .req_json(
            "POST",
            &format!("/api/shows/{show_id}/next"),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(nxt["episode"]["id"], ep2_id);
    assert_eq!(nxt["linked_list"]["current_id"], ep2_id);

    // 6. Click Prev Episode -> returns to ep1 with saved timestamp intact
    let (status, prv) = srv
        .req_json(
            "POST",
            &format!("/api/shows/{show_id}/prev"),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(prv["episode"]["id"], ep1_id);
    assert!((prv["episode"]["position_sec"].as_f64().unwrap() - 88.5).abs() < 1e-6);

    // 7. HTTP 206 Partial Content byte-range streaming
    let (status, headers, body) = srv
        .raw_request(
            "GET",
            &format!("/api/episodes/{ep1_id}/stream"),
            &[("Range", "bytes=0-12")],
            None,
        )
        .await;
    assert_eq!(status, 206);
    assert_eq!(
        headers.get("accept-ranges").map(String::as_str),
        Some("bytes")
    );
    assert_eq!(
        headers.get("content-range").map(String::as_str),
        Some("bytes 0-12/30")
    );
    assert_eq!(body, b"FIRST-EPISODE");

    // 7b. HEAD stream request returns headers with empty body
    let (status, headers, body) = srv
        .raw_request("HEAD", &format!("/api/episodes/{ep1_id}/stream"), &[], None)
        .await;
    assert_eq!(status, 200);
    assert_eq!(
        headers.get("content-length").map(String::as_str),
        Some("30")
    );
    assert!(body.is_empty());

    // 8. Invalid byte range returns 416
    let (status, headers, _) = srv
        .raw_request(
            "GET",
            &format!("/api/episodes/{ep1_id}/stream"),
            &[("Range", "bytes=999-1000")],
            None,
        )
        .await;
    assert_eq!(status, 416);
    assert_eq!(
        headers.get("content-range").map(String::as_str),
        Some("bytes */30")
    );

    // 9. Intro detection endpoint (server pass -> requests client visual scan without ffmpeg/chapters)
    let (status, det) = srv
        .req_json(
            "POST",
            &format!("/api/shows/{show_id}/detect-intros"),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, 200);
    assert!(det.get("needs_client_visual_scan").is_some());

    // 10. Intro detection with client-supplied visual fingerprints
    let mut rng = SimpleRng::new(7);
    let tmpl: Vec<Vec<f64>> = (0..20)
        .map(|_| (0..36).map(|_| rng.uniform(20.0, 235.0)).collect())
        .collect();
    let mut ep1_vecs: Vec<Vec<f64>> = (0..80)
        .map(|_| (0..36).map(|_| rng.uniform(20.0, 235.0)).collect())
        .collect();
    let mut ep2_vecs: Vec<Vec<f64>> = (0..80)
        .map(|_| (0..36).map(|_| rng.uniform(20.0, 235.0)).collect())
        .collect();
    for (idx, vec) in tmpl.iter().enumerate() {
        ep1_vecs[10 + idx] = vec.clone();
        ep2_vecs[30 + idx] = vec.clone();
    }

    let (status, vis) = srv
        .req_json(
            "POST",
            &format!("/api/shows/{show_id}/detect-intros"),
            Some(json!({
                "fingerprints": [
                    { "episode_id": ep1_id, "step_sec": 1.5, "vectors": ep1_vecs },
                    { "episode_id": ep2_id, "step_sec": 1.5, "vectors": ep2_vecs }
                ],
                "mark_scanned": true
            })),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(vis["method"], "visual");
    assert_eq!(vis["detected_count"], 2);
    let ep_by_id: HashMap<&str, &Value> = vis["linked_list"]["episodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["id"].as_str().unwrap(), e))
        .collect();
    assert!(
        (ep_by_id[ep1_id.as_str()]["intro_start_sec"]
            .as_f64()
            .unwrap()
            - 15.0)
            .abs()
            <= 2.0
    );
    assert!(
        (ep_by_id[ep2_id.as_str()]["intro_start_sec"]
            .as_f64()
            .unwrap()
            - 45.0)
            .abs()
            <= 2.0
    );

    // 11. Manual intro override & clear
    let (status, manual) = srv
        .req_json(
            "PATCH",
            &format!("/api/episodes/{ep1_id}/intro"),
            Some(json!({ "intro_start_sec": 20.0, "intro_end_sec": 80.0 })),
        )
        .await;
    assert_eq!(status, 200);
    assert!((manual["episode"]["intro_start_sec"].as_f64().unwrap() - 20.0).abs() < 1e-6);
    assert_eq!(manual["episode"]["intro_source"], "manual");
    assert_eq!(manual["episode"]["next_id"], ep2_id);

    let (status, cleared) = srv
        .req_json(
            "PATCH",
            &format!("/api/episodes/{ep1_id}/intro"),
            Some(json!({ "clear": true })),
        )
        .await;
    assert_eq!(status, 200);
    assert!(cleared["episode"]["intro_start_sec"].is_null());

    // 12. Progress PATCH preserves next_id and watched toggle does not hijack current_episode_id
    let (status, watched_ep2) = srv
        .req_json(
            "PATCH",
            &format!("/api/episodes/{ep2_id}/progress"),
            Some(json!({ "watched": true, "set_current": false })),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(watched_ep2["episode"]["watched"], true);
    assert_eq!(watched_ep2["episode"]["prev_id"], ep1_id);

    let (status, show_after) = srv
        .req_json("GET", &format!("/api/shows/{show_id}"), None)
        .await;
    assert_eq!(status, 200);
    assert_eq!(show_after["linked_list"]["current_id"], ep1_id);

    // 13. Bulk season watched endpoint + show rename + season summary stats
    let (status, bulk) = srv
        .req_json(
            "POST",
            &format!("/api/shows/{show_id}/watched"),
            Some(json!({
                "episode_ids": [ep1_id, ep2_id],
                "watched": false,
                "reset_position": true
            })),
        )
        .await;
    assert_eq!(status, 200);
    assert!(bulk["linked_list"]["episodes"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["watched"] == false));

    let (status, renamed) = srv
        .req_json(
            "PATCH",
            &format!("/api/shows/{show_id}"),
            Some(json!({ "name": "Renamed Sample Show" })),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(renamed["show"]["name"], "Renamed Sample Show");

    let (status, shows_list) = srv.req_json("GET", "/api/shows", None).await;
    assert_eq!(status, 200);
    assert_eq!(shows_list["shows"][0]["season_count"], 1);
    assert_eq!(shows_list["shows"][0]["main_episode_count"], 2);
}

#[tokio::test]
async fn test_static_js_serving_missing_and_path_traversal() {
    let srv = TestServer::start().await;

    let (status, headers, body) = srv
        .raw_request("GET", "/static/js/utils.js", &[], None)
        .await;
    assert_eq!(status, 200);
    assert!(headers
        .get("content-type")
        .unwrap()
        .contains("application/javascript"));
    assert!(!body.is_empty());

    let (status, _, _) = srv
        .raw_request("GET", "/static/js/does-not-exist.js", &[], None)
        .await;
    assert_eq!(status, 404);

    let (status, _, _) = srv
        .raw_request("GET", "/static/js/../../Cargo.toml", &[], None)
        .await;
    assert_eq!(status, 403);
}

#[tokio::test]
async fn test_update_endpoints_never_crash_when_non_repo_or_offline() {
    let srv = TestServer::start().await;
    srv.state.set_repo_dir(srv.base_dir.clone());

    let (status, info) = srv.req_json("GET", "/api/update/status", None).await;
    assert_eq!(status, 200);
    assert_eq!(info["status"], "not_a_repo");

    let (status, checked) = srv
        .req_json(
            "POST",
            "/api/update/check",
            Some(json!({ "timeout_sec": 2 })),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(checked["status"], "not_a_repo");

    let (status, applied) = srv
        .req_json(
            "POST",
            "/api/update/apply",
            Some(json!({ "timeout_sec": 2 })),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(applied["updated"], false);
}

#[tokio::test]
async fn test_show_poster_endpoint_and_has_poster_flag() {
    let srv = TestServer::start().await;

    let (status, created) = srv
        .req_json(
            "POST",
            "/api/shows",
            Some(json!({
                "folder_path": srv.show_dir.to_string_lossy(),
                "name": "Poster Show"
            })),
        )
        .await;
    assert_eq!(status, 201);
    let show_id = created["show"]["id"].as_str().unwrap().to_string();

    // Without a poster file: has_poster is false and /poster returns 404
    let (status, shows_before) = srv.req_json("GET", "/api/shows", None).await;
    assert_eq!(status, 200);
    assert_eq!(shows_before["shows"][0]["has_poster"], false);

    let (status, _, _) = srv
        .raw_request("GET", &format!("/api/shows/{show_id}/poster"), &[], None)
        .await;
    assert_eq!(status, 404);

    // Add poster.jpg -> has_poster becomes true and /poster serves image/jpeg
    let fake_jpeg = b"\xff\xd8\xff\xe0\x00\x10JFIF-POSTER-BYTES".to_vec();
    fs::write(srv.show_dir.join("poster.jpg"), &fake_jpeg).unwrap();

    let (status, shows_after) = srv.req_json("GET", "/api/shows", None).await;
    assert_eq!(status, 200);
    assert_eq!(shows_after["shows"][0]["has_poster"], true);

    let (status, headers, body) = srv
        .raw_request("GET", &format!("/api/shows/{show_id}/poster"), &[], None)
        .await;
    assert_eq!(status, 200);
    assert_eq!(
        headers.get("content-type").map(String::as_str),
        Some("image/jpeg")
    );
    assert!(headers
        .get("cache-control")
        .unwrap_or(&String::new())
        .contains("max-age=3600"));
    assert_eq!(body, fake_jpeg);
}

#[tokio::test]
async fn test_episode_tracks_subtitles_and_thumbnail_endpoints() {
    let srv = TestServer::start().await;

    // Add sidecar SRT subtitle and sidecar JPG thumbnail for S01E01.mp4
    fs::write(
        srv.show_dir.join("S01E01.en.srt"),
        "1\n00:00:01,000 --> 00:00:03,500\nHello Notflix!\n",
    )
    .unwrap();
    let fake_thumb = b"\xff\xd8\xff\xe0\x00\x10JFIF-THUMB-BYTES".to_vec();
    fs::write(srv.show_dir.join("S01E01.jpg"), &fake_thumb).unwrap();

    let (status, created) = srv
        .req_json(
            "POST",
            "/api/shows",
            Some(json!({
                "folder_path": srv.show_dir.to_string_lossy(),
                "name": "Track Show"
            })),
        )
        .await;
    assert_eq!(status, 201);
    let eps = created["linked_list"]["episodes"].as_array().unwrap();
    let ep1_id = eps[0]["id"].as_str().unwrap().to_string();
    let ep2_id = eps[1]["id"].as_str().unwrap().to_string();

    // GET /api/episodes/{id}/tracks
    let (status, tracks) = srv
        .req_json("GET", &format!("/api/episodes/{ep1_id}/tracks"), None)
        .await;
    assert_eq!(status, 200);
    assert_eq!(tracks["subtitles"].as_array().unwrap().len(), 1);
    assert!(tracks["audio"].is_array());
    assert!(tracks["needs_remux_hint"].is_boolean());

    // GET /api/episodes/{id}/subtitles/0 -> WebVTT converted from SRT
    let (status, headers, body) = srv
        .raw_request(
            "GET",
            &format!("/api/episodes/{ep1_id}/subtitles/0"),
            &[],
            None,
        )
        .await;
    assert_eq!(status, 200);
    assert!(headers.get("content-type").unwrap().contains("text/vtt"));
    let vtt = String::from_utf8(body).unwrap();
    assert!(vtt.starts_with("WEBVTT"));
    assert!(vtt.contains("00:00:01.000 --> 00:00:03.500"));
    assert!(vtt.contains("Hello Notflix!"));

    // Out-of-range subtitle index returns 404
    let (status, _, _) = srv
        .raw_request(
            "GET",
            &format!("/api/episodes/{ep1_id}/subtitles/9"),
            &[],
            None,
        )
        .await;
    assert_eq!(status, 404);

    // Sidecar thumbnail returns 200 image/jpeg
    let (status, headers, body) = srv
        .raw_request(
            "GET",
            &format!("/api/episodes/{ep1_id}/thumbnail"),
            &[],
            None,
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(
        headers.get("content-type").map(String::as_str),
        Some("image/jpeg")
    );
    assert_eq!(body, fake_thumb);

    // Episode without sidecar thumbnail (and non-decodable dummy bytes) returns 404
    let (status, _, _) = srv
        .raw_request(
            "GET",
            &format!("/api/episodes/{ep2_id}/thumbnail"),
            &[],
            None,
        )
        .await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn test_detect_intros_with_tail_fingerprints_saves_outro() {
    let srv = TestServer::start().await;

    let (status, created) = srv
        .req_json(
            "POST",
            "/api/shows",
            Some(json!({
                "folder_path": srv.show_dir.to_string_lossy(),
                "name": "Outro Show"
            })),
        )
        .await;
    assert_eq!(status, 201);
    let show_id = created["show"]["id"].as_str().unwrap().to_string();
    let eps = created["linked_list"]["episodes"].as_array().unwrap();
    let ep1_id = eps[0]["id"].as_str().unwrap().to_string();
    let ep2_id = eps[1]["id"].as_str().unwrap().to_string();

    let mut rng = SimpleRng::new(42);
    let shared_outro: Vec<Vec<f64>> = (0..22)
        .map(|_| (0..36).map(|_| rng.uniform(25.0, 230.0)).collect())
        .collect();
    let mut tail1: Vec<Vec<f64>> = (0..80)
        .map(|_| (0..36).map(|_| rng.uniform(25.0, 230.0)).collect())
        .collect();
    let mut tail2: Vec<Vec<f64>> = (0..80)
        .map(|_| (0..36).map(|_| rng.uniform(25.0, 230.0)).collect())
        .collect();
    for (i, vec) in shared_outro.iter().enumerate() {
        tail1[40 + i] = vec.clone();
        tail2[44 + i] = vec.clone();
    }

    let (status, det) = srv
        .req_json(
            "POST",
            &format!("/api/shows/{show_id}/detect-intros"),
            Some(json!({
                "fingerprints": [],
                "tail_fingerprints": [
                    { "episode_id": ep1_id, "duration_sec": 1320.0, "step_sec": 1.5, "vectors": tail1 },
                    { "episode_id": ep2_id, "duration_sec": 1320.0, "step_sec": 1.5, "vectors": tail2 }
                ],
                "mark_scanned": true
            })),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(det["outro_detected_count"], 2);
    let ep_by_id: HashMap<&str, &Value> = det["linked_list"]["episodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["id"].as_str().unwrap(), e))
        .collect();
    let outro1 = ep_by_id[ep1_id.as_str()]["outro_start_sec"]
        .as_f64()
        .expect("ep1 should have outro_start_sec");
    let outro2 = ep_by_id[ep2_id.as_str()]["outro_start_sec"]
        .as_f64()
        .expect("ep2 should have outro_start_sec");
    assert!(outro1 > 1200.0 && outro1 < 1320.0);
    assert!(outro2 > 1200.0 && outro2 < 1320.0);
}

#[tokio::test]
async fn test_library_export_and_import_roundtrip() {
    let srv = TestServer::start().await;

    let (status, created) = srv
        .req_json(
            "POST",
            "/api/shows",
            Some(json!({
                "folder_path": srv.show_dir.to_string_lossy(),
                "name": "Export Show"
            })),
        )
        .await;
    assert_eq!(status, 201);
    let show_id = created["show"]["id"].as_str().unwrap().to_string();
    let eps = created["linked_list"]["episodes"].as_array().unwrap();
    let ep1_id = eps[0]["id"].as_str().unwrap().to_string();
    let ep2_id = eps[1]["id"].as_str().unwrap().to_string();

    // Populate progress, watched state, intro, and outro markers
    let (status, _) = srv
        .req_json(
            "PATCH",
            &format!("/api/episodes/{ep1_id}/progress"),
            Some(json!({ "position_sec": 145.5, "duration_sec": 1320.0 })),
        )
        .await;
    assert_eq!(status, 200);

    let (status, _) = srv
        .req_json(
            "PATCH",
            &format!("/api/episodes/{ep2_id}/progress"),
            Some(json!({ "watched": true, "set_current": false })),
        )
        .await;
    assert_eq!(status, 200);

    let (status, _) = srv
        .req_json(
            "PATCH",
            &format!("/api/episodes/{ep1_id}/intro"),
            Some(json!({ "intro_start_sec": 18.0, "intro_end_sec": 78.0 })),
        )
        .await;
    assert_eq!(status, 200);

    let mut rng = SimpleRng::new(99);
    let shared_outro: Vec<Vec<f64>> = (0..22)
        .map(|_| (0..36).map(|_| rng.uniform(25.0, 230.0)).collect())
        .collect();
    let mut tail1: Vec<Vec<f64>> = (0..80)
        .map(|_| (0..36).map(|_| rng.uniform(25.0, 230.0)).collect())
        .collect();
    let mut tail2: Vec<Vec<f64>> = (0..80)
        .map(|_| (0..36).map(|_| rng.uniform(25.0, 230.0)).collect())
        .collect();
    for (i, vec) in shared_outro.iter().enumerate() {
        tail1[42 + i] = vec.clone();
        tail2[42 + i] = vec.clone();
    }
    let (status, det) = srv
        .req_json(
            "POST",
            &format!("/api/shows/{show_id}/detect-intros"),
            Some(json!({
                "fingerprints": [],
                "tail_fingerprints": [
                    { "episode_id": ep1_id, "duration_sec": 1320.0, "step_sec": 1.5, "vectors": tail1 },
                    { "episode_id": ep2_id, "duration_sec": 1320.0, "step_sec": 1.5, "vectors": tail2 }
                ]
            })),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(det["outro_detected_count"], 2);
    let expected_outro = det["linked_list"]["episodes"][0]["outro_start_sec"]
        .as_f64()
        .unwrap();

    // Export watch history
    let (status, headers, export_bytes) = srv
        .raw_request("GET", "/api/library/export", &[], None)
        .await;
    assert_eq!(status, 200);
    assert!(headers
        .get("content-disposition")
        .unwrap_or(&String::new())
        .contains("notflix-watch-history.json"));
    let exported: Value = serde_json::from_slice(&export_bytes).unwrap();

    // Delete & re-add show so all progress/markers start fresh with new IDs
    let (status, _) = srv
        .req_json("DELETE", &format!("/api/shows/{show_id}"), None)
        .await;
    assert_eq!(status, 200);

    let (status, recreated) = srv
        .req_json(
            "POST",
            "/api/shows",
            Some(json!({
                "folder_path": srv.show_dir.to_string_lossy(),
                "name": "Export Show"
            })),
        )
        .await;
    assert_eq!(status, 201);
    let new_show_id = recreated["show"]["id"].as_str().unwrap().to_string();

    // Invalid import body returns 400
    let (status, _) = srv
        .req_json("POST", "/api/library/import", Some(json!({ "bad": true })))
        .await;
    assert_eq!(status, 400);

    // Import exported watch history
    let (status, imported) = srv
        .req_json("POST", "/api/library/import", Some(exported))
        .await;
    assert_eq!(status, 200);
    assert_eq!(imported["ok"], true);
    assert!(imported["summary"].is_object());

    // Verify progress, watched flag, intro, and outro markers were restored
    let (status, restored) = srv
        .req_json("GET", &format!("/api/shows/{new_show_id}"), None)
        .await;
    assert_eq!(status, 200);
    let restored_eps = restored["linked_list"]["episodes"].as_array().unwrap();
    let r1 = &restored_eps[0];
    let r2 = &restored_eps[1];
    assert!((r1["position_sec"].as_f64().unwrap() - 145.5).abs() < 1e-6);
    assert_eq!(r2["watched"], true);
    assert!((r1["intro_start_sec"].as_f64().unwrap() - 18.0).abs() < 1e-6);
    assert!((r1["intro_end_sec"].as_f64().unwrap() - 78.0).abs() < 1e-6);
    assert!((r1["outro_start_sec"].as_f64().unwrap() - expected_outro).abs() < 1e-3);
}
