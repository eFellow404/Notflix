//! Offline-safe automatic Git updater with pre-update SQLite backups.
//!
//! Checks the configured Git remote for newer commits and applies fast-forward
//! updates cleanly without ever crashing the app when offline or touching user data.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use chrono::Utc;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::util::{canonicalize_clean, expand_tilde, run_command_with_timeout, utc_now};

/// Default timeout (in seconds) for network-facing Git fetch operations.
pub const DEFAULT_FETCH_TIMEOUT_SEC: f64 = 5.0;

/// Default number of pre-update SQLite backups retained per database.
pub const DEFAULT_BACKUP_KEEP: usize = 5;

const NETWORK_ERROR_MARKERS: &[&str] = &[
    "could not resolve",
    "unable to access",
    "failed to connect",
    "connection timed out",
    "connection refused",
    "network is unreachable",
    "temporary failure in name resolution",
    "name or service not known",
    "timed out",
    "could not read from remote repository",
    "the remote end hung up",
    "ssl",
    "tls",
    "repository not found",
];

/// Structured outcome of an update status check or update attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateResult {
    pub status: String,
    pub message: String,
    pub branch: Option<String>,
    pub remote: Option<String>,
    pub local_sha: Option<String>,
    pub local_short_sha: Option<String>,
    pub remote_sha: Option<String>,
    pub remote_short_sha: Option<String>,
    pub old_sha: Option<String>,
    pub old_short_sha: Option<String>,
    pub behind_count: usize,
    pub ahead_count: usize,
    pub updated: bool,
    pub restart_required: bool,
    pub backup_path: Option<String>,
    pub checked_at: String,
    pub version: String,
}

impl UpdateResult {
    fn base(status: &str, message: impl Into<String>, checked_at: String) -> Self {
        Self {
            status: status.to_string(),
            message: message.into(),
            branch: None,
            remote: None,
            local_sha: None,
            local_short_sha: None,
            remote_sha: None,
            remote_short_sha: None,
            old_sha: None,
            old_short_sha: None,
            behind_count: 0,
            ahead_count: 0,
            updated: false,
            restart_required: false,
            backup_path: None,
            checked_at,
            version: crate::VERSION.to_string(),
        }
    }

    fn with_local_sha(mut self, sha: Option<String>) -> Self {
        self.local_short_sha = short_sha(sha.as_deref());
        self.local_sha = sha;
        self
    }

    fn with_remote_sha(mut self, sha: Option<String>) -> Self {
        self.remote_short_sha = short_sha(sha.as_deref());
        self.remote_sha = sha;
        self
    }

    fn with_old_sha(mut self, sha: Option<String>) -> Self {
        self.old_short_sha = short_sha(sha.as_deref());
        self.old_sha = sha;
        self
    }

    fn with_branch_remote(mut self, branch: Option<String>, remote: Option<String>) -> Self {
        self.branch = branch;
        self.remote = remote;
        self
    }

    fn from_target(
        status: &str,
        message: impl Into<String>,
        checked_at: String,
        target: &RepoTarget,
    ) -> Self {
        Self::base(status, message, checked_at)
            .with_local_sha(target.local_sha.clone())
            .with_branch_remote(Some(target.branch.clone()), Some(target.remote.clone()))
    }

    fn from_check(
        status: &str,
        message: impl Into<String>,
        checked_at: String,
        check: &UpdateResult,
    ) -> Self {
        let mut res = Self::base(status, message, checked_at)
            .with_local_sha(check.local_sha.clone())
            .with_remote_sha(check.remote_sha.clone())
            .with_branch_remote(check.branch.clone(), check.remote.clone());
        res.behind_count = check.behind_count;
        res.ahead_count = check.ahead_count;
        res
    }
}

struct GitOutput {
    code: i32,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

struct GitRepo<'a> {
    bin: &'a OsStr,
    root: &'a Path,
}

impl<'a> GitRepo<'a> {
    fn new(bin: &'a OsStr, root: &'a Path) -> Self {
        Self { bin, root }
    }

