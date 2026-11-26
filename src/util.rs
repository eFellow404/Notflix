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

/// Result of running an external command with a wall-clock timeout.
pub struct CommandOutcome {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
    pub spawn_error: Option<String>,
}

impl CommandOutcome {
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
        Err(err) => {
            return CommandOutcome {
                code: -1,
                stdout: Vec::new(),
                stderr: Vec::new(),
                timed_out: false,
                spawn_error: Some(err.to_string()),
            };
        }
    };

    let stdout_handle = child.stdout.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            buf
        })
    });
    let stderr_handle = child.stderr.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            buf
        })
    });

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let mut wait_err: Option<String> = None;

    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code().unwrap_or(-1),
            Ok(None) => {
                let now = Instant::now();
                if now >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break -1;
                }
                let remaining = deadline.saturating_duration_since(now);
                std::thread::sleep(remaining.min(Duration::from_millis(20)));
            }
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                wait_err = Some(err.to_string());
                break -1;
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
        timed_out,
        spawn_error: wait_err,
    }
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
        let p = lexical_normalize(Path::new("/a/b/../c/./d"));
        assert_eq!(p, PathBuf::from("/a/c/d"));
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
}
