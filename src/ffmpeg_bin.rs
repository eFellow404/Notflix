//! Automatic discovery, validation, and background downloading of `ffmpeg` and `ffprobe`.
//!
//! Resolves user-configured binaries (`NOTFLIX_FFMPEG_PATH` / `NOTFLIX_FFPROBE_PATH`),
//! system `PATH` binaries, and Notflix-managed binaries stored alongside the database
//! (`<db_dir>/bin` or `NOTFLIX_FFMPEG_DIR`).

use std::fs::{self, File};
use std::io::{BufWriter, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::storage::default_db_path;
#[cfg(unix)]
use crate::util::set_executable_permissions;
use crate::util::{
    build_http_agent, clamp_timeout_secs, env_var_trimmed, http_user_agent, parse_env_flag_enabled,
    resolve_path, validate_executable_magic_for_os as util_validate_magic,
};

/// Default GitHub Releases base URL serving static `ffmpeg` and `ffprobe` binaries.
pub const DEFAULT_FFMPEG_BASE_URL: &str =
    "https://github.com/eugeneware/ffmpeg-static/releases/download/b6.1.1";

/// Minimum accepted size for a downloaded `ffmpeg` or `ffprobe` binary (1 MiB).
pub const DEFAULT_MIN_BINARY_BYTES: u64 = 1024 * 1024;

/// Maximum accepted size for a downloaded `ffmpeg` or `ffprobe` binary (200 MiB).
pub const MAX_BINARY_BYTES: u64 = 200 * 1024 * 1024;

/// Default read timeout (in seconds) for binary downloads.
pub const DEFAULT_DOWNLOAD_TIMEOUT_SEC: f64 = 120.0;

const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const STREAM_CHUNK_BYTES: usize = 65_536;
const MIN_DOWNLOAD_TIMEOUT_SEC: f64 = 5.0;
const MAX_DOWNLOAD_TIMEOUT_SEC: f64 = 600.0;

static PART_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Current availability and download progress of `ffmpeg` and `ffprobe`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FfmpegStatus {
    pub status: String,
    pub progress_pct: Option<u8>,
    pub message: String,
    pub ffmpeg_available: bool,
    pub ffprobe_available: bool,
}

/// Return the upstream release asset suffix for the given `(os, arch)` pair.
pub fn platform_asset_suffix_for(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Some("linux-x64"),
        ("linux", "aarch64") => Some("linux-arm64"),
        ("macos", "aarch64") => Some("darwin-arm64"),
        ("macos", "x86_64") => Some("darwin-x64"),
        ("windows", "x86_64") => Some("win32-x64"),
        _ => None,
    }
}

/// Return the upstream release asset filename for `tool` on the given `(os, arch)` pair.
pub fn asset_name_for(tool: &str, os: &str, arch: &str) -> Option<String> {
    platform_asset_suffix_for(os, arch).map(|suffix| format!("{tool}-{suffix}"))
}

/// Return the local executable filename for `tool` on `os`.
pub fn binary_filename_for(tool: &str, os: &str) -> String {
    if os == "windows" {
        format!("{tool}.exe")
    } else {
        tool.to_string()
    }
}

/// Validate executable header magic bytes for the specified target OS (`"linux"`, `"windows"`, `"macos"`).
pub fn validate_executable_magic_for_os(bytes: &[u8], os: &str) -> bool {
    util_validate_magic(bytes, os)
}

fn auto_ffmpeg_enabled_from_env(cli_disabled: bool, env_value: Option<&str>) -> bool {
    !cli_disabled && parse_env_flag_enabled(env_value)
}

/// Return whether automatic `ffmpeg`/`ffprobe` downloading is enabled.
pub fn auto_ffmpeg_enabled(cli_disabled: bool) -> bool {
    let env_value = std::env::var("NOTFLIX_AUTO_FFMPEG").ok();
    auto_ffmpeg_enabled_from_env(cli_disabled, env_value.as_deref())
}