    fn run(&self, args: &[&str], timeout_sec: f64) -> GitOutput {
        let timeout_secs = if timeout_sec.is_finite() && timeout_sec > 0.0 {
            timeout_sec
        } else {
            0.1
        };

        let mut cmd = Command::new(self.bin);
        cmd.args(args)
            .current_dir(self.root)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "")
            .env("LC_ALL", "C");

        let outcome =
            run_command_with_timeout(&mut cmd, Duration::from_secs_f64(timeout_secs), true);
        if outcome.timed_out {
            let subcmd = args.first().copied().unwrap_or("command");
            return GitOutput {
                code: -1,
                stdout: String::new(),
                stderr: format!("git {subcmd} timed out after {timeout_secs:.1}s"),
                timed_out: true,
            };
        }

        GitOutput {
            code: outcome.code,
            stdout: outcome.stdout_lossy_trimmed(),
            stderr: outcome.stderr_lossy_trimmed(),
            timed_out: false,
        }
    }
}

struct RepoTarget {
    git_bin: PathBuf,
    branch: String,
    remote: String,
    remote_branch: String,
    local_sha: Option<String>,
}

fn short_sha(sha: Option<&str>) -> Option<String> {
    sha.filter(|s| !s.is_empty())
        .map(|s| s.chars().take(7).collect())
}

/// Resolve the default repository directory for updates.
///
/// Prefers the directory containing `.git` if either the current working directory or
/// the executable's parent/ancestors contains one; otherwise falls back to the current
/// working directory or executable parent.
pub fn default_repo_dir() -> PathBuf {
    let cwd = std::env::current_dir().ok();
    if let Some(ref dir) = cwd {
        if dir.join(".git").exists() {
            return dir.clone();
        }
    }

    let exe_parent = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));

    if let Some(ref parent) = exe_parent {
        for ancestor in parent.ancestors() {
            if ancestor.join(".git").exists() {
                return ancestor.to_path_buf();
            }
        }
    }

    cwd.or(exe_parent).unwrap_or_else(|| PathBuf::from("."))
}

fn looks_like_network_error(stderr: &str, timed_out: bool) -> bool {
    if timed_out {
        return true;
    }
    let low = stderr.to_ascii_lowercase();
    NETWORK_ERROR_MARKERS
        .iter()
        .any(|marker| low.contains(marker))
}

/// Create a consistent WAL-safe SQLite backup before applying an update.
///
/// Returns `Ok(None)` if `db_path` does not exist yet. Prunes older backups beyond `keep`.
pub fn backup_database(db_path: &Path, label: &str, keep: usize) -> Result<Option<PathBuf>> {
    let expanded = expand_tilde(db_path);
    let resolved = canonicalize_clean(&expanded).unwrap_or(expanded);
    if !resolved.is_file() {
        return Ok(None);
    }

    let backup_dir = resolved
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("backups");
    std::fs::create_dir_all(&backup_dir)?;

    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let safe_label_raw: String = label
        .chars()
        .filter(|ch| ch.is_alphanumeric() || *ch == '-' || *ch == '_')
        .take(24)
        .collect();
    let safe_label = if safe_label_raw.is_empty() {
        "snapshot"
    } else {
        safe_label_raw.as_str()
    };

    let stem = resolved
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("notflix");

    let mut dest = backup_dir.join(format!("{stem}-{safe_label}-{stamp}.db"));
    let mut suffix = 1usize;
    while dest.exists() {
        dest = backup_dir.join(format!("{stem}-{safe_label}-{stamp}-{suffix}.db"));
        suffix += 1;
    }

    let backup_result: rusqlite::Result<()> = (|| {
        let src = Connection::open(&resolved)?;
        src.busy_timeout(Duration::from_secs(10))?;
        let mut dst = Connection::open(&dest)?;
        dst.busy_timeout(Duration::from_secs(10))?;
        let backup = rusqlite::backup::Backup::new(&src, &mut dst)?;
        backup.run_to_completion(100, Duration::from_millis(10), None)?;
        Ok(())
    })();

    if let Err(err) = backup_result {
        let _ = std::fs::remove_file(&dest);
        return Err(err.into());
    }

    if keep > 0 {
        let prefix = format!("{stem}-");
        let mut existing: Vec<(SystemTime, String, PathBuf)> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&backup_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if name.starts_with(&prefix) && name.ends_with(".db") {
                    let mtime = entry
                        .metadata()
                        .and_then(|m| m.modified())
                        .unwrap_or(SystemTime::UNIX_EPOCH);
                    existing.push((mtime, name.to_string(), path));
                }
            }
        }
        existing.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
        for (_, _, stale) in existing.into_iter().skip(keep) {
            let _ = std::fs::remove_file(stale);
        }
    }

    Ok(Some(dest))
}

