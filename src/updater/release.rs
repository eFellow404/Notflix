//! GitHub Releases binary discovery, integrity validation, streaming download, and atomic swap.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::util::{
    build_http_agent, clamp_timeout_secs, env_var_trimmed, http_user_agent, utc_now,
    validate_executable_magic_for_os,
};

use super::backup::run_pre_update_backup;
use super::{short_sha, UpdateResult, DEFAULT_FETCH_TIMEOUT_SEC, DEFAULT_RELEASE_REPO};

/// Maximum allowed download size for a release binary (128 MiB).
const MAX_RELEASE_BINARY_BYTES: u64 = 128 * 1024 * 1024;

/// Streaming buffer size used while writing release binaries to disk (64 KiB).
const STREAM_CHUNK_BYTES: usize = 65_536;

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

/// Return the configured GitHub repository slug (`owner/repo`) for release updates.
///
/// Reads `NOTFLIX_RELEASE_REPO` when set and non-empty; otherwise defaults to
/// [`DEFAULT_RELEASE_REPO`].
pub fn release_repo_slug() -> String {
    env_var_trimmed("NOTFLIX_RELEASE_REPO").unwrap_or_else(|| DEFAULT_RELEASE_REPO.to_string())
}

fn release_latest_api_url(repo_slug: &str) -> String {
    let slug = repo_slug.trim();
    let slug = if slug.is_empty() {
        DEFAULT_RELEASE_REPO
    } else {
        slug
    };
    format!("https://api.github.com/repos/{slug}/releases/latest")
}

fn is_transient_http_status(code: u16) -> bool {
    code >= 500 || code == 429 || code == 403
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
    bytes.len() >= 4 && validate_executable_magic_for_os(bytes, os)
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

fn fetch_latest_release_json(
    api_url: &str,
    current_sha: &str,
    timeout_sec: f64,
) -> std::result::Result<GithubRelease, Box<UpdateResult>> {
    let timeout = clamp_timeout_secs(timeout_sec, 1.0, 60.0, DEFAULT_FETCH_TIMEOUT_SEC);
    let agent = build_http_agent(timeout, timeout);
    let user_agent = http_user_agent();

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
            if is_transient_http_status(code) {
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

    response.into_json().map_err(|err| {
        Box::new(release_result_base(
            "error",
            format!("Failed to parse GitHub Releases metadata: {err}"),
            current_sha,
        ))
    })
}

fn select_release_asset(
    release: GithubRelease,
    current_sha: &str,
) -> std::result::Result<(ResolvedRelease, UpdateResult), Box<UpdateResult>> {
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

fn inspect_release_from_url(
    api_url: &str,
    current_sha: &str,
    timeout_sec: f64,
) -> std::result::Result<(ResolvedRelease, UpdateResult), Box<UpdateResult>> {
    let release = fetch_latest_release_json(api_url, current_sha, timeout_sec)?;
    select_release_asset(release, current_sha)
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
    let api_url = release_latest_api_url(repo_slug);
    check_release_updates_from_url(&api_url, current_sha, timeout_sec)
}

fn stream_release_binary_to_staged(
    download_url: &str,
    staged: &Path,
    timeout_sec: f64,
) -> std::result::Result<(), (&'static str, String)> {
    let connect_timeout = clamp_timeout_secs(timeout_sec, 1.0, 60.0, DEFAULT_FETCH_TIMEOUT_SEC);
    let read_timeout = clamp_timeout_secs(timeout_sec, 30.0, 120.0, 30.0);
    let agent = build_http_agent(connect_timeout, read_timeout);
    let user_agent = http_user_agent();

    let resp = match agent
        .get(download_url)
        .set("User-Agent", &user_agent)
        .set("Accept", "application/octet-stream")
        .call()
    {
        Ok(r) => r,
        Err(ureq::Error::Transport(_)) => {
            return Err((
                "offline",
                "Offline or failed to download release binary.".to_string(),
            ));
        }
        Err(ureq::Error::Status(code, _)) => {
            let status = if is_transient_http_status(code) {
                "offline"
            } else {
                "error"
            };
            return Err((
                status,
                format!("Release binary download failed (HTTP {code})."),
            ));
        }
    };

    if let Some(content_len) = resp
        .header("Content-Length")
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        if content_len > MAX_RELEASE_BINARY_BYTES {
            return Err((
                "error",
                "Downloaded release binary exceeds 128 MiB safety limit.".to_string(),
            ));
        }
    }

    let write_res = (|| -> std::result::Result<(), (&'static str, String)> {
        let mut reader = resp.into_reader();
        let mut file = std::fs::File::create(staged).map_err(|err| {
            (
                "error",
                format!("Failed to write staged release binary: {err}"),
            )
        })?;

        let mut buf = vec![0u8; STREAM_CHUNK_BYTES];
        let mut header = [0u8; 4];
        let mut header_len = 0usize;
        let mut magic_checked = false;
        let mut total_bytes = 0u64;

        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) => {
                    return Err((
                        "offline",
                        format!("Failed while downloading release binary: {err}"),
                    ));
                }
            };

            total_bytes = total_bytes.saturating_add(n as u64);
            if total_bytes > MAX_RELEASE_BINARY_BYTES {
                return Err((
                    "error",
                    "Downloaded release binary exceeds 128 MiB safety limit.".to_string(),
                ));
            }

            let chunk = &buf[..n];
            if !magic_checked {
                let take = (4 - header_len).min(chunk.len());
                header[header_len..header_len + take].copy_from_slice(&chunk[..take]);
                header_len += take;
                if header_len == 4 {
                    if !validate_binary_magic(&header) {
                        return Err((
                            "error",
                            "Downloaded release binary failed integrity check.".to_string(),
                        ));
                    }
                    magic_checked = true;
                }
            }

            file.write_all(chunk).map_err(|err| {
                (
                    "error",
                    format!("Failed to write staged release binary: {err}"),
                )
            })?;
        }

        if !magic_checked {
            return Err((
                "error",
                "Downloaded release binary failed integrity check.".to_string(),
            ));
        }

        file.flush().map_err(|err| {
            (
                "error",
                format!("Failed to write staged release binary: {err}"),
            )
        })?;
        Ok(())
    })();

    if write_res.is_err() {
        let _ = std::fs::remove_file(staged);
    }
    write_res
}

