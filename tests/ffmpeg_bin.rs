//! Integration tests for automatic `ffmpeg` / `ffprobe` binary resolution and downloading.

mod common;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use common::{
    assert_is_executable, fake_valid_binary_for_os, unused_local_port, MockHttpServer, MockResponse,
};
use notflix::ffmpeg_bin::{
    asset_name_for, binary_filename_for, cleanup_stale_partial_downloads, download_tool_binary,
    platform_asset_suffix_for, resolve_tool_with, validate_executable_magic_for_os, FfmpegManager,
    DEFAULT_FFMPEG_BASE_URL, DEFAULT_MIN_BINARY_BYTES, MAX_BINARY_BYTES,
};
use tempfile::tempdir;

fn write_dummy_executable(path: &Path) {
    let payload = fake_valid_binary_for_os(std::env::consts::OS, 64);
    fs::write(path, payload).expect("write dummy executable");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o755);
        fs::set_permissions(path, perms).expect("chmod dummy executable");
    }
}

fn assert_no_part_files(dir: &Path) {
    let part_files: Vec<String> = fs::read_dir(dir)
        .expect("read target dir")
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.contains(".part."))
        .collect();
    assert!(
        part_files.is_empty(),
        "expected no .part. files in {}, found {part_files:?}",
        dir.display()
    );
}

#[test]
fn platform_asset_names_and_filenames_cover_all_supported_targets() {
    assert!(DEFAULT_FFMPEG_BASE_URL.starts_with("https://"));
    const { assert!(DEFAULT_MIN_BINARY_BYTES > 0) };
    const { assert!(MAX_BINARY_BYTES > DEFAULT_MIN_BINARY_BYTES) };

    let supported = [
        ("linux", "x86_64"),
        ("linux", "aarch64"),
        ("macos", "aarch64"),
        ("macos", "x86_64"),
        ("windows", "x86_64"),
    ];

    let mut ffmpeg_assets = HashSet::new();
    let mut ffprobe_assets = HashSet::new();

    for (os, arch) in supported {
        let suffix = platform_asset_suffix_for(os, arch)
            .unwrap_or_else(|| panic!("expected suffix for ({os}, {arch})"));
        assert!(!suffix.is_empty());

        let ffmpeg_asset = asset_name_for("ffmpeg", os, arch)
            .unwrap_or_else(|| panic!("expected ffmpeg asset for ({os}, {arch})"));
        let ffprobe_asset = asset_name_for("ffprobe", os, arch)
            .unwrap_or_else(|| panic!("expected ffprobe asset for ({os}, {arch})"));

        assert!(
            ffmpeg_asset.starts_with("ffmpeg-"),
            "unexpected ffmpeg asset name: {ffmpeg_asset}"
        );
        assert!(
            ffprobe_asset.starts_with("ffprobe-"),
            "unexpected ffprobe asset name: {ffprobe_asset}"
        );
        assert!(
            ffmpeg_asset.contains(suffix),
            "ffmpeg asset {ffmpeg_asset} should include suffix {suffix}"
        );
        assert!(
            ffprobe_asset.contains(suffix),
            "ffprobe asset {ffprobe_asset} should include suffix {suffix}"
        );
        assert_ne!(ffmpeg_asset, ffprobe_asset);

        ffmpeg_assets.insert(ffmpeg_asset);
        ffprobe_assets.insert(ffprobe_asset);
    }

    assert_eq!(ffmpeg_assets.len(), supported.len());
    assert_eq!(ffprobe_assets.len(), supported.len());

    // Unsupported platform returns None
    assert!(platform_asset_suffix_for("freebsd", "x86_64").is_none());
    assert!(asset_name_for("ffmpeg", "freebsd", "x86_64").is_none());
    assert!(asset_name_for("ffprobe", "freebsd", "x86_64").is_none());

    // Binary filenames per OS
    assert_eq!(binary_filename_for("ffmpeg", "linux"), "ffmpeg");
    assert_eq!(binary_filename_for("ffmpeg", "macos"), "ffmpeg");
    assert_eq!(binary_filename_for("ffmpeg", "windows"), "ffmpeg.exe");

    assert_eq!(binary_filename_for("ffprobe", "linux"), "ffprobe");
    assert_eq!(binary_filename_for("ffprobe", "macos"), "ffprobe");
    assert_eq!(binary_filename_for("ffprobe", "windows"), "ffprobe.exe");
}

