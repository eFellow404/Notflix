//! Shared fixtures and helpers for Notflix integration tests.
#![allow(dead_code)]

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// Configurable HTTP response served by [`MockHttpServer`].
#[derive(Clone, Debug)]
pub struct MockResponse {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
    pub declared_content_length: Option<u64>,
}

impl MockResponse {
    pub fn ok(body: impl Into<Vec<u8>>, content_type: impl Into<String>) -> Self {
        Self::with_status(200, body, content_type)
    }

    pub fn binary(body: impl Into<Vec<u8>>) -> Self {
        Self::with_status(200, body, "application/octet-stream")
    }

    pub fn with_status(
        status: u16,
        body: impl Into<Vec<u8>>,
        content_type: impl Into<String>,
    ) -> Self {
        Self {
            status,
            content_type: content_type.into(),
            body: body.into(),
            declared_content_length: None,
        }
    }

    /// Build a response whose `Content-Length` header declares `declared_len` without
    /// needing to allocate that many bytes in memory (useful for testing size-cap rejections).
    pub fn with_declared_length(status: u16, declared_len: u64, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            content_type: "application/octet-stream".to_string(),
            body: body.into(),
            declared_content_length: Some(declared_len),
        }
    }
}

fn status_line_for(status: u16) -> String {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    };
    format!("HTTP/1.1 {status} {reason}")
}

/// Lightweight local `TcpListener`-backed HTTP server for integration tests.
pub struct MockHttpServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<String>>>,
    handle: Option<JoinHandle<()>>,
}

impl MockHttpServer {
    /// Start a server dispatching each normalized request path to `handler`.
    pub fn start<F>(handler: F) -> Self
    where
        F: Fn(&str) -> MockResponse + Send + Sync + 'static,
    {
        Self::start_with_base(move |path, _base| handler(path))
    }

    /// Start a server whose handler also receives the server's `http://127.0.0.1:<port>` base URL.
    pub fn start_with_base<F>(handler: F) -> Self
    where
        F: Fn(&str, &str) -> MockResponse + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock http server");
        let addr = listener.local_addr().expect("mock server addr");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_clone = Arc::clone(&requests);
        let base_url = format!("http://{addr}");

        let handle = std::thread::spawn(move || {
            while !stop_flag.load(Ordering::Relaxed) {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                if stop_flag.load(Ordering::Relaxed) {
                    break;
                }
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));

                let mut req_buf = [0u8; 4096];
                let n = stream.read(&mut req_buf).unwrap_or(0);
                if n == 0 {
                    continue;
                }
                let req_text = String::from_utf8_lossy(&req_buf[..n]);
                let mut first_line = req_text.lines().next().unwrap_or("").split_whitespace();
                let method = first_line.next().unwrap_or("GET");
                let raw_path = first_line.next().unwrap_or("/");
                let normalized_path = format!("/{}", raw_path.trim_start_matches('/'));

                requests_clone
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(normalized_path.clone());

                let resp = handler(&normalized_path, &base_url);
                let content_len = resp
                    .declared_content_length
                    .unwrap_or(resp.body.len() as u64);
                let status_line = status_line_for(resp.status);
                let header = format!(
                    "{status_line}\r\nContent-Type: {}\r\nContent-Length: {content_len}\r\nConnection: close\r\n\r\n",
                    resp.content_type
                );
                let _ = stream.write_all(header.as_bytes());
                if method != "HEAD" && !resp.body.is_empty() {
                    let _ = stream.write_all(&resp.body);
                }
                let _ = stream.flush();
            }
        });

        Self {
            addr,
            stop,
            requests,
            handle: Some(handle),
        }
    }

    /// Start a server serving static path-to-response mappings (404 on unknown paths).
    pub fn from_routes(routes: HashMap<String, MockResponse>) -> Self {
        Self::start(move |path| {
            routes.get(path).cloned().unwrap_or_else(|| {
                MockResponse::with_status(404, b"<html><body>Not Found</body></html>", "text/html")
            })
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    pub fn requests(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl Drop for MockHttpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.addr);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Produce a synthetic binary payload with valid OS executable magic bytes padded to `total_len`.
pub fn fake_valid_binary_for_os(os: &str, total_len: usize) -> Vec<u8> {
    let magic: &[u8] = match os {
        "linux" => b"\x7fELF",
        "windows" => b"MZ\x90\x00",
        "macos" => &[0xcf, 0xfa, 0xed, 0xfe],
        _ => b"\x7fELF",
    };
    let mut bytes = vec![0x42u8; total_len.max(magic.len())];
    bytes[..magic.len()].copy_from_slice(magic);
    bytes
}

/// Bind an ephemeral localhost port and immediately drop the listener to yield an unused port.
pub fn unused_local_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("local addr").port()
}

/// Assert that `path` has at least one executable permission bit set on Unix (no-op elsewhere).
pub fn assert_is_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(path).expect("metadata").permissions().mode();
        assert!(
            mode & 0o111 != 0,
            "expected {} to be executable (mode {mode:o})",
            path.display()
        );
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// Tiny deterministic SplitMix64 PRNG so tests need no external `rand` crate.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    pub fn uniform(&mut self, low: f64, high: f64) -> f64 {
        let unit = ((self.next_u64() >> 11) as f64) * (1.0 / ((1u64 << 53) as f64));
        low + unit * (high - low)
    }
}

pub fn rand_vec(rng: &mut Rng, dim: usize) -> Vec<f64> {
    (0..dim).map(|_| rng.uniform(20.0, 235.0)).collect()
}

pub fn perturb_vec(rng: &mut Rng, vec: &[f64], noise: f64) -> Vec<f64> {
    vec.iter()
        .map(|&v| (v + rng.uniform(-noise, noise)).clamp(0.0, 255.0))
        .collect()
}

pub fn make_episode_vectors(
    rng: &mut Rng,
    total_frames: usize,
    dim: usize,
    intro_template: Option<&[Vec<f64>]>,
    intro_offset: usize,
    noise: f64,
) -> Vec<Vec<f64>> {
    let mut frames: Vec<Vec<f64>> = (0..total_frames).map(|_| rand_vec(rng, dim)).collect();
    if let Some(template) = intro_template {
        for (idx, base) in template.iter().enumerate() {
            let pos = intro_offset + idx;
            if pos < total_frames {
                frames[pos] = if noise > 0.0 {
                    perturb_vec(rng, base, noise)
                } else {
                    base.clone()
                };
            }
        }
    }
    frames
}

/// Create `dir` (and any intermediate subdirectories) populated with dummy video files.
pub fn create_show_folder(dir: &Path, files: &[&str]) {
    fs::create_dir_all(dir).expect("create show folder");
    for rel in files {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create show subfolder");
        }
        fs::write(&path, b"video").expect("write dummy video file");
    }
}