/// Resolve the managed directory where Notflix stores downloaded `ffmpeg`/`ffprobe` binaries.
pub fn default_ffmpeg_dir() -> PathBuf {
    if let Some(env_dir) = env_var_trimmed("NOTFLIX_FFMPEG_DIR") {
        return resolve_path(Path::new(&env_dir));
    }
    default_db_path()
        .parent()
        .map(|p| p.join("bin"))
        .unwrap_or_else(|| PathBuf::from("bin"))
}

/// Resolve a tool executable using explicit env override, `PATH` lookup, and managed directory fallback.
pub fn resolve_tool_with(
    env_path: Option<&Path>,
    path_lookup: Option<PathBuf>,
    managed_dir: &Path,
    tool: &str,
) -> Option<PathBuf> {
    if let Some(p) = env_path.filter(|p| p.is_file()) {
        return Some(p.to_path_buf());
    }
    if let Some(p) = path_lookup.filter(|p| p.is_file()) {
        return Some(p);
    }
    let managed = managed_dir.join(binary_filename_for(tool, std::env::consts::OS));
    if managed.is_file() {
        return Some(managed);
    }
    None
}

fn env_override_path(var_name: &str) -> Option<PathBuf> {
    env_var_trimmed(var_name).map(|val| resolve_path(Path::new(&val)))
}

/// Resolve `ffmpeg` checking `NOTFLIX_FFMPEG_PATH`, `PATH`, and `managed_dir`.
pub fn resolve_ffmpeg_in(managed_dir: &Path) -> Option<PathBuf> {
    let env_path = env_override_path("NOTFLIX_FFMPEG_PATH");
    let path_lookup = which::which("ffmpeg").ok();
    resolve_tool_with(env_path.as_deref(), path_lookup, managed_dir, "ffmpeg")
}

/// Resolve `ffprobe` checking `NOTFLIX_FFPROBE_PATH`, `PATH`, and `managed_dir`.
pub fn resolve_ffprobe_in(managed_dir: &Path) -> Option<PathBuf> {
    let env_path = env_override_path("NOTFLIX_FFPROBE_PATH");
    let path_lookup = which::which("ffprobe").ok();
    resolve_tool_with(env_path.as_deref(), path_lookup, managed_dir, "ffprobe")
}

/// Resolve `ffmpeg` using the default managed directory.
pub fn resolve_ffmpeg() -> Option<PathBuf> {
    resolve_ffmpeg_in(&default_ffmpeg_dir())
}

/// Resolve `ffprobe` using the default managed directory.
pub fn resolve_ffprobe() -> Option<PathBuf> {
    resolve_ffprobe_in(&default_ffmpeg_dir())
}

/// Best-effort removal of stale `*.part.*` temporary downloads in `dir`.
pub fn cleanup_stale_partial_downloads(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().contains(".part.") {
            let path = entry.path();
            if path.is_file() {
                let _ = fs::remove_file(path);
            }
        }
    }
}

/// RAII guard that removes a staged temporary download file on drop unless disarmed.
struct TempFileGuard<'a> {
    path: &'a Path,
    armed: bool,
}

impl<'a> TempFileGuard<'a> {
    fn new(path: &'a Path) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempFileGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(self.path);
        }
    }
}

fn fetch_binary_response(
    tool: &str,
    url: &str,
    timeout_sec: f64,
) -> Result<(ureq::Response, Option<u64>), String> {
    let read_timeout = clamp_timeout_secs(
        timeout_sec,
        MIN_DOWNLOAD_TIMEOUT_SEC,
        MAX_DOWNLOAD_TIMEOUT_SEC,
        DEFAULT_DOWNLOAD_TIMEOUT_SEC,
    );
    let agent = build_http_agent(HTTP_CONNECT_TIMEOUT, read_timeout);
    let user_agent = http_user_agent();

    let response = agent
        .get(url)
        .set("User-Agent", &user_agent)
        .call()
        .map_err(|err| match err {
            ureq::Error::Status(code, _) => format!("HTTP {code} while downloading {tool}"),
            ureq::Error::Transport(transport) => {
                format!("Network error while downloading {tool}: {transport}")
            }
        })?;

    let status = response.status();
    if !(200..300).contains(&status) {
        return Err(format!("Unexpected HTTP status {status} for {tool}"));
    }

    let content_length = response
        .header("Content-Length")
        .and_then(|value| value.trim().parse::<u64>().ok());

    if let Some(len) = content_length.filter(|&len| len > MAX_BINARY_BYTES) {
        return Err(format!(
            "Refusing to download {tool}: Content-Length ({len} bytes) exceeds {} MiB limit",
            MAX_BINARY_BYTES / (1024 * 1024)
        ));
    }

    Ok((response, content_length))
}