#[test]
fn validate_executable_magic_accepts_valid_and_rejects_html_or_wrong_os() {
    // ELF on Linux
    let elf = b"\x7fELF\x02\x01\x01\x00";
    assert!(validate_executable_magic_for_os(elf, "linux"));
    assert!(!validate_executable_magic_for_os(elf, "windows"));
    assert!(!validate_executable_magic_for_os(elf, "macos"));
    assert!(!validate_executable_magic_for_os(b"\x7fEL", "linux"));

    // PE on Windows
    let pe = b"MZ\x90\x00";
    assert!(validate_executable_magic_for_os(pe, "windows"));
    assert!(!validate_executable_magic_for_os(pe, "linux"));
    assert!(!validate_executable_magic_for_os(pe, "macos"));

    // Mach-O (64-bit big/little endian and universal fat binary) on macOS
    for magic in [
        [0xfe, 0xed, 0xfa, 0xcf],
        [0xcf, 0xfa, 0xed, 0xfe],
        [0xca, 0xfe, 0xba, 0xbe],
    ] {
        let mut payload = magic.to_vec();
        payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        assert!(validate_executable_magic_for_os(&payload, "macos"));
        assert!(!validate_executable_magic_for_os(&payload, "linux"));
        assert!(!validate_executable_magic_for_os(&payload, "windows"));
    }

    // HTML and empty payloads fail on all OSes
    let html = b"<!DOCTYPE html><html><body>Not a binary</body></html>";
    for os in ["linux", "macos", "windows", "freebsd"] {
        assert!(!validate_executable_magic_for_os(html, os));
        assert!(!validate_executable_magic_for_os(b"", os));
    }
}

#[test]
fn resolve_tool_priority_env_then_path_then_managed_dir() {
    let tmp = tempdir().expect("tempdir");
    let env_dir = tmp.path().join("env_dir");
    let path_dir = tmp.path().join("path_dir");
    let managed_dir = tmp.path().join("managed_dir");
    let empty_dir = tmp.path().join("empty_dir");

    fs::create_dir_all(&env_dir).expect("create env_dir");
    fs::create_dir_all(&path_dir).expect("create path_dir");
    fs::create_dir_all(&managed_dir).expect("create managed_dir");
    fs::create_dir_all(&empty_dir).expect("create empty_dir");

    let bin_name = binary_filename_for("ffmpeg", std::env::consts::OS);
    let env_bin = env_dir.join(&bin_name);
    let path_bin = path_dir.join(&bin_name);
    let managed_bin = managed_dir.join(&bin_name);

    write_dummy_executable(&env_bin);
    write_dummy_executable(&path_bin);
    write_dummy_executable(&managed_bin);

    let missing_env = env_dir.join("missing_ffmpeg");
    let missing_path = path_dir.join("missing_ffmpeg");

    // 1. Prefers env_path over path_lookup and managed_dir
    assert_eq!(
        resolve_tool_with(
            Some(&env_bin),
            Some(path_bin.clone()),
            &managed_dir,
            "ffmpeg"
        ),
        Some(env_bin.clone())
    );

    // 2. Falls back to path_lookup when env_path does not exist or is None
    assert_eq!(
        resolve_tool_with(
            Some(&missing_env),
            Some(path_bin.clone()),
            &managed_dir,
            "ffmpeg"
        ),
        Some(path_bin.clone())
    );
    assert_eq!(
        resolve_tool_with(None, Some(path_bin.clone()), &managed_dir, "ffmpeg"),
        Some(path_bin)
    );

    // 3. Falls back to managed_dir when neither env_path nor path_lookup exists
    assert_eq!(
        resolve_tool_with(
            Some(&missing_env),
            Some(missing_path.clone()),
            &managed_dir,
            "ffmpeg"
        ),
        Some(managed_bin.clone())
    );
    assert_eq!(
        resolve_tool_with(None, None, &managed_dir, "ffmpeg"),
        Some(managed_bin)
    );

    // 4. Returns None when none of the candidates exist
    assert_eq!(
        resolve_tool_with(Some(&missing_env), Some(missing_path), &empty_dir, "ffmpeg"),
        None
    );
    assert_eq!(resolve_tool_with(None, None, &empty_dir, "ffmpeg"), None);
}

