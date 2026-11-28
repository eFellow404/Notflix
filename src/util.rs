//! Shared cross-platform path, numeric, timestamp, and subprocess helpers.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use chrono::Utc;

/// Return the current UTC timestamp formatted as RFC 3339.
pub fn utc_now() -> String {
    Utc::now().to_rfc3339()
}

/// Parse an optional opt-out boolean flag value.
///
/// Returns `true` unless the trimmed, ASCII-lowercased value is `"0"`, `"false"`, `"no"`, or `"off"`.
pub fn parse_env_flag_enabled(val: Option<&str>) -> bool {
    !matches!(
        val.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("0" | "false" | "no" | "off")
    )
}

/// Return whether an opt-out environment flag `var` is enabled (defaults to `true` when unset).
pub fn env_flag_enabled(var: &str) -> bool {
    let val = std::env::var(var).ok();
    parse_env_flag_enabled(val.as_deref())
}

/// Return the trimmed value of environment variable `name`, or `None` if unset or blank.
pub fn env_var_trimmed(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|raw| raw.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Convert a timeout in seconds into a [`Duration`], clamping finite values to `[min, max]`
/// and falling back to `default` for non-finite values.
pub fn clamp_timeout_secs(raw: f64, min: f64, max: f64, default: f64) -> Duration {
    let secs = if raw.is_finite() {
        raw.clamp(min, max)
    } else {
        default
    };
    Duration::from_secs_f64(secs)
}

/// Build a configured `ureq::Agent` with explicit connect and read timeouts.
pub fn build_http_agent(connect_timeout: Duration, read_timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(connect_timeout)
        .timeout_read(read_timeout)
        .build()
}

/// Return the standard Notflix HTTP `User-Agent` header value.
pub fn http_user_agent() -> String {
    format!("notflix/{}", crate::VERSION)
}

/// Validate executable header magic bytes for the specified target OS (`"linux"`, `"windows"`, `"macos"`).
pub(crate) fn validate_executable_magic_for_os(bytes: &[u8], os: &str) -> bool {
    match os {
        "linux" => bytes.starts_with(b"\x7fELF"),
        "windows" => bytes.starts_with(b"MZ"),
        "macos" => {
            bytes.len() >= 4
                && matches!(
                    &bytes[..4],
                    [0xfe, 0xed, 0xfa, 0xce]
                        | [0xce, 0xfa, 0xed, 0xfe]
                        | [0xfe, 0xed, 0xfa, 0xcf]
                        | [0xcf, 0xfa, 0xed, 0xfe]
                        | [0xca, 0xfe, 0xba, 0xbe]
                        | [0xbe, 0xba, 0xfe, 0xca]
                )
        }
        _ => false,
    }
}

/// Set `0o755` executable permissions on `path` on Unix platforms.
#[cfg(unix)]
pub(crate) fn set_executable_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

/// Clamp a floating-point value to `>= 0.0`, returning `0.0` for NaN/Inf.
#[inline]
pub fn finite_non_negative(value: f64) -> f64 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

/// Clamp a floating-point confidence/ratio to `0.0..=1.0`, returning `0.0` for NaN/Inf.
#[inline]
pub fn finite_unit(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// Strip Windows extended-length verbatim prefixes (`\\?\` / `\\?\UNC\`) returned by
/// `std::fs::canonicalize` so paths display and compare cleanly across platforms.
pub fn strip_windows_verbatim_prefix(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(s) = path.to_str() {
            if let Some(unc) = s.strip_prefix(r"\\?\UNC\") {
                return PathBuf::from(format!(r"\\{unc}"));
            }
            if let Some(stripped) = s.strip_prefix(r"\\?\") {
                return PathBuf::from(stripped);
            }
        }
    }
    path
}

/// Expand a leading `~`, `~/`, or `~\` path component using the user's home directory.
pub fn expand_tilde(path: &Path) -> PathBuf {
    if let Some(s) = path.to_str() {
        return expand_tilde_str(s);
    }
    path.to_path_buf()
}

/// Expand a leading `~`, `~/`, or `~\` string into a [`PathBuf`].
pub fn expand_tilde_str(raw: &str) -> PathBuf {
    if raw == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(raw));
    }
    if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(raw)
}