fn stream_and_validate_to_file<F>(
    tool: &str,
    response: ureq::Response,
    content_length: Option<u64>,
    temp_path: &Path,
    os: &str,
    min_bytes: u64,
    on_progress: &mut F,
) -> Result<(), String>
where
    F: FnMut(u8),
{
    let file = File::create(temp_path).map_err(|err| {
        format!(
            "Failed to create temporary file {}: {err}",
            temp_path.display()
        )
    })?;
    let mut writer = BufWriter::new(file);
    let mut reader = response.into_reader();

    let mut buf = [0u8; STREAM_CHUNK_BYTES];
    let mut magic = [0u8; 4];
    let mut magic_len: usize = 0;
    let mut magic_checked = false;
    let mut total_bytes: u64 = 0;
    let mut last_pct: Option<u8> = None;

    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) => return Err(format!("Failed while reading {tool} download: {err}")),
        };

        if magic_len < 4 {
            let copy_len = (4 - magic_len).min(n);
            magic[magic_len..magic_len + copy_len].copy_from_slice(&buf[..copy_len]);
            magic_len += copy_len;
        }

        if !magic_checked && magic_len >= 4 {
            if !validate_executable_magic_for_os(&magic[..magic_len], os) {
                return Err(
                    "Downloaded file is not a valid executable for this platform".to_string(),
                );
            }
            magic_checked = true;
        }

        total_bytes = total_bytes.saturating_add(n as u64);
        if total_bytes > MAX_BINARY_BYTES {
            return Err(format!(
                "Downloaded {tool} binary exceeds {} MiB safety limit",
                MAX_BINARY_BYTES / (1024 * 1024)
            ));
        }

        writer
            .write_all(&buf[..n])
            .map_err(|err| format!("Failed writing temporary file for {tool}: {err}"))?;

        if let Some(expected) = content_length.filter(|&expected| expected > 0) {
            let pct = ((u128::from(total_bytes) * 100) / u128::from(expected)).min(100) as u8;
            if last_pct != Some(pct) {
                last_pct = Some(pct);
                on_progress(pct);
            }
        }
    }

    if !magic_checked && !validate_executable_magic_for_os(&magic[..magic_len], os) {
        return Err("Downloaded file is not a valid executable for this platform".to_string());
    }

    writer
        .flush()
        .map_err(|err| format!("Failed to flush temporary file for {tool}: {err}"))?;

    // Drop the buffered file handle before chmodding or renaming (required on Windows).
    drop(writer);

    if total_bytes < min_bytes {
        return Err(format!(
            "Downloaded {tool} binary is too small ({total_bytes} bytes < {min_bytes} bytes minimum)"
        ));
    }

    Ok(())
}

fn finalize_downloaded_binary(
    tool: &str,
    temp_path: &Path,
    final_path: &Path,
) -> Result<(), String> {
    #[cfg(unix)]
    set_executable_permissions(temp_path)
        .map_err(|err| format!("Failed to set executable permissions on {tool}: {err}"))?;

    #[cfg(windows)]
    {
        if let Err(err) = fs::remove_file(final_path) {
            if err.kind() != ErrorKind::NotFound {
                return Err(format!(
                    "Failed to replace existing {tool} binary at {}: {err}",
                    final_path.display()
                ));
            }
        }
    }

    fs::rename(temp_path, final_path).map_err(|err| {
        format!(
            "Failed to move downloaded {tool} into place at {}: {err}",
            final_path.display()
        )
    })
}

