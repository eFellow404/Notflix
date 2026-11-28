//! Dual-mode update dispatch (Git vs. GitHub Releases), startup orchestration, and self-relaunch.

use std::path::Path;
use std::process::Command;

use crate::util::{env_var_trimmed, utc_now};

use super::git::{apply_update, check_for_updates, get_local_repo_info, resolve_repo_root};
use super::release::{apply_release_update, check_release_updates, release_repo_slug};
use super::UpdateResult;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdateModeOverride {
    Auto,
    Git,
    Release,
}

fn update_mode_override() -> UpdateModeOverride {
    let raw = env_var_trimmed("NOTFLIX_UPDATE_MODE").map(|s| s.to_ascii_lowercase());
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

fn dispatch_with_fallback<G, R>(
    allow_release_fallback: bool,
    git_fn: G,
    release_fn: R,
) -> UpdateResult
where
    G: FnOnce() -> UpdateResult,
    R: FnOnce() -> UpdateResult,
{
    match update_mode_override() {
        UpdateModeOverride::Release => release_fn(),
        UpdateModeOverride::Git => git_fn(),
        UpdateModeOverride::Auto => {
            let git_res = git_fn();
            if allow_release_fallback && matches!(git_res.status.as_str(), "not_a_repo" | "no_git")
            {
                release_fn()
            } else {
                git_res
            }
        }
    }
}

/// Return local Git or GitHub Release build info without making any network requests.
///
/// Honors `NOTFLIX_UPDATE_MODE` (`"release"` or `"git"`). When `allow_release_fallback`
/// is `true` and `repo_dir` is not a Git repository (or `git` is unavailable), returns
/// release-mode info using the embedded build SHA.
pub fn get_update_mode_info(repo_dir: &Path, allow_release_fallback: bool) -> UpdateResult {
    dispatch_with_fallback(
        allow_release_fallback,
        || get_local_repo_info(Some(repo_dir)),
        release_mode_info,
    )
}

/// Check for updates via Git, optionally falling back to GitHub Releases for standalone binaries.
pub fn check_for_updates_with_fallback(
    repo_dir: &Path,
    allow_release_fallback: bool,
    timeout_sec: f64,
) -> UpdateResult {
    dispatch_with_fallback(
        allow_release_fallback,
        || check_for_updates(Some(repo_dir), timeout_sec),
        || check_release_updates(&release_repo_slug(), crate::BUILD_SHA, timeout_sec),
    )
}

/// Apply updates via Git, optionally falling back to GitHub Releases for standalone binaries.
pub fn apply_update_with_fallback(
    repo_dir: &Path,
    allow_release_fallback: bool,
    db_path: Option<&Path>,
    timeout_sec: f64,
) -> UpdateResult {
    dispatch_with_fallback(
        allow_release_fallback,
        || apply_update(Some(repo_dir), db_path, timeout_sec),
        || apply_release_update(&release_repo_slug(), db_path, timeout_sec),
    )
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
