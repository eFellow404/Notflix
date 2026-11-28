//! Offline-safe automatic Git and GitHub Releases updater with pre-update SQLite backups.
//!
//! Checks the configured Git remote (for source installs) or GitHub Releases (for prebuilt
//! standalone binaries) for newer builds and applies updates cleanly without ever crashing
//! when offline or touching user data.

mod backup;
mod git;
mod release;
mod startup;

use serde::{Deserialize, Serialize};

pub use backup::backup_database;
pub use git::{apply_update, check_for_updates, default_repo_dir, get_local_repo_info};
pub use release::{
    apply_release_update, apply_release_update_to_path, check_release_updates,
    check_release_updates_from_url, cleanup_old_binary_artifacts, release_asset_name,
    release_matches_build, release_repo_slug, validate_binary_magic, validate_binary_magic_for_os,
};
pub use startup::{
    apply_update_with_fallback, auto_update_on_startup, check_for_updates_with_fallback,
    get_update_mode_info, relaunch_updated_binary, should_relaunch_after_startup_update,
};

/// Default timeout (in seconds) for network-facing update operations.
pub const DEFAULT_FETCH_TIMEOUT_SEC: f64 = 5.0;

/// Default number of pre-update SQLite backups retained per database.
pub const DEFAULT_BACKUP_KEEP: usize = 5;

/// Default GitHub repository (`owner/repo`) queried for prebuilt release binaries.
pub const DEFAULT_RELEASE_REPO: &str = "eFellow404/Notflix";

/// Standard short SHA display length.
pub(super) const SHORT_SHA_LEN: usize = 7;

/// Default timeout (in seconds) for local, non-network Git commands.
pub(super) const GIT_LOCAL_TIMEOUT_SEC: f64 = 3.0;

/// Minimum timeout (in seconds) for `git merge --ff-only`.
pub(super) const GIT_MERGE_MIN_TIMEOUT_SEC: f64 = 10.0;

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
    pub(super) fn base(status: &str, message: impl Into<String>, checked_at: String) -> Self {
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

    pub(super) fn with_local_sha(mut self, sha: Option<String>) -> Self {
        self.local_short_sha = short_sha(sha.as_deref());
        self.local_sha = sha;
        self
    }

    pub(super) fn with_remote_sha(mut self, sha: Option<String>) -> Self {
        self.remote_short_sha = short_sha(sha.as_deref());
        self.remote_sha = sha;
        self
    }

    pub(super) fn with_old_sha(mut self, sha: Option<String>) -> Self {
        self.old_short_sha = short_sha(sha.as_deref());
        self.old_sha = sha;
        self
    }

    pub(super) fn with_branch_remote(
        mut self,
        branch: Option<String>,
        remote: Option<String>,
    ) -> Self {
        self.branch = branch;
        self.remote = remote;
        self
    }

    fn from_target(
        status: &str,
        message: impl Into<String>,
        checked_at: String,
        target: &git::RepoTarget,
    ) -> Self {
        Self::base(status, message, checked_at)
            .with_local_sha(target.local_sha.clone())
            .with_branch_remote(Some(target.branch.clone()), Some(target.remote.clone()))
    }

    pub(super) fn from_check(
        status: &str,
        message: impl Into<String>,
        checked_at: String,
        check: &UpdateResult,
    ) -> Self {
        let mut res = Self::base(status, message, checked_at)
            .with_branch_remote(check.branch.clone(), check.remote.clone());
        res.local_sha = check.local_sha.clone();
        res.local_short_sha = check.local_short_sha.clone();
        res.remote_sha = check.remote_sha.clone();
        res.remote_short_sha = check.remote_short_sha.clone();
        res.behind_count = check.behind_count;
        res.ahead_count = check.ahead_count;
        res
    }
}

pub(super) fn short_sha(sha: Option<&str>) -> Option<String> {
    sha.filter(|s| !s.is_empty())
        .map(|s| s.chars().take(SHORT_SHA_LEN).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::utc_now;

    #[test]
    fn from_check_preserves_short_shas_for_release_tags() {
        let mut check = UpdateResult::base("update_available", "New release", utc_now())
            .with_local_sha(Some("1111111".to_string()))
            .with_branch_remote(
                Some("release".to_string()),
                Some("github-releases".to_string()),
            );
        check.remote_sha = Some("build-20261127-023000-4952bb1".to_string());
        check.remote_short_sha = Some("4952bb1".to_string());
        check.behind_count = 1;

        let derived = UpdateResult::from_check("offline", "failed", utc_now(), &check);
        assert_eq!(derived.local_short_sha.as_deref(), Some("1111111"));
        assert_eq!(
            derived.remote_sha.as_deref(),
            Some("build-20261127-023000-4952bb1")
        );
        assert_eq!(derived.remote_short_sha.as_deref(), Some("4952bb1"));
        assert_eq!(derived.behind_count, 1);
    }
}