/// Stream a binary from `url` to a temporary file in `target_dir`, validate its executable
/// header and size bounds, mark executable on Unix, and atomically rename into place.
pub fn download_tool_binary<F>(
    tool: &str,
    url: &str,
    target_dir: &Path,
    os: &str,
    min_bytes: u64,
    timeout_sec: f64,
    mut on_progress: F,
) -> Result<PathBuf, String>
where
    F: FnMut(u8),
{
    fs::create_dir_all(target_dir).map_err(|err| {
        format!(
            "Failed to create binary directory {}: {err}",
            target_dir.display()
        )
    })?;

    let (response, content_length) = fetch_binary_response(tool, url, timeout_sec)?;

    let final_name = binary_filename_for(tool, os);
    let final_path = target_dir.join(&final_name);
    let seq = PART_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp_path = target_dir.join(format!("{final_name}.part.{}.{seq}", std::process::id()));
    let mut temp_guard = TempFileGuard::new(&temp_path);

    stream_and_validate_to_file(
        tool,
        response,
        content_length,
        &temp_path,
        os,
        min_bytes,
        &mut on_progress,
    )?;
    finalize_downloaded_binary(tool, &temp_path, &final_path)?;
    temp_guard.disarm();

    Ok(final_path)
}

fn ready_message(has_ffprobe: bool) -> String {
    if has_ffprobe {
        "ffmpeg and ffprobe are ready.".to_string()
    } else {
        "ffmpeg is ready (ffprobe unavailable).".to_string()
    }
}

fn resolve_base_url(base_url: Option<&str>) -> String {
    let raw = base_url
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| env_var_trimmed("NOTFLIX_FFMPEG_BASE_URL"))
        .unwrap_or_else(|| DEFAULT_FFMPEG_BASE_URL.to_string());
    raw.trim_end_matches('/').to_string()
}

/// Thread-safe coordinator tracking `ffmpeg`/`ffprobe` availability and downloads.
#[derive(Debug)]
pub struct FfmpegManager {
    state: Mutex<FfmpegStatus>,
    managed_dir: Option<PathBuf>,
}

impl Default for FfmpegManager {
    fn default() -> Self {
        Self::new()
    }
}

impl FfmpegManager {
    fn initial_status(has_ffmpeg: bool, has_ffprobe: bool, disabled: bool) -> FfmpegStatus {
        if has_ffmpeg {
            FfmpegStatus {
                status: "ready".to_string(),
                progress_pct: Some(100),
                message: ready_message(has_ffprobe),
                ffmpeg_available: true,
                ffprobe_available: has_ffprobe,
            }
        } else if disabled {
            FfmpegStatus {
                status: "disabled".to_string(),
                progress_pct: None,
                message: "Automatic ffmpeg download is disabled.".to_string(),
                ffmpeg_available: false,
                ffprobe_available: has_ffprobe,
            }
        } else {
            FfmpegStatus {
                status: "unavailable".to_string(),
                progress_pct: None,
                message: "ffmpeg is not installed yet.".to_string(),
                ffmpeg_available: false,
                ffprobe_available: has_ffprobe,
            }
        }
    }

    /// Create a manager using the default managed binary directory.
    pub fn new() -> Self {
        let dir = default_ffmpeg_dir();
        let has_ffmpeg = resolve_ffmpeg_in(&dir).is_some();
        let has_ffprobe = resolve_ffprobe_in(&dir).is_some();
        Self {
            state: Mutex::new(Self::initial_status(has_ffmpeg, has_ffprobe, false)),
            managed_dir: None,
        }
    }

    /// Create a manager with automatic downloading disabled.
    pub fn new_disabled() -> Self {
        let dir = default_ffmpeg_dir();
        let has_ffmpeg = resolve_ffmpeg_in(&dir).is_some();
        let has_ffprobe = resolve_ffprobe_in(&dir).is_some();
        Self {
            state: Mutex::new(Self::initial_status(has_ffmpeg, has_ffprobe, true)),
            managed_dir: None,
        }
    }