fn resolve_repo_target(
    repo_dir: &Path,
    git_bin: Option<PathBuf>,
) -> std::result::Result<RepoTarget, Box<UpdateResult>> {
    let now = utc_now();
    let Some(git_bin) = git_bin else {
        return Err(Box::new(UpdateResult::base(
            "no_git",
            "Git is not installed; automatic updates unavailable.",
            now,
        )));
    };

    if !repo_dir.is_dir() {
        return Err(Box::new(UpdateResult::base(
            "not_a_repo",
            "Application folder not found.",
            now,
        )));
    }

    let repo = GitRepo::new(git_bin.as_os_str(), repo_dir);

    let check_wt = repo.run(&["rev-parse", "--is-inside-work-tree"], 3.0);
    if check_wt.code != 0 || !check_wt.stdout.eq_ignore_ascii_case("true") {
        return Err(Box::new(UpdateResult::base(
            "not_a_repo",
            "Not a Git installation; automatic updates unavailable.",
            now,
        )));
    }

    let head = repo.run(&["rev-parse", "HEAD"], 3.0);
    let local_sha = if head.code == 0 && !head.stdout.is_empty() {
        Some(head.stdout)
    } else {
        None
    };

    let br = repo.run(&["symbolic-ref", "--short", "HEAD"], 3.0);
    if br.code != 0 || br.stdout.is_empty() {
        return Err(Box::new(
            UpdateResult::base(
                "error",
                "Detached HEAD; switch to a branch to enable automatic updates.",
                now,
            )
            .with_local_sha(local_sha),
        ));
    }
    let branch = br.stdout;

    let mut remote: Option<String> = None;
    let mut remote_branch = branch.clone();

    let upstream = repo.run(
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
        3.0,
    );
    if upstream.code == 0 {
        if let Some((rem, rem_br)) = upstream.stdout.split_once('/') {
            remote = Some(rem.to_string());
            remote_branch = rem_br.to_string();
        }
    }

    if remote.is_none() {
        let remotes_out = repo.run(&["remote"], 3.0);
        if remotes_out.code == 0 {
            let remotes: Vec<&str> = remotes_out
                .stdout
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect();
            if remotes.contains(&"origin") {
                remote = Some("origin".to_string());
            } else if let Some(&first) = remotes.first() {
                remote = Some(first.to_string());
            }
        }
    }

    let Some(remote) = remote else {
        return Err(Box::new(
            UpdateResult::base("no_remote", "No Git remote configured for updates.", now)
                .with_local_sha(local_sha)
                .with_branch_remote(Some(branch), None),
        ));
    };

    Ok(RepoTarget {
        git_bin,
        branch,
        remote,
        remote_branch,
        local_sha,
    })
}

fn resolve_repo_root(repo_dir: Option<&Path>) -> PathBuf {
    let dir = repo_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(default_repo_dir);
    let expanded = expand_tilde(&dir);
    canonicalize_clean(&expanded).unwrap_or(expanded)
}