fn install_staged_executable(staged: &Path, target_exe: &Path) -> std::result::Result<(), String> {
    #[cfg(unix)]
    {
        if let Err(err) = crate::util::set_executable_permissions(staged) {
            let _ = std::fs::remove_file(staged);
            return Err(format!(
                "Failed to set executable permissions on updated binary: {err}"
            ));
        }
        if let Err(err) = std::fs::rename(staged, target_exe) {
            let _ = std::fs::remove_file(staged);
            return Err(format!(
                "Failed to replace executable with updated binary: {err}"
            ));
        }
    }

    #[cfg(windows)]
    {
        let old_path = append_file_suffix(target_exe, ".old");
        let _ = std::fs::remove_file(&old_path);
        let moved_existing = if target_exe.exists() {
            if let Err(err) = std::fs::rename(target_exe, &old_path) {
                let _ = std::fs::remove_file(staged);
                return Err(format!("Failed to move running executable aside: {err}"));
            }
            true
        } else {
            false
        };

        if let Err(err) = std::fs::rename(staged, target_exe) {
            if moved_existing {
                let _ = std::fs::rename(&old_path, target_exe);
            }
            let _ = std::fs::remove_file(staged);
            return Err(format!("Failed to install updated executable: {err}"));
        }
    }

    #[cfg(not(any(unix, windows)))]
    {
        if let Err(err) = std::fs::rename(staged, target_exe) {
            let _ = std::fs::remove_file(staged);
            return Err(format!(
                "Failed to replace executable with updated binary: {err}"
            ));
        }
    }

    Ok(())
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

    let backup_str = match run_pre_update_backup(db_path, "release", &check_res) {
        Ok(path) => path,
        Err(res) => return *res,
    };

    let make_err = |status: &str, msg: String, backup: Option<String>| -> UpdateResult {
        let mut res = UpdateResult::from_check(status, msg, utc_now(), &check_res);
        res.backup_path = backup;
        res
    };

    let staged = append_file_suffix(target_exe, ".new");
    if let Err((status, msg)) =
        stream_release_binary_to_staged(&resolved.download_url, &staged, timeout_sec)
    {
        return make_err(status, msg, backup_str);
    }

    if let Err(msg) = install_staged_executable(&staged, target_exe) {
        return make_err("error", msg, backup_str);
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
    let api_url = release_latest_api_url(repo_slug);
    apply_release_update_to_path(&api_url, &exe, crate::BUILD_SHA, db_path, timeout_sec)
}