    /// Create a manager pinned to an explicit managed binary directory.
    pub fn with_dir(dir: PathBuf) -> Self {
        let has_ffmpeg = resolve_ffmpeg_in(&dir).is_some();
        let has_ffprobe = resolve_ffprobe_in(&dir).is_some();
        Self {
            state: Mutex::new(Self::initial_status(has_ffmpeg, has_ffprobe, false)),
            managed_dir: Some(dir),
        }
    }

    fn effective_dir(&self) -> PathBuf {
        self.managed_dir.clone().unwrap_or_else(default_ffmpeg_dir)
    }

    fn lock_state(&self) -> MutexGuard<'_, FfmpegStatus> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Return the current status snapshot, refreshing live availability when idle.
    pub fn snapshot(&self) -> FfmpegStatus {
        {
            let state = self.lock_state();
            if state.status == "downloading" {
                return state.clone();
            }
        }

        let dir = self.effective_dir();
        let has_ffmpeg = resolve_ffmpeg_in(&dir).is_some();
        let has_ffprobe = resolve_ffprobe_in(&dir).is_some();

        let mut state = self.lock_state();
        if state.status != "downloading" {
            state.ffmpeg_available = has_ffmpeg;
            state.ffprobe_available = has_ffprobe;
            if has_ffmpeg && state.status != "ready" {
                state.status = "ready".to_string();
                state.progress_pct = Some(100);
                state.message = ready_message(has_ffprobe);
            }
        }
        state.clone()
    }

    /// Return `true` only while `ffmpeg` itself is still being downloaded.
    ///
    /// Once `ffmpeg` is available, callers (such as audio intro detection) can proceed
    /// without waiting for the optional `ffprobe` download to complete.
    pub fn is_downloading(&self) -> bool {
        let state = self.lock_state();
        state.status == "downloading" && !state.ffmpeg_available
    }

    /// Transition status to `"downloading"` with 0% progress and the given message.
    pub fn mark_downloading(&self, message: impl Into<String>) {
        let mut state = self.lock_state();
        state.status = "downloading".to_string();
        state.progress_pct = Some(0);
        state.message = message.into();
    }

    fn download_managed_tool(
        &self,
        tool: &str,
        asset: &str,
        base_url: &str,
        dir: &Path,
        os: &str,
        timeout_sec: f64,
    ) -> Result<PathBuf, String> {
        let url = format!("{base_url}/{asset}");
        println!(
            "[ffmpeg] Downloading {tool} ({asset}) into {}...",
            dir.display()
        );
        download_tool_binary(
            tool,
            &url,
            dir,
            os,
            DEFAULT_MIN_BINARY_BYTES,
            timeout_sec,
            |pct| {
                let mut state = self.lock_state();
                state.progress_pct = Some(pct);
            },
        )
    }

