//! Offline-safe automatic Git and GitHub Releases updater with pre-update SQLite backups.
//!
//! Checks the configured Git remote (for source installs) or GitHub Releases (for prebuilt
//! standalone binaries) for newer builds and applies updates cleanly without ever crashing
//! when offline or touching user data.

use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use chrono::Utc;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::util::{canonicalize_clean, expand_tilde, run_command_with_timeout, utc_now};

/// Default timeout (in seconds) for network-facing update operations.
pub const DEFAULT_FETCH_TIMEOUT_SEC: f64 = 5.0;

/// Default number of pre-update SQLite backups retained per database.
pub const DEFAULT_BACKUP_KEEP: usize = 5;

/// Default GitHub repository (`owner/repo`) queried for prebuilt release binaries.
pub const DEFAULT_RELEASE_REPO: &str = "eFellow404/Notflix";

/// Maximum allowed download size for a release binary (128 MiB).
const MAX_RELEASE_BINARY_BYTES: u64 = 128 * 1024 * 1024;

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

/// Return the configured GitHub repository slug (`owner/repo`) for release updates.
///
/// Reads `NOTFLIX_RELEASE_REPO` when set and non-empty; otherwise defaults to
/// [`DEFAULT_RELEASE_REPO`].
pub fn release_repo_slug() -> String {
    std::env::var("NOTFLIX_RELEASE_REPO")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_RELEASE_REPO.to_string())
}

/// Return the expected GitHub Release binary asset filename for the current OS and CPU arch.
pub fn release_asset_name() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("notflix-linux-x86_64"),
        ("macos", "aarch64") => Some("notflix-macos-aarch64"),
        ("macos", "x86_64") => Some("notflix-macos-x86_64"),
        ("windows", "x86_64") => Some("notflix-windows-x86_64.exe"),
        _ => None,
    }
}

/// Validate executable header magic bytes for the specified target OS (`"linux"`, `"windows"`, `"macos"`).
pub fn validate_binary_magic_for_os(bytes: &[u8], os: &str) -> bool {
    if bytes.len() < 4 {
        return false;
    }
    match os {
        "linux" => bytes.starts_with(b"\x7fELF"),
        "windows" => bytes.starts_with(b"MZ"),
        "macos" => matches!(
            &bytes[..4],
            [0xfe, 0xed, 0xfa, 0xce]
                | [0xce, 0xfa, 0xed, 0xfe]
                | [0xfe, 0xed, 0xfa, 0xcf]
                | [0xcf, 0xfa, 0xed, 0xfe]
                | [0xca, 0xfe, 0xba, 0xbe]
                | [0xbe, 0xba, 0xfe, 0xca]
        ),
        _ => false,
    }
}

/// Validate executable header magic bytes for the current operating system.
pub fn validate_binary_magic(bytes: &[u8]) -> bool {
    validate_binary_magic_for_os(bytes, std::env::consts::OS)
}

fn append_file_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(suffix);
    PathBuf::from(os)
}

/// Best-effort removal of a stale `<exe>.old` binary left behind by a previous Windows self-update.
///
/// Never fails or panics.
pub fn cleanup_old_binary_artifacts() {
    if let Ok(exe) = std::env::current_exe() {
        let old_path = append_file_suffix(&exe, ".old");
        if old_path.exists() {
            let _ = std::fs::remove_file(old_path);
        }
    }
}