#[test]
fn cleanup_stale_partial_downloads_removes_only_part_files() {
    let tmp = tempdir().expect("tempdir");
    let keep_bin = tmp.path().join("ffmpeg");
    let stale_part_1 = tmp.path().join("ffmpeg.part.12345.1");
    let stale_part_2 = tmp.path().join("ffprobe.part.99999.2");

    fs::write(&keep_bin, b"real-binary").expect("write ffmpeg");
    fs::write(&stale_part_1, b"partial-1").expect("write stale part 1");
    fs::write(&stale_part_2, b"partial-2").expect("write stale part 2");

    cleanup_stale_partial_downloads(tmp.path());

    assert!(keep_bin.is_file(), "installed binary must be preserved");
    assert!(!stale_part_1.exists(), "stale part 1 must be removed");
    assert!(!stale_part_2.exists(), "stale part 2 must be removed");

    // Non-existent directory should not panic
    cleanup_stale_partial_downloads(&tmp.path().join("does-not-exist"));
}

#[test]
fn download_tool_binary_streams_validates_and_installs_atomically() {
    let os = std::env::consts::OS;
    let payload = fake_valid_binary_for_os(os, 16 * 1024);

    let mut routes = HashMap::new();
    routes.insert(
        "/ffmpeg-valid".to_string(),
        MockResponse::binary(payload.clone()),
    );
    let server = MockHttpServer::from_routes(routes);

    let tmp = tempdir().expect("tempdir");
    let mut progress_events = Vec::new();
    let installed = download_tool_binary(
        "ffmpeg",
        &server.url("/ffmpeg-valid"),
        tmp.path(),
        os,
        4096,
        10.0,
        |pct| progress_events.push(pct),
    )
    .expect("download_tool_binary should succeed");

    let expected_path = tmp.path().join(binary_filename_for("ffmpeg", os));
    assert_eq!(installed, expected_path);
    assert!(installed.is_file());

    let disk_bytes = fs::read(&installed).expect("read installed binary");
    assert_eq!(disk_bytes.len(), 16 * 1024);
    assert_eq!(disk_bytes, payload);

    assert!(
        progress_events.contains(&100),
        "expected progress events to include 100, got {progress_events:?}"
    );
    assert_no_part_files(tmp.path());
    assert_is_executable(&installed);
}

#[test]
fn download_tool_binary_rejects_html_and_truncated_payloads_without_leaving_partials() {
    let os = std::env::consts::OS;
    let mut html_large = b"<html>not a binary</html>".to_vec();
    html_large.resize(8192, b' ');
    let truncated_valid = fake_valid_binary_for_os(os, 100);

    let mut routes = HashMap::new();
    routes.insert(
        "/html-200".to_string(),
        MockResponse::ok(html_large, "text/html"),
    );
    routes.insert(
        "/html-404".to_string(),
        MockResponse::with_status(404, b"<html><body>404 Not Found</body></html>", "text/html"),
    );
    routes.insert(
        "/truncated".to_string(),
        MockResponse::binary(truncated_valid),
    );
    let server = MockHttpServer::from_routes(routes);

    for path in ["/html-200", "/html-404", "/truncated"] {
        let tmp = tempdir().expect("tempdir");
        let res = download_tool_binary(
            "ffmpeg",
            &server.url(path),
            tmp.path(),
            os,
            4096,
            10.0,
            |_| {},
        );
        assert!(res.is_err(), "expected {path} to be rejected, got {res:?}");
        assert!(
            !tmp.path().join(binary_filename_for("ffmpeg", os)).exists(),
            "target binary should not exist after rejecting {path}"
        );
        assert_no_part_files(tmp.path());
    }
}