    /// Ensure `ffmpeg` (and `ffprobe`) are installed, downloading missing binaries into
    /// `target_dir` (or this manager's directory) from `base_url` / `NOTFLIX_FFMPEG_BASE_URL`.
    pub fn ensure_binaries(
        &self,
        target_dir: Option<&Path>,
        base_url: Option<&str>,
        timeout_sec: f64,
    ) -> FfmpegStatus {
        let dir = target_dir
            .map(PathBuf::from)
            .unwrap_or_else(|| self.effective_dir());
        cleanup_stale_partial_downloads(&dir);

        let mut has_ffmpeg = resolve_ffmpeg_in(&dir).is_some();
        let mut has_ffprobe = resolve_ffprobe_in(&dir).is_some();

        if has_ffmpeg && has_ffprobe {
            let mut state = self.lock_state();
            state.status = "ready".to_string();
            state.progress_pct = Some(100);
            state.message = ready_message(true);
            state.ffmpeg_available = true;
            state.ffprobe_available = true;
            return state.clone();
        }

        let effective_base = resolve_base_url(base_url);
        let os = std::env::consts::OS;
        let arch = std::env::consts::ARCH;
        let (Some(ffmpeg_asset), Some(ffprobe_asset)) = (
            asset_name_for("ffmpeg", os, arch),
            asset_name_for("ffprobe", os, arch),
        ) else {
            let mut state = self.lock_state();
            state.status = if has_ffmpeg {
                "ready".to_string()
            } else {
                "unavailable".to_string()
            };
            state.progress_pct = if has_ffmpeg { Some(100) } else { None };
            state.message = format!("Automatic ffmpeg download is not supported on {os}-{arch}.");
            state.ffmpeg_available = has_ffmpeg;
            state.ffprobe_available = has_ffprobe;
            return state.clone();
        };

        {
            let mut state = self.lock_state();
            state.status = "downloading".to_string();
            state.progress_pct = Some(0);
            state.ffmpeg_available = has_ffmpeg;
            state.ffprobe_available = has_ffprobe;
            state.message = if !has_ffmpeg {
                "Downloading ffmpeg...".to_string()
            } else {
                "Downloading ffprobe...".to_string()
            };
        }

        if !has_ffmpeg {
            match self.download_managed_tool(
                "ffmpeg",
                &ffmpeg_asset,
                &effective_base,
                &dir,
                os,
                timeout_sec,
            ) {
                Ok(path) => {
                    has_ffmpeg = true;
                    {
                        let mut state = self.lock_state();
                        state.ffmpeg_available = true;
                        state.progress_pct = Some(100);
                        state.message = "Installed ffmpeg.".to_string();
                    }
                    println!("[ffmpeg] Installed ffmpeg to {}", path.display());
                }
                Err(err) => {
                    println!(
                        "[ffmpeg] Auto-download skipped ({err}); falling back to chapter/visual detection."
                    );
                    let mut state = self.lock_state();
                    state.status = "unavailable".to_string();
                    state.progress_pct = None;
                    state.message = format!("ffmpeg auto-download skipped: {err}");
                    state.ffmpeg_available = false;
                    state.ffprobe_available = has_ffprobe;
                    return state.clone();
                }
            }
        }

        if !has_ffprobe {
            {
                let mut state = self.lock_state();
                state.progress_pct = Some(0);
                state.message = "Downloading ffprobe...".to_string();
            }
            match self.download_managed_tool(
                "ffprobe",
                &ffprobe_asset,
                &effective_base,
                &dir,
                os,
                timeout_sec,
            ) {
                Ok(path) => {
                    has_ffprobe = true;
                    println!("[ffmpeg] Installed ffprobe to {}", path.display());
                }
                Err(err) => {
                    println!("[ffmpeg] Optional ffprobe download failed ({err}).");
                }
            }
        }

        let mut state = self.lock_state();
        state.status = if has_ffmpeg {
            "ready".to_string()
        } else {
            "unavailable".to_string()
        };
        state.progress_pct = if has_ffmpeg { Some(100) } else { None };
        state.message = if has_ffmpeg {
            ready_message(has_ffprobe)
        } else {
            "ffmpeg is unavailable.".to_string()
        };
        state.ffmpeg_available = has_ffmpeg;
        state.ffprobe_available = has_ffprobe;
        state.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_ffmpeg_env_parsing_respects_cli_and_falsey_values() {
        assert!(!auto_ffmpeg_enabled_from_env(true, None));
        assert!(!auto_ffmpeg_enabled_from_env(true, Some("1")));
        assert!(auto_ffmpeg_enabled_from_env(false, None));
        assert!(auto_ffmpeg_enabled_from_env(false, Some("1")));
        assert!(auto_ffmpeg_enabled_from_env(false, Some("true")));
        for disabled in ["0", "false", "FALSE", " no ", "Off"] {
            assert!(!auto_ffmpeg_enabled_from_env(false, Some(disabled)));
        }
    }

    #[test]
    fn manager_is_downloading_releases_once_ffmpeg_is_available() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let manager = FfmpegManager::with_dir(tmp.path().to_path_buf());
        {
            let mut st = manager.lock_state();
            st.status = "downloading".to_string();
            st.ffmpeg_available = false;
            st.ffprobe_available = false;
        }
        assert!(manager.is_downloading());

        {
            let mut st = manager.lock_state();
            st.ffmpeg_available = true;
        }
        assert!(!manager.is_downloading());
    }
}