fn check_for_updates_with_git(
    repo_dir: Option<&Path>,
    timeout_sec: f64,
    git_bin: Option<PathBuf>,
) -> UpdateResult {
    let now = utc_now();
    let root = resolve_repo_root(repo_dir);
    let target = match resolve_repo_target(&root, git_bin) {
        Ok(t) => t,
        Err(res) => return *res,
    };

    let repo = GitRepo::new(target.git_bin.as_os_str(), &root);

    let fetch = repo.run(
        &["fetch", "--quiet", &target.remote, &target.remote_branch],
        timeout_sec,
    );
    if fetch.code != 0 {
        if looks_like_network_error(&fetch.stderr, fetch.timed_out) {
            return UpdateResult::from_target(
                "offline",
                "Could not reach update server; continuing offline.",
                now,
                &target,
            );
        }
        let detail = if fetch.stderr.is_empty() {
            "git fetch returned an error"
        } else {
            fetch.stderr.as_str()
        };
        return UpdateResult::from_target(
            "error",
            format!("Update check failed: {detail}"),
            now,
            &target,
        );
    }

    let mut remote_ref = format!("refs/remotes/{}/{}", target.remote, target.remote_branch);
    let mut rsha = repo.run(&["rev-parse", &remote_ref], 3.0);
    if rsha.code != 0 || rsha.stdout.is_empty() {
        rsha = repo.run(&["rev-parse", "FETCH_HEAD"], 3.0);
        remote_ref = "FETCH_HEAD".to_string();
    }
    if rsha.code != 0 || rsha.stdout.is_empty() {
        return UpdateResult::from_target(
            "error",
            "Could not determine remote commit.",
            now,
            &target,
        );
    }
    let remote_sha = rsha.stdout;

    let rev_range = format!("HEAD...{remote_ref}");
    let cnt = repo.run(&["rev-list", "--left-right", "--count", &rev_range], 3.0);
    let (mut ahead, mut behind) = (0usize, 0usize);
    if cnt.code == 0 && !cnt.stdout.is_empty() {
        let mut parts = cnt.stdout.split_whitespace();
        if let (Some(a), Some(b)) = (parts.next(), parts.next()) {
            ahead = a.parse().unwrap_or(0);
            behind = b.parse().unwrap_or(0);
        }
    }

    if behind == 0 {
        let short = short_sha(target.local_sha.as_deref()).unwrap_or_else(|| "unknown".to_string());
        let mut res = UpdateResult::from_target(
            "up_to_date",
            format!("Notflix is up to date ({short})."),
            now,
            &target,
        )
        .with_remote_sha(Some(remote_sha));
        res.ahead_count = ahead;
        return res;
    }

    if ahead > 0 {
        let mut res = UpdateResult::from_target(
            "diverged",
            format!(
                "Local branch has {ahead} unpushed commit(s) and is {behind} behind {}/{}; skipping automatic merge.",
                target.remote, target.remote_branch
            ),
            now,
            &target,
        )
        .with_remote_sha(Some(remote_sha));
        res.behind_count = behind;
        res.ahead_count = ahead;
        return res;
    }

    let noun = if behind == 1 { "commit" } else { "commits" };
    let mut res = UpdateResult::from_target(
        "update_available",
        format!(
            "Update available: {behind} new {noun} on {}/{}.",
            target.remote, target.remote_branch
        ),
        now,
        &target,
    )
    .with_remote_sha(Some(remote_sha));
    res.behind_count = behind;
    res
}

/// Return local commit and branch info without making any network requests.
pub fn get_local_repo_info(repo_dir: Option<&Path>) -> UpdateResult {
    let root = resolve_repo_root(repo_dir);
    let target = match resolve_repo_target(&root, which::which("git").ok()) {
        Ok(t) => t,
        Err(res) => return *res,
    };

    let short = short_sha(target.local_sha.as_deref()).unwrap_or_else(|| "unknown".to_string());
    UpdateResult::from_target(
        "idle",
        format!("Running {} ({short})", target.branch),
        utc_now(),
        &target,
    )
}

/// Fetch remote branch metadata and report whether the local repository is behind.
///
/// Guaranteed never to panic even when offline, timed out, or misconfigured.
pub fn check_for_updates(repo_dir: Option<&Path>, timeout_sec: f64) -> UpdateResult {
    check_for_updates_with_git(repo_dir, timeout_sec, which::which("git").ok())
}