/// Canonicalize an existing path and strip any Windows verbatim prefix.
pub fn canonicalize_clean(path: &Path) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(path).map(strip_windows_verbatim_prefix)
}

/// Lexically collapse `.` and `..` components without hitting the filesystem.
pub fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(comp.as_os_str());
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Resolve a path (existing or not yet created): expands `~`, canonicalizes the path
/// or its deepest existing ancestor, and lexically normalizes remaining segments.
pub fn resolve_path(path: &Path) -> PathBuf {
    let expanded = expand_tilde(path);
    if let Ok(canon) = canonicalize_clean(&expanded) {
        return canon;
    }

    let abs = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&expanded))
            .unwrap_or(expanded)
    };

    let mut missing_tail: Vec<OsString> = Vec::new();
    let mut probe = abs.as_path();
    while !probe.exists() {
        let Some(name) = probe.file_name() else {
            break;
        };
        missing_tail.push(name.to_os_string());
        match probe.parent() {
            Some(parent) => probe = parent,
            None => break,
        }
    }

    if probe.exists() {
        if let Ok(mut base) = canonicalize_clean(probe) {
            for part in missing_tail.into_iter().rev() {
                base.push(part);
            }
            return lexical_normalize(&base);
        }
    }

    lexical_normalize(&abs)
}

const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Result of running an external command with a wall-clock timeout.
pub struct CommandOutcome {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
    pub spawn_error: Option<String>,
}

impl CommandOutcome {
    fn spawn_err(err: impl Into<String>) -> Self {
        Self {
            code: -1,
            stdout: Vec::new(),
            stderr: Vec::new(),
            timed_out: false,
            spawn_error: Some(err.into()),
        }
    }

    fn timed_out() -> Self {
        Self {
            code: -1,
            stdout: Vec::new(),
            stderr: Vec::new(),
            timed_out: true,
            spawn_error: None,
        }
    }

    pub fn success(&self) -> bool {
        !self.timed_out && self.spawn_error.is_none() && self.code == 0
    }

    pub fn stdout_lossy_trimmed(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_string()
    }

    pub fn stderr_lossy_trimmed(&self) -> String {
        if let Some(ref err) = self.spawn_error {
            return err.clone();
        }
        String::from_utf8_lossy(&self.stderr).trim().to_string()
    }
}

fn spawn_pipe_reader<R: Read + Send + 'static>(
    pipe: Option<R>,
) -> Option<std::thread::JoinHandle<Vec<u8>>> {
    pipe.map(|mut reader| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = reader.read_to_end(&mut buf);
            buf
        })
    })
}

/// Spawn `cmd`, drain stdout (and optionally stderr) on background threads to avoid OS pipe
/// buffer deadlocks, and terminate the child cleanly if `timeout` elapses.
pub fn run_command_with_timeout(
    cmd: &mut Command,
    timeout: Duration,
    capture_stderr: bool,
) -> CommandOutcome {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(if capture_stderr {
            Stdio::piped()
        } else {
            Stdio::null()
        });

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(err) => return CommandOutcome::spawn_err(err.to_string()),
    };

    let stdout_handle = spawn_pipe_reader(child.stdout.take());
    let stderr_handle = spawn_pipe_reader(child.stderr.take());

    let deadline = Instant::now() + timeout;
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code().unwrap_or(-1),
            Ok(None) => {
                let now = Instant::now();
                if now >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    // Return immediately without joining pipe reader threads: grandchild
                    // processes (e.g. `git-remote-http`) may still hold inherited pipe FDs.
                    return CommandOutcome::timed_out();
                }
                let remaining = deadline.saturating_duration_since(now);
                std::thread::sleep(remaining.min(CHILD_POLL_INTERVAL));
            }
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                return CommandOutcome::spawn_err(err.to_string());
            }
        }
    };

    let stdout = stdout_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    let stderr = stderr_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();

    CommandOutcome {
        code,
        stdout,
        stderr,
        timed_out: false,
        spawn_error: None,
    }
}