#[test]
fn download_tool_binary_rejects_oversized_content_length_before_writing() {
    let os = std::env::consts::OS;
    let mut routes = HashMap::new();
    routes.insert(
        "/oversized".to_string(),
        MockResponse::with_declared_length(200, MAX_BINARY_BYTES + 1, Vec::new()),
    );
    let server = MockHttpServer::from_routes(routes);

    let tmp = tempdir().expect("tempdir");
    let err = download_tool_binary(
        "ffmpeg",
        &server.url("/oversized"),
        tmp.path(),
        os,
        4096,
        10.0,
        |_| {},
    )
    .expect_err("oversized Content-Length should be rejected");

    assert!(
        err.contains("Content-Length") || err.contains("limit"),
        "unexpected error for oversized Content-Length: {err}"
    );
    assert!(
        !tmp.path().join(binary_filename_for("ffmpeg", os)).exists(),
        "target binary must not exist after rejecting oversized Content-Length"
    );
    assert_no_part_files(tmp.path());
    assert_eq!(server.requests(), vec!["/oversized".to_string()]);
}

#[test]
fn download_tool_binary_offline_unreachable_port_fails_cleanly() {
    let port = unused_local_port();

    let tmp = tempdir().expect("tempdir");
    let os = std::env::consts::OS;
    let url = format!("http://127.0.0.1:{port}/ffmpeg");

    let res = download_tool_binary("ffmpeg", &url, tmp.path(), os, 4096, 2.0, |_| {});
    assert!(res.is_err(), "unreachable port should fail cleanly");
    assert!(!tmp.path().join(binary_filename_for("ffmpeg", os)).exists());
    assert_no_part_files(tmp.path());
}

#[test]
fn ffmpeg_manager_with_mock_server_downloads_both_tools_and_marks_ready() {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;

    let Some(_suffix) = platform_asset_suffix_for(os, arch) else {
        return;
    };
    let Some(ffmpeg_asset) = asset_name_for("ffmpeg", os, arch) else {
        return;
    };
    let Some(ffprobe_asset) = asset_name_for("ffprobe", os, arch) else {
        return;
    };

    let payload_size = (DEFAULT_MIN_BINARY_BYTES as usize + 24).max(1_048_600);
    let payload = fake_valid_binary_for_os(os, payload_size);

    let mut routes = HashMap::new();
    routes.insert(
        format!("/{ffmpeg_asset}"),
        MockResponse::binary(payload.clone()),
    );
    routes.insert(format!("/{ffprobe_asset}"), MockResponse::binary(payload));
    let server = MockHttpServer::from_routes(routes);

    let tmp = tempdir().expect("tempdir");
    let manager = FfmpegManager::with_dir(tmp.path().to_path_buf());
    let status = manager.ensure_binaries(Some(tmp.path()), Some(&server.base_url()), 15.0);

    assert_eq!(
        status.status, "ready",
        "expected ready status, got '{}' ({})",
        status.status, status.message
    );
    assert!(status.ffmpeg_available);
    assert!(status.ffprobe_available);

    let snap = manager.snapshot();
    assert_eq!(snap.status, "ready");
    assert!(snap.ffmpeg_available);
    assert!(snap.ffprobe_available);
    assert!(!manager.is_downloading());

    let has_env_ffmpeg = std::env::var_os("NOTFLIX_FFMPEG_PATH").is_some();
    let has_env_ffprobe = std::env::var_os("NOTFLIX_FFPROBE_PATH").is_some();

    if !has_env_ffmpeg && which::which("ffmpeg").is_err() {
        let ffmpeg_path = tmp.path().join(binary_filename_for("ffmpeg", os));
        assert!(
            ffmpeg_path.is_file(),
            "expected ffmpeg to be downloaded to {}",
            ffmpeg_path.display()
        );
    }
    if !has_env_ffprobe && which::which("ffprobe").is_err() {
        let ffprobe_path = tmp.path().join(binary_filename_for("ffprobe", os));
        assert!(
            ffprobe_path.is_file(),
            "expected ffprobe to be downloaded to {}",
            ffprobe_path.display()
        );
    }
    assert_no_part_files(tmp.path());
}