fn has_dirty_tracked_files(repo: &GitRepo<'_>) -> bool {
    let out = repo.run(&["status", "--porcelain", "--untracked-files=no"], 3.0);
    out.code != 0 || !out.stdout.is_empty()
}

/// Safely back up user data and fast-forward the repository if behind remote.
///
/// Never overwrites uncommitted tracked edits, never touches untracked user files,
/// and never fails destructively if the remote is unreachable.
pub fn apply_update(
    repo_dir: Option<&Path>,
    db_path: Option<&Path>,
    timeout_sec: f64,
) -> UpdateResult {
    let now = utc_now();
    let root = resolve_repo_root(repo_dir);
    let git_bin = which::which("git").ok();
    let check = check_for_updates_with_git(Some(&root), timeout_sec, git_bin.clone());
    if check.status != "update_available" {
        return check;
    }

    let Some(git_bin) = git_bin else {
        return UpdateResult::base(
            "no_git",
            "Git is not installed; automatic updates unavailable.",
            now,
        );
    };

    let repo = GitRepo::new(git_bin.as_os_str(), &root);

    if has_dirty_tracked_files(&repo) {
        return UpdateResult::from_check(
            "dirty",
            "Local uncommitted changes detected; skipping automatic update to protect your edits.",
            now,
            &check,
        );
    }

    let mut backup_str: Option<String> = None;
    if let Some(db) = db_path {
        let label = format!(
            "pre-{}",
            check.local_short_sha.as_deref().unwrap_or("update")
        );
        match backup_database(db, &label, DEFAULT_BACKUP_KEEP) {
            Ok(Some(path)) => backup_str = Some(path.to_string_lossy().into_owned()),
            Ok(None) => {}
            Err(err) => {
                return UpdateResult::from_check(
                    "error",
                    format!("Aborted update because database backup failed: {err}"),
                    now,
                    &check,
                );
            }
        }
    }

    let target_ref = check.remote_sha.as_deref().unwrap_or("FETCH_HEAD");
    let merge = repo.run(&["merge", "--ff-only", target_ref], timeout_sec.max(10.0));
    if merge.code != 0 {
        let detail = if merge.stderr.is_empty() {
            "git merge failed"
        } else {
            merge.stderr.as_str()
        };
        let mut res = UpdateResult::from_check(
            "error",
            format!("Fast-forward update failed safely: {detail}"),
            now,
            &check,
        );
        res.backup_path = backup_str;
        return res;
    }

    let new_head = repo.run(&["rev-parse", "HEAD"], 3.0);
    let final_sha = if new_head.code == 0 && !new_head.stdout.is_empty() {
        Some(new_head.stdout)
    } else {
        check.remote_sha.clone()
    };

    let from_short = check.local_short_sha.as_deref().unwrap_or("unknown");
    let to_short = short_sha(final_sha.as_deref()).unwrap_or_else(|| "unknown".to_string());
    let plural = if check.behind_count == 1 { "" } else { "s" };

    let mut res = UpdateResult::base(
        "updated",
        format!(
            "Updated Notflix from {from_short} to {to_short} ({} commit{plural}).",
            check.behind_count
        ),
        now,
    )
    .with_local_sha(final_sha)
    .with_remote_sha(check.remote_sha)
    .with_old_sha(check.local_sha)
    .with_branch_remote(check.branch, check.remote);
    res.updated = true;
    res.restart_required = true;
    res.backup_path = backup_str;
    res
}

/// Fail-safe startup entry point: check and apply fast-forward updates.
pub fn auto_update_on_startup(
    repo_dir: Option<&Path>,
    db_path: Option<&Path>,
    timeout_sec: f64,
) -> UpdateResult {
    apply_update(repo_dir, db_path, timeout_sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_git_binary_reports_no_git() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let res = check_for_updates_with_git(Some(tmp.path()), 1.0, None);
        assert_eq!(res.status, "no_git");
        assert!(!res.updated);
    }
}