/// Best-effort launch of `url` in the system's default web browser (non-blocking, silent).
pub fn open_browser(url: &str) {
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    };

    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = Command::new("open");
        c.arg(url);
        c
    };

    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };

    let _ = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finite_clamps_handle_nan_inf_and_negatives() {
        assert_eq!(finite_non_negative(-5.0), 0.0);
        assert_eq!(finite_non_negative(f64::NAN), 0.0);
        assert_eq!(finite_non_negative(f64::INFINITY), 0.0);
        assert_eq!(finite_non_negative(12.5), 12.5);

        assert_eq!(finite_unit(-0.5), 0.0);
        assert_eq!(finite_unit(2.5), 1.0);
        assert_eq!(finite_unit(f64::NAN), 0.0);
        assert_eq!(finite_unit(0.75), 0.75);
    }

    #[test]
    fn lexical_normalize_collapses_dot_and_dotdot() {
        let p = lexical_normalize(Path::new("a/b/../c/./d"));
        assert_eq!(p, PathBuf::from("a").join("c").join("d"));
    }

    #[test]
    fn expand_tilde_preserves_plain_paths() {
        assert_eq!(
            expand_tilde(Path::new("/tmp/notflix")),
            PathBuf::from("/tmp/notflix")
        );
        if let Some(home) = dirs::home_dir() {
            assert_eq!(expand_tilde_str("~"), home);
            assert_eq!(expand_tilde_str("~/videos"), home.join("videos"));
        }
    }

    #[test]
    fn parse_env_flag_enabled_handles_default_truthy_and_falsey() {
        assert!(parse_env_flag_enabled(None));
        assert!(parse_env_flag_enabled(Some("")));
        assert!(parse_env_flag_enabled(Some("1")));
        assert!(parse_env_flag_enabled(Some("true")));
        for disabled in ["0", "false", "FALSE", " no ", "Off"] {
            assert!(!parse_env_flag_enabled(Some(disabled)));
        }
    }

    #[test]
    fn clamp_timeout_secs_clamps_finite_and_falls_back_on_non_finite() {
        assert_eq!(
            clamp_timeout_secs(1.0, 5.0, 60.0, 30.0),
            Duration::from_secs_f64(5.0)
        );
        assert_eq!(
            clamp_timeout_secs(120.0, 5.0, 60.0, 30.0),
            Duration::from_secs_f64(60.0)
        );
        assert_eq!(
            clamp_timeout_secs(20.0, 5.0, 60.0, 30.0),
            Duration::from_secs_f64(20.0)
        );
        assert_eq!(
            clamp_timeout_secs(f64::NAN, 5.0, 60.0, 30.0),
            Duration::from_secs_f64(30.0)
        );
        assert_eq!(
            clamp_timeout_secs(f64::INFINITY, 5.0, 60.0, 30.0),
            Duration::from_secs_f64(30.0)
        );
    }

    #[test]
    fn validate_executable_magic_checks_all_supported_os_headers() {
        assert!(validate_executable_magic_for_os(b"\x7fELF\x02", "linux"));
        assert!(!validate_executable_magic_for_os(b"\x7fEL", "linux"));
        assert!(validate_executable_magic_for_os(b"MZ", "windows"));
        assert!(!validate_executable_magic_for_os(b"M", "windows"));
        for magic in [
            [0xfe, 0xed, 0xfa, 0xce],
            [0xce, 0xfa, 0xed, 0xfe],
            [0xfe, 0xed, 0xfa, 0xcf],
            [0xcf, 0xfa, 0xed, 0xfe],
            [0xca, 0xfe, 0xba, 0xbe],
            [0xbe, 0xba, 0xfe, 0xca],
        ] {
            assert!(validate_executable_magic_for_os(&magic, "macos"));
        }
        assert!(!validate_executable_magic_for_os(b"\x7fELF", "unknown"));
    }
}