fn is_hex_sha(value: &str) -> bool {
    (7..=40).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn extract_sha_from_tag(tag_name: &str) -> Option<&str> {
    let trimmed = tag_name.trim();
    let candidate = trimmed.rsplit('-').next()?;
    if is_hex_sha(candidate) {
        Some(candidate)
    } else {
        None
    }
}

fn sha_prefix_matches(a: &str, b: &str) -> bool {
    if a.len() < 7 || b.len() < 7 {
        return false;
    }
    let a_low = a.to_ascii_lowercase();
    let b_low = b.to_ascii_lowercase();
    a_low.starts_with(&b_low) || b_low.starts_with(&a_low)
}

/// Return whether a GitHub Release tag or commitish matches the currently running build SHA.
pub fn release_matches_build(tag_name: &str, target_commitish: &str, current_sha: &str) -> bool {
    let sha = current_sha.trim();
    if sha.is_empty() || sha.eq_ignore_ascii_case("dev") {
        return false;
    }

    let tag = tag_name.trim();
    if tag.eq_ignore_ascii_case(sha) {
        return true;
    }

    let sha_low = sha.to_ascii_lowercase();
    let tag_low = tag.to_ascii_lowercase();
    if tag_low.ends_with(&format!("-{sha_low}")) {
        return true;
    }

    if let Some(tag_sha) = extract_sha_from_tag(tag) {
        if is_hex_sha(sha) && sha_prefix_matches(tag_sha, sha) {
            return true;
        }
    }

    let commitish = target_commitish.trim();
    is_hex_sha(commitish) && commitish.to_ascii_lowercase().starts_with(&sha_low)
}

fn clamp_http_timeout(timeout_sec: f64) -> Duration {
    let secs = if timeout_sec.is_finite() {
        timeout_sec.clamp(1.0, 60.0)
    } else {
        DEFAULT_FETCH_TIMEOUT_SEC
    };
    Duration::from_secs_f64(secs)
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    #[serde(default)]
    tag_name: Option<String>,
    #[serde(default)]
    target_commitish: Option<String>,
    #[serde(default)]
    assets: Option<Vec<GithubReleaseAsset>>,
}

#[derive(Debug, Deserialize)]
struct GithubReleaseAsset {
    #[serde(default)]
    name: String,
    #[serde(default)]
    browser_download_url: String,
}

struct ResolvedRelease {
    tag_name: String,
    remote_short_sha: Option<String>,
    download_url: String,
}

fn release_remote_short_sha(tag_name: &str, target_commitish: &str) -> Option<String> {
    if let Some(sha) = extract_sha_from_tag(tag_name) {
        return short_sha(Some(sha));
    }
    let commitish = target_commitish.trim();
    if is_hex_sha(commitish) {
        return short_sha(Some(commitish));
    }
    short_sha(Some(tag_name))
}

fn release_result_base(
    status: &str,
    message: impl Into<String>,
    current_sha: &str,
) -> UpdateResult {
    let local = (!current_sha.trim().is_empty()).then(|| current_sha.trim().to_string());
    UpdateResult::base(status, message, utc_now())
        .with_local_sha(local)
        .with_branch_remote(
            Some("release".to_string()),
            Some("github-releases".to_string()),
        )
}

fn inspect_release_from_url(
    api_url: &str,
    current_sha: &str,
    timeout_sec: f64,
) -> std::result::Result<(ResolvedRelease, UpdateResult), Box<UpdateResult>> {
    let agent = ureq::AgentBuilder::new()
        .timeout(clamp_http_timeout(timeout_sec))
        .build();
    let user_agent = format!("notflix/{}", crate::VERSION);

    let response = match agent
        .get(api_url)
        .set("User-Agent", &user_agent)
        .set("Accept", "application/vnd.github+json")
        .call()
    {
        Ok(resp) => resp,
        Err(ureq::Error::Transport(_)) => {
            return Err(Box::new(release_result_base(
                "offline",
                "Offline or GitHub Releases unreachable.",
                current_sha,
            )));
        }
        Err(ureq::Error::Status(code, _)) => {
            if code >= 500 || code == 429 || code == 403 {
                let msg = if code == 429 || code == 403 {
                    format!("GitHub Releases API rate-limited or unavailable (HTTP {code}).")
                } else {
                    format!("GitHub Releases temporarily unavailable (HTTP {code}).")
                };
                return Err(Box::new(release_result_base("offline", msg, current_sha)));
            }
            let msg = if code == 404 {
                "No published GitHub Release found.".to_string()
            } else {
                format!("GitHub Releases returned HTTP {code}.")
            };
            return Err(Box::new(release_result_base("error", msg, current_sha)));
        }
    };

    let release: GithubRelease = match response.into_json() {
        Ok(parsed) => parsed,
        Err(err) => {
            return Err(Box::new(release_result_base(
                "error",
                format!("Failed to parse GitHub Releases metadata: {err}"),
                current_sha,
            )));
        }
    };

    let tag_name = release
        .tag_name
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    if tag_name.is_empty() {
        return Err(Box::new(release_result_base(
            "error",
            "GitHub Release response did not include a tag_name.",
            current_sha,
        )));
    }

    let target_commitish = release
        .target_commitish
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let remote_short = release_remote_short_sha(&tag_name, &target_commitish);

    let attach_remote = |mut res: UpdateResult| -> UpdateResult {
        res.remote_sha = Some(tag_name.clone());
        res.remote_short_sha = remote_short.clone();
        res
    };

    if release_matches_build(&tag_name, &target_commitish, current_sha) {
        return Err(Box::new(attach_remote(release_result_base(
            "up_to_date",
            "Already running the latest Notflix release.",
            current_sha,
        ))));
    }

    let Some(expected_asset) = release_asset_name() else {
        return Err(Box::new(attach_remote(release_result_base(
            "unsupported",
            format!(
                "No prebuilt release binary for {}-{}.",
                std::env::consts::OS,
                std::env::consts::ARCH
            ),
            current_sha,
        ))));
    };

    let assets = release.assets.unwrap_or_default();
    let matching = assets.into_iter().find(|asset| {
        asset.name == expected_asset && !asset.browser_download_url.trim().is_empty()
    });

    let Some(asset) = matching else {
        return Err(Box::new(attach_remote(release_result_base(
            "unsupported",
            format!("Release {tag_name} does not contain a binary for {expected_asset}."),
            current_sha,
        ))));
    };

    let mut check_res = attach_remote(release_result_base(
        "update_available",
        format!("New release available: {tag_name}"),
        current_sha,
    ));
    check_res.behind_count = 1;

    Ok((
        ResolvedRelease {
            tag_name,
            remote_short_sha: remote_short,
            download_url: asset.browser_download_url.trim().to_string(),
        },
        check_res,
    ))
}

/// Check a GitHub Releases API URL for a newer prebuilt binary without downloading it.
pub fn check_release_updates_from_url(
    api_url: &str,
    current_sha: &str,
    timeout_sec: f64,
) -> UpdateResult {
    match inspect_release_from_url(api_url, current_sha, timeout_sec) {
        Ok((_, check_res)) => check_res,
        Err(res) => *res,
    }
}

/// Check GitHub Releases (`https://api.github.com/repos/{repo_slug}/releases/latest`) for a newer binary.
pub fn check_release_updates(repo_slug: &str, current_sha: &str, timeout_sec: f64) -> UpdateResult {
    let slug = repo_slug.trim();
    let slug = if slug.is_empty() {
        DEFAULT_RELEASE_REPO
    } else {
        slug
    };
    let api_url = format!("https://api.github.com/repos/{slug}/releases/latest");
    check_release_updates_from_url(&api_url, current_sha, timeout_sec)
}

/// Download and atomically install a GitHub Release binary to `target_exe`, backing up `db_path` first.
pub fn apply_release_update_to_path(
    api_url: &str,
    target_exe: &Path,
    current_sha: &str,
    db_path: Option<&Path>,
    timeout_sec: f64,
) -> UpdateResult {
    let (resolved, check_res) = match inspect_release_from_url(api_url, current_sha, timeout_sec) {
        Ok(pair) => pair,
        Err(res) => return *res,
    };

    let mut backup_str: Option<String> = None;
    if let Some(db) = db_path {
        let label = format!(
            "pre-{}",
            check_res.local_short_sha.as_deref().unwrap_or("release")
        );
        match backup_database(db, &label, DEFAULT_BACKUP_KEEP) {
            Ok(Some(path)) => backup_str = Some(path.to_string_lossy().into_owned()),
            Ok(None) => {}
            Err(err) => {
                return UpdateResult::from_check(
                    "error",
                    format!("Aborted update because database backup failed: {err}"),
                    utc_now(),
                    &check_res,
                );
            }
        }
    }

    let make_err = |status: &str, msg: String, backup: Option<String>| -> UpdateResult {
        let mut res = UpdateResult::from_check(status, msg, utc_now(), &check_res);
        res.remote_short_sha = check_res.remote_short_sha.clone();
        res.backup_path = backup;
        res
    };

    let read_timeout = if timeout_sec.is_finite() {
        Duration::from_secs_f64(timeout_sec.max(30.0).clamp(1.0, 120.0))
    } else {
        Duration::from_secs(30)
    };
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(clamp_http_timeout(timeout_sec))
        .timeout_read(read_timeout)
        .build();
    let user_agent = format!("notflix/{}", crate::VERSION);

    let resp = match agent
        .get(&resolved.download_url)
        .set("User-Agent", &user_agent)
        .set("Accept", "application/octet-stream")
        .call()
    {
        Ok(r) => r,
        Err(ureq::Error::Transport(_)) => {
            return make_err(
                "offline",
                "Offline or failed to download release binary.".to_string(),
                backup_str,
            );
        }
        Err(ureq::Error::Status(code, _)) => {
            let status = if code >= 500 || code == 429 || code == 403 {
                "offline"
            } else {
                "error"
            };
            return make_err(
                status,
                format!("Release binary download failed (HTTP {code})."),
                backup_str,
            );
        }
    };

    let mut bytes = Vec::new();
    let mut limited = resp.into_reader().take(MAX_RELEASE_BINARY_BYTES + 1);
    if let Err(err) = limited.read_to_end(&mut bytes) {
        return make_err(
            "offline",
            format!("Failed while downloading release binary: {err}"),
            backup_str,
        );
    }

    if (bytes.len() as u64) > MAX_RELEASE_BINARY_BYTES {
        return make_err(
            "error",
            "Downloaded release binary exceeds 128 MiB safety limit.".to_string(),
            backup_str,
        );
    }

    if !validate_binary_magic(&bytes) {
        return make_err(
            "error",
            "Downloaded release binary failed integrity check.".to_string(),
            backup_str,
        );
    }

    let staged = append_file_suffix(target_exe, ".new");
    if let Err(err) = std::fs::write(&staged, &bytes) {
        let _ = std::fs::remove_file(&staged);
        return make_err(
            "error",
            format!("Failed to write staged release binary: {err}"),
            backup_str,
        );
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(err) = std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
        {
            let _ = std::fs::remove_file(&staged);
            return make_err(
                "error",
                format!("Failed to set executable permissions on updated binary: {err}"),
                backup_str,
            );
        }
        if let Err(err) = std::fs::rename(&staged, target_exe) {
            let _ = std::fs::remove_file(&staged);
            return make_err(
                "error",
                format!("Failed to replace executable with updated binary: {err}"),
                backup_str,
            );
        }
    }

    #[cfg(windows)]
    {
        let old_path = append_file_suffix(target_exe, ".old");
        let _ = std::fs::remove_file(&old_path);
        let moved_existing = if target_exe.exists() {
            if let Err(err) = std::fs::rename(target_exe, &old_path) {
                let _ = std::fs::remove_file(&staged);
                return make_err(
                    "error",
                    format!("Failed to move running executable aside: {err}"),
                    backup_str,
                );
            }
            true
        } else {
            false
        };

        if let Err(err) = std::fs::rename(&staged, target_exe) {
            if moved_existing {
                let _ = std::fs::rename(&old_path, target_exe);
            }
            let _ = std::fs::remove_file(&staged);
            return make_err(
                "error",
                format!("Failed to install updated executable: {err}"),
                backup_str,
            );
        }
    }

    #[cfg(not(any(unix, windows)))]
    {
        if let Err(err) = std::fs::rename(&staged, target_exe) {
            let _ = std::fs::remove_file(&staged);
            return make_err(
                "error",
                format!("Failed to replace executable with updated binary: {err}"),
                backup_str,
            );
        }
    }

    let local = (!current_sha.trim().is_empty()).then(|| current_sha.trim().to_string());
    let mut res = UpdateResult::base(
        "updated",
        format!(
            "Updated to {}. Restart Notflix to run the new version.",
            resolved.tag_name
        ),
        utc_now(),
    )
    .with_local_sha(local.clone())
    .with_old_sha(local)
    .with_branch_remote(
        Some("release".to_string()),
        Some("github-releases".to_string()),
    );
    res.remote_sha = Some(resolved.tag_name);
    res.remote_short_sha = resolved.remote_short_sha;
    res.updated = true;
    res.restart_required = true;
    res.backup_path = backup_str;
    res
}

/// Download and install the latest GitHub Release binary for the running Notflix executable.
pub fn apply_release_update(
    repo_slug: &str,
    db_path: Option<&Path>,
    timeout_sec: f64,
) -> UpdateResult {
    let slug = repo_slug.trim();
    let slug = if slug.is_empty() {
        DEFAULT_RELEASE_REPO
    } else {
        slug
    };
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(err) => {
            return release_result_base(
                "error",
                format!("Could not determine current executable path: {err}"),
                crate::BUILD_SHA,
            );
        }
    };
    let api_url = format!("https://api.github.com/repos/{slug}/releases/latest");
    apply_release_update_to_path(&api_url, &exe, crate::BUILD_SHA, db_path, timeout_sec)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdateModeOverride {
    Auto,
    Git,
    Release,
}

fn update_mode_override() -> UpdateModeOverride {
    let raw = std::env::var("NOTFLIX_UPDATE_MODE")
        .ok()
        .map(|s| s.trim().to_ascii_lowercase());
    match raw.as_deref() {
        Some("release") => UpdateModeOverride::Release,
        Some("git") => UpdateModeOverride::Git,
        _ => UpdateModeOverride::Auto,
    }
}

fn release_mode_info() -> UpdateResult {
    let slug = release_repo_slug();
    let sha = crate::BUILD_SHA;
    UpdateResult::base(
        "release",
        format!("Running release build ({sha}) via {slug}"),
        utc_now(),
    )
    .with_local_sha(Some(sha.to_string()))
    .with_branch_remote(Some(slug), Some("github-releases".to_string()))
}

/// Return local Git or GitHub Release build info without making any network requests.
///
/// Honors `NOTFLIX_UPDATE_MODE` (`"release"` or `"git"`). When `allow_release_fallback`
/// is `true` and `repo_dir` is not a Git repository (or `git` is unavailable), returns
/// release-mode info using the embedded build SHA.
pub fn get_update_mode_info(repo_dir: &Path, allow_release_fallback: bool) -> UpdateResult {
    match update_mode_override() {
        UpdateModeOverride::Release => return release_mode_info(),
        UpdateModeOverride::Git => return get_local_repo_info(Some(repo_dir)),
        UpdateModeOverride::Auto => {}
    }
    let git_info = get_local_repo_info(Some(repo_dir));
    if allow_release_fallback && matches!(git_info.status.as_str(), "not_a_repo" | "no_git") {
        return release_mode_info();
    }
    git_info
}

/// Check for updates via Git, optionally falling back to GitHub Releases for standalone binaries.
pub fn check_for_updates_with_fallback(
    repo_dir: &Path,
    allow_release_fallback: bool,
    timeout_sec: f64,
) -> UpdateResult {
    match update_mode_override() {
        UpdateModeOverride::Release => {
            return check_release_updates(&release_repo_slug(), crate::BUILD_SHA, timeout_sec);
        }
        UpdateModeOverride::Git => return check_for_updates(Some(repo_dir), timeout_sec),
        UpdateModeOverride::Auto => {}
    }
    let git_res = check_for_updates(Some(repo_dir), timeout_sec);
    if allow_release_fallback && matches!(git_res.status.as_str(), "not_a_repo" | "no_git") {
        return check_release_updates(&release_repo_slug(), crate::BUILD_SHA, timeout_sec);
    }
    git_res
}

/// Apply updates via Git, optionally falling back to GitHub Releases for standalone binaries.
pub fn apply_update_with_fallback(
    repo_dir: &Path,
    allow_release_fallback: bool,
    db_path: Option<&Path>,
    timeout_sec: f64,
) -> UpdateResult {
    match update_mode_override() {
        UpdateModeOverride::Release => {
            return apply_release_update(&release_repo_slug(), db_path, timeout_sec);
        }
        UpdateModeOverride::Git => return apply_update(Some(repo_dir), db_path, timeout_sec),
        UpdateModeOverride::Auto => {}
    }
    let git_res = apply_update(Some(repo_dir), db_path, timeout_sec);
    if allow_release_fallback && matches!(git_res.status.as_str(), "not_a_repo" | "no_git") {
        return apply_release_update(&release_repo_slug(), db_path, timeout_sec);
    }
    git_res
}

/// Fail-safe startup entry point: check and apply fast-forward Git updates or release binary updates.
///
/// When `repo_dir` is `None`, release fallback is enabled so standalone binaries can auto-update.
/// When `repo_dir` is `Some(...)`, release fallback is disabled unless `NOTFLIX_UPDATE_MODE=release`.
pub fn auto_update_on_startup(
    repo_dir: Option<&Path>,
    db_path: Option<&Path>,
    timeout_sec: f64,
) -> UpdateResult {
    let allow_release_fallback = repo_dir.is_none();
    let root = resolve_repo_root(repo_dir);
    apply_update_with_fallback(&root, allow_release_fallback, db_path, timeout_sec)
}

/// Decide whether the current process should immediately relaunch itself after a
/// successful startup update so the newly installed executable takes effect right away.
///
/// Returns `true` when:
/// - `already_relaunched` is `false` (loop guard),
/// - `res.updated` is `true`, and
/// - either the update came from GitHub Releases (which directly replaced the running
///   executable) OR a Git fast-forward updated a precompiled executable outside Cargo's
///   `target/` build directory.
pub fn should_relaunch_after_startup_update(
    res: &UpdateResult,
    exe_path: &Path,
    already_relaunched: bool,
) -> bool {
    if already_relaunched || !res.updated {
        return false;
    }
    if res.remote.as_deref() == Some("github-releases") {
        return true;
    }
    let in_cargo_target = exe_path.components().any(|c| {
        c.as_os_str()
            .to_str()
            .is_some_and(|s| s.eq_ignore_ascii_case("target"))
    });
    !in_cargo_target
}

/// Re-execute the newly updated binary in place (Unix `exec`) or via foreground child
/// handoff (Windows), forwarding all original CLI arguments and setting loop-guard
/// environment variables so the relaunched process does not re-enter startup updates.
pub fn relaunch_updated_binary() -> std::io::Result<std::process::ExitCode> {
    let exe = std::env::current_exe()?;
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();

    let mut cmd = Command::new(&exe);
    cmd.args(&args)
        .env("NOTFLIX_SKIP_AUTO_UPDATE", "1")
        .env("NOTFLIX_RELAUNCHED", "1");

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(cmd.exec())
    }

    #[cfg(not(unix))]
    {
        let status = cmd.status()?;
        let code = status
            .code()
            .and_then(|c| u8::try_from(c).ok())
            .unwrap_or_else(|| if status.success() { 0 } else { 1 });
        Ok(std::process::ExitCode::from(code))
    }
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

    #[test]
    fn relaunch_policy_handles_release_git_target_and_loop_guard() {
        let mut res = UpdateResult::base("updated", "ok", utc_now());
        res.updated = true;
        res.remote = Some("github-releases".to_string());

        // Standalone GitHub Release update always relaunches on first pass
        assert!(should_relaunch_after_startup_update(
            &res,
            Path::new("/usr/local/bin/notflix"),
            false
        ));
        // Loop guard blocks second relaunch
        assert!(!should_relaunch_after_startup_update(
            &res,
            Path::new("/usr/local/bin/notflix"),
            true
        ));

        // Git fast-forward of repo-root precompiled binary relaunches
        res.remote = Some("origin".to_string());
        assert!(should_relaunch_after_startup_update(
            &res,
            Path::new("/home/user/notflix/notflix"),
            false
        ));
        // Cargo dev/release build inside target/ does not relaunch stale binary
        assert!(!should_relaunch_after_startup_update(
            &res,
            Path::new("/home/user/notflix/target/release/notflix"),
            false
        ));

        // Non-updated result never relaunches
        res.updated = false;
        res.remote = Some("github-releases".to_string());
        assert!(!should_relaunch_after_startup_update(
            &res,
            Path::new("/usr/local/bin/notflix"),
            false
        ));
    }
}
