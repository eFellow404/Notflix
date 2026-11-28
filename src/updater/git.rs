//! Offline-safe Git repository inspection, fetching, and fast-forward updating.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::util::{canonicalize_clean, expand_tilde, run_command_with_timeout, utc_now};

use super::backup::run_pre_update_backup;
use super::{short_sha, UpdateResult, GIT_LOCAL_TIMEOUT_SEC, GIT_MERGE_MIN_TIMEOUT_SEC};

const NETWORK_ERROR_MARKERS: &[&str] = &[
    "could not resolve",
    "unable to access",
    "failed to connect",
    "couldn't connect",
    "could not connect",
    "unable to connect",
    "connection timed out",
    "connection refused",
    "connection was reset",
    "recv failure",
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

pub(super) struct RepoTarget {
    pub(super) git_bin: PathBuf,
    pub(super) branch: String,
    pub(super) remote: String,
    pub(super) remote_branch: String,
    pub(super) local_sha: Option<String>,
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

fn detect_git_remote(repo: &GitRepo<'_>, branch: &str) -> Option<(String, String)> {
    let upstream = repo.run(
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
        GIT_LOCAL_TIMEOUT_SEC,
    );
    if upstream.code == 0 {
        if let Some((rem, rem_br)) = upstream.stdout.split_once('/') {
            return Some((rem.to_string(), rem_br.to_string()));
        }
    }

    let remotes_out = repo.run(&["remote"], GIT_LOCAL_TIMEOUT_SEC);
    if remotes_out.code == 0 {
        let remotes: Vec<&str> = remotes_out
            .stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        if remotes.contains(&"origin") {
            return Some(("origin".to_string(), branch.to_string()));
        }
        if let Some(&first) = remotes.first() {
            return Some((first.to_string(), branch.to_string()));
        }
    }

    None
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

    let check_wt = repo.run(
        &["rev-parse", "--is-inside-work-tree"],
        GIT_LOCAL_TIMEOUT_SEC,
    );
    if check_wt.code != 0 || !check_wt.stdout.eq_ignore_ascii_case("true") {
        return Err(Box::new(UpdateResult::base(
            "not_a_repo",
            "Not a Git installation; automatic updates unavailable.",
            now,
        )));
    }

    let head = repo.run(&["rev-parse", "HEAD"], GIT_LOCAL_TIMEOUT_SEC);
    let local_sha = if head.code == 0 && !head.stdout.is_empty() {
        Some(head.stdout)
    } else {
        None
    };

    let br = repo.run(&["symbolic-ref", "--short", "HEAD"], GIT_LOCAL_TIMEOUT_SEC);
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

    let Some((remote, remote_branch)) = detect_git_remote(&repo, &branch) else {
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

pub(super) fn resolve_repo_root(repo_dir: Option<&Path>) -> PathBuf {
    let dir = repo_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(default_repo_dir);
    let expanded = expand_tilde(&dir);
    canonicalize_clean(&expanded).unwrap_or(expanded)
}

fn resolve_remote_head_and_counts(
    repo: &GitRepo<'_>,
    target: &RepoTarget,
) -> Option<(String, usize, usize)> {
    let mut remote_ref = format!("refs/remotes/{}/{}", target.remote, target.remote_branch);
    let mut rsha = repo.run(&["rev-parse", &remote_ref], GIT_LOCAL_TIMEOUT_SEC);
    if rsha.code != 0 || rsha.stdout.is_empty() {
        rsha = repo.run(&["rev-parse", "FETCH_HEAD"], GIT_LOCAL_TIMEOUT_SEC);
        remote_ref = "FETCH_HEAD".to_string();
    }
    if rsha.code != 0 || rsha.stdout.is_empty() {
        return None;
    }
    let remote_sha = rsha.stdout;

    let rev_range = format!("HEAD...{remote_ref}");
    let cnt = repo.run(
        &["rev-list", "--left-right", "--count", &rev_range],
        GIT_LOCAL_TIMEOUT_SEC,
    );
    let (mut ahead, mut behind) = (0usize, 0usize);
    if cnt.code == 0 && !cnt.stdout.is_empty() {
        let mut parts = cnt.stdout.split_whitespace();
        if let (Some(a), Some(b)) = (parts.next(), parts.next()) {
            ahead = a.parse().unwrap_or(0);
            behind = b.parse().unwrap_or(0);
        }
    }

    Some((remote_sha, ahead, behind))
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

    let Some((remote_sha, ahead, behind)) = resolve_remote_head_and_counts(&repo, &target) else {
        return UpdateResult::from_target(
            "error",
            "Could not determine remote commit.",
            now,
            &target,
        );
    };

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
    let out = repo.run(
        &["status", "--porcelain", "--untracked-files=no"],
        GIT_LOCAL_TIMEOUT_SEC,
    );
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

    let backup_str = match run_pre_update_backup(db_path, "update", &check) {
        Ok(path) => path,
        Err(res) => return *res,
    };

    let target_ref = check.remote_sha.as_deref().unwrap_or("FETCH_HEAD");
    let merge = repo.run(
        &["merge", "--ff-only", target_ref],
        timeout_sec.max(GIT_MERGE_MIN_TIMEOUT_SEC),
    );
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

    let new_head = repo.run(&["rev-parse", "HEAD"], GIT_LOCAL_TIMEOUT_SEC);
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
