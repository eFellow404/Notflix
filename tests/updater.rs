//! Integration tests for the offline-safe automatic Git updater and SQLite DB backups.

mod common;

use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::{
    assert_is_executable, create_show_folder, fake_valid_binary_for_os, unused_local_port,
    MockHttpServer, MockResponse,
};
use notflix::storage::NotflixStore;
use notflix::updater::{
    apply_release_update_to_path, apply_update, auto_update_on_startup, backup_database,
    check_for_updates, check_release_updates_from_url, get_local_repo_info, release_asset_name,
    release_matches_build, validate_binary_magic_for_os, DEFAULT_FETCH_TIMEOUT_SEC,
};
use rusqlite::Connection;
use tempfile::tempdir;

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("failed to execute git");
    assert!(
        output.status.success(),
        "git {:?} failed in {}:\nstdout: {}\nstderr: {}",
        args,
        cwd.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn create_bare_and_clone(base: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let remote_dir = base.join("remote.git");
    let author_dir = base.join("author");
    let local_dir = base.join("local");
    fs::create_dir(&remote_dir).expect("create remote dir");
    fs::create_dir(&author_dir).expect("create author dir");

    git(&remote_dir, &["init", "--bare", "-b", "main"]);
    git(&author_dir, &["init", "-b", "main"]);
    git(&author_dir, &["config", "commit.gpgsign", "false"]);
    git(&author_dir, &["config", "core.autocrlf", "false"]);
    git(&author_dir, &["config", "user.name", "Test User"]);
    git(&author_dir, &["config", "user.email", "test@example.com"]);
    fs::write(author_dir.join("app.txt"), "v1\n").expect("write app.txt");
    git(&author_dir, &["add", "app.txt"]);
    git(&author_dir, &["commit", "-m", "Initial commit"]);
    git(
        &author_dir,
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
    );
    git(&author_dir, &["push", "-u", "origin", "main"]);

    git(
        base,
        &[
            "clone",
            "-c",
            "core.autocrlf=false",
            remote_dir.to_str().unwrap(),
            local_dir.to_str().unwrap(),
        ],
    );
    git(&local_dir, &["config", "commit.gpgsign", "false"]);
    git(&local_dir, &["config", "core.autocrlf", "false"]);
    git(&local_dir, &["config", "user.name", "Test User"]);
    git(&local_dir, &["config", "user.email", "test@example.com"]);

    (remote_dir, author_dir, local_dir)
}

#[test]
fn check_and_apply_update_fast_forwards_and_preserves_user_db() {
    let tmp = tempdir().expect("tempdir");
    let base = tmp.path();
    let (_, author_dir, local_dir) = create_bare_and_clone(base);

    // Create realistic Notflix user database (inside repo to prove untracked DB is never wiped!)
    let db_path = local_dir.join("user_notflix.db");
    let show_dir = base.join("MyShow");
    create_show_folder(&show_dir, &["S01E01 - Pilot.mp4", "S01E02 - Next.mp4"]);

    let store = NotflixStore::new(Some(db_path.clone())).expect("create store");
    let (show, ll) = store
        .add_show(show_dir.to_str().unwrap(), None)
        .expect("add show");
    let head = ll.head().expect("head episode");
    store
        .update_progress(&head.id, Some(123.5), Some(1800.0), None, None, false)
        .expect("update progress");

    // Initially up to date
    let res_initial = check_for_updates(Some(&local_dir), DEFAULT_FETCH_TIMEOUT_SEC);
    assert_eq!(res_initial.status, "up_to_date");
    assert_eq!(res_initial.behind_count, 0);

    // Push 2 new commits to remote
    fs::write(author_dir.join("app.txt"), "v2\n").expect("write v2");
    git(&author_dir, &["add", "app.txt"]);
    git(&author_dir, &["commit", "-m", "Update 1"]);
    fs::write(author_dir.join("feature.txt"), "new feature\n").expect("write feature");
    git(&author_dir, &["add", "feature.txt"]);
    git(&author_dir, &["commit", "-m", "Update 2"]);
    git(&author_dir, &["push", "origin", "main"]);

    // Check detects 2 commits behind
    let res_check = check_for_updates(Some(&local_dir), DEFAULT_FETCH_TIMEOUT_SEC);
    assert_eq!(res_check.status, "update_available");
    assert_eq!(res_check.behind_count, 2);
    assert_eq!(res_check.ahead_count, 0);

    // Apply update backs up DB and fast-forwards code
    let res_apply =
        auto_update_on_startup(Some(&local_dir), Some(&db_path), DEFAULT_FETCH_TIMEOUT_SEC);
    assert_eq!(res_apply.status, "updated");
    assert!(res_apply.updated);
    assert!(res_apply.restart_required);
    let backup_path_str = res_apply.backup_path.expect("backup_path present");
    let backup_file = PathBuf::from(&backup_path_str);
    assert!(backup_file.is_file());

    // Code updated cleanly (normalize CRLF in case host Git overrides autocrlf)
    assert_eq!(
        fs::read_to_string(local_dir.join("app.txt"))
            .expect("read app.txt")
            .replace("\r\n", "\n"),
        "v2\n"
    );
    assert_eq!(
        fs::read_to_string(local_dir.join("feature.txt"))
            .expect("read feature.txt")
            .replace("\r\n", "\n"),
        "new feature\n"
    );

    // User database AND backup both preserve exact show + playback timeplace
    for check_db in [&db_path, &backup_file] {
        let reloaded_store = NotflixStore::new(Some(check_db.clone())).expect("reopen db");
        let (_, reloaded_ll) = reloaded_store
            .get_show(&show.id)
            .expect("get_show")
            .expect("show exists");
        let reloaded_head = reloaded_ll.head().expect("head exists");
        assert!((reloaded_head.position_sec - 123.5).abs() < 1e-6);
    }
}

#[test]
fn offline_unreachable_remote_never_crashes() {
    let tmp = tempdir().expect("tempdir");
    let (_, _, local_dir) = create_bare_and_clone(tmp.path());

    // Point origin at an unreachable local port
    git(
        &local_dir,
        &[
            "remote",
            "set-url",
            "origin",
            "http://127.0.0.1:1/unreachable.git",
        ],
    );

    let res_check = check_for_updates(Some(&local_dir), 2.0);
    assert_eq!(res_check.status, "offline");
    assert!(!res_check.updated);

    let res_apply = auto_update_on_startup(Some(&local_dir), None, 2.0);
    assert_eq!(res_apply.status, "offline");
    assert!(!res_apply.updated);
}

#[test]
fn timeout_returns_offline_safely() {
    let tmp = tempdir().expect("tempdir");
    let (_, _, local_dir) = create_bare_and_clone(tmp.path());

    // Start a local TCP tarpit that accepts connections and never replies so git fetch times out.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind tcp listener");
    let port = listener.local_addr().expect("local addr").port();
    listener.set_nonblocking(true).expect("set nonblocking");

    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);
    let server_thread = std::thread::spawn(move || {
        let mut held = Vec::new();
        while !stop_flag.load(Ordering::Relaxed) {
            if let Ok((stream, _)) = listener.accept() {
                held.push(stream);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(held);
    });

    let slow_url = format!("http://127.0.0.1:{port}/slow.git");
    git(&local_dir, &["remote", "set-url", "origin", &slow_url]);

    let res = check_for_updates(Some(&local_dir), 0.4);
    stop.store(true, Ordering::Relaxed);
    let _ = server_thread.join();

    assert_eq!(res.status, "offline");
    assert!(!res.updated);
}

#[test]
fn dirty_working_tree_skips_update_without_overwriting_edits() {
    let tmp = tempdir().expect("tempdir");
    let (_, author_dir, local_dir) = create_bare_and_clone(tmp.path());

    fs::write(author_dir.join("app.txt"), "remote-v2\n").expect("write remote change");
    git(&author_dir, &["add", "app.txt"]);
    git(&author_dir, &["commit", "-m", "Remote change"]);
    git(&author_dir, &["push", "origin", "main"]);

    // User has local uncommitted edit on tracked file
    fs::write(local_dir.join("app.txt"), "my-local-edit\n").expect("write local edit");

    let res = apply_update(Some(&local_dir), None, DEFAULT_FETCH_TIMEOUT_SEC);
    assert_eq!(res.status, "dirty");
    assert!(!res.updated);
    assert_eq!(
        fs::read_to_string(local_dir.join("app.txt")).expect("read app.txt"),
        "my-local-edit\n"
    );
}

#[test]
fn diverged_history_skips_update_safely() {
    let tmp = tempdir().expect("tempdir");
    let (_, author_dir, local_dir) = create_bare_and_clone(tmp.path());

    fs::write(author_dir.join("remote.txt"), "remote\n").expect("write remote");
    git(&author_dir, &["add", "remote.txt"]);
    git(&author_dir, &["commit", "-m", "Remote commit"]);
    git(&author_dir, &["push", "origin", "main"]);

    fs::write(local_dir.join("local.txt"), "local\n").expect("write local");
    git(&local_dir, &["add", "local.txt"]);
    git(&local_dir, &["commit", "-m", "Local unpushed commit"]);

    let res = apply_update(Some(&local_dir), None, DEFAULT_FETCH_TIMEOUT_SEC);
    assert_eq!(res.status, "diverged");
    assert_eq!(res.ahead_count, 1);
    assert_eq!(res.behind_count, 1);
    assert!(!res.updated);
}

#[test]
fn non_git_directory_reports_not_a_repo() {
    let tmp = tempdir().expect("tempdir");
    let plain_dir = tmp.path();

    let res_plain = check_for_updates(Some(plain_dir), DEFAULT_FETCH_TIMEOUT_SEC);
    assert_eq!(res_plain.status, "not_a_repo");
    assert_eq!(get_local_repo_info(Some(plain_dir)).status, "not_a_repo");
}

#[test]
fn database_backup_prunes_old_snapshots() {
    let tmp = tempdir().expect("tempdir");
    let db_path = tmp.path().join("notflix.db");
    {
        let conn = Connection::open(&db_path).expect("open db");
        conn.execute("CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT)", [])
            .expect("create table");
        conn.execute("INSERT INTO items (name) VALUES ('saved-progress')", [])
            .expect("insert row");
    }

    let mut created = Vec::new();
    for idx in 0..5 {
        let label = format!("step{idx}");
        let backup = backup_database(&db_path, &label, 3)
            .expect("backup_database")
            .expect("backup path");
        created.push(backup);
    }

    let backup_files: Vec<_> = fs::read_dir(tmp.path().join("backups"))
        .expect("read backups dir")
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("notflix-") && n.ends_with(".db"))
        })
        .collect();
    assert_eq!(backup_files.len(), 3);

    // Latest backup is valid SQLite with preserved row
    let latest = created.last().expect("latest backup");
    let conn = Connection::open(latest).expect("open latest backup");
    let name: String = conn
        .query_row("SELECT name FROM items", [], |row| row.get(0))
        .expect("query preserved row");
    assert_eq!(name, "saved-progress");
}

#[test]
fn validate_binary_magic_across_platforms() {
    // Linux ELF
    assert!(validate_binary_magic_for_os(
        b"\x7fELF\x02\x01\x01\x00",
        "linux"
    ));
    assert!(!validate_binary_magic_for_os(b"MZ\x90\x00", "linux"));
    assert!(!validate_binary_magic_for_os(b"\x7fEL", "linux"));
    assert!(!validate_binary_magic_for_os(
        b"<html>not elf</html>",
        "linux"
    ));

    // Windows PE/MZ
    assert!(validate_binary_magic_for_os(
        b"MZ\x90\x00\x03\x00",
        "windows"
    ));
    assert!(!validate_binary_magic_for_os(b"\x7fELF\x02\x01", "windows"));
    assert!(!validate_binary_magic_for_os(b"MZ\x90", "windows"));

    // macOS Mach-O (32-bit, 64-bit, universal, big & little endian)
    for magic in [
        [0xfe, 0xed, 0xfa, 0xce],
        [0xce, 0xfa, 0xed, 0xfe],
        [0xfe, 0xed, 0xfa, 0xcf],
        [0xcf, 0xfa, 0xed, 0xfe],
        [0xca, 0xfe, 0xba, 0xbe],
        [0xbe, 0xba, 0xfe, 0xca],
    ] {
        let mut payload = magic.to_vec();
        payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        assert!(validate_binary_magic_for_os(&payload, "macos"));
    }
    assert!(!validate_binary_magic_for_os(b"\x7fELF\x02\x01", "macos"));
    assert!(!validate_binary_magic_for_os(b"\xcf\xfa\xed", "macos"));

    // Unknown OS
    assert!(!validate_binary_magic_for_os(b"\x7fELF\x02\x01", "freebsd"));
}

#[test]
fn release_matches_build_checks_tag_and_commitish() {
    assert!(release_matches_build(
        "build-20261127-023000-4952bb1",
        "main",
        "4952bb1"
    ));
    assert!(release_matches_build(
        "build-20261127-023000-4952BB1",
        "main",
        "4952bb1"
    ));
    assert!(release_matches_build("4952bb1", "main", "4952bb1"));
    assert!(release_matches_build(
        "v1.2.3",
        "4952bb1234567890abcdef1234567890abcdef12",
        "4952bb1"
    ));

    // Different SHA
    assert!(!release_matches_build(
        "build-20261127-023000-abcdef0",
        "main",
        "4952bb1"
    ));
    assert!(!release_matches_build("v1.2.3", "main", "4952bb1"));

    // Dev or empty build SHA never matches
    assert!(!release_matches_build(
        "build-20261127-023000-4952bb1",
        "4952bb1234567890",
        "dev"
    ));
    assert!(!release_matches_build("dev", "dev", "dev"));
    assert!(!release_matches_build(
        "build-20261127-023000-4952bb1",
        "main",
        ""
    ));
}

#[test]
fn check_and_apply_release_update_with_mock_server() {
    let Some(asset_name) = release_asset_name() else {
        return;
    };

    let expected_binary = fake_valid_binary_for_os(std::env::consts::OS, 64);
    let tag = "build-20261127-023000-4952bb1";
    let tag_owned = tag.to_string();
    let asset_owned = asset_name.to_string();
    let binary_for_server = expected_binary.clone();
    let server = MockHttpServer::start_with_base(move |path, base| match path {
        "/releases/latest" => MockResponse::ok(
            serde_json::json!({
                "tag_name": tag_owned,
                "name": format!("Notflix {tag_owned}"),
                "target_commitish": "main",
                "assets": [{
                    "name": asset_owned,
                    "browser_download_url": format!("{base}/download/valid")
                }]
            })
            .to_string(),
            "application/json",
        ),
        "/releases/corrupt" => MockResponse::ok(
            serde_json::json!({
                "tag_name": tag_owned,
                "name": format!("Notflix {tag_owned}"),
                "target_commitish": "main",
                "assets": [{
                    "name": asset_owned,
                    "browser_download_url": format!("{base}/download/corrupt")
                }]
            })
            .to_string(),
            "application/json",
        ),
        "/download/valid" => MockResponse::binary(binary_for_server.clone()),
        "/download/corrupt" => {
            MockResponse::ok(b"<html><body>corrupted download</body></html>", "text/html")
        }
        _ => MockResponse::with_status(404, b"not found", "text/plain"),
    });
    let latest_url = server.url("/releases/latest");
    let corrupt_url = server.url("/releases/corrupt");

    // 1. Older build sees update_available
    let check_avail = check_release_updates_from_url(&latest_url, "1111111", 5.0);
    assert_eq!(check_avail.status, "update_available");
    assert_eq!(check_avail.behind_count, 1);
    assert_eq!(check_avail.branch.as_deref(), Some("release"));
    assert_eq!(check_avail.remote_sha.as_deref(), Some(tag));
    assert_eq!(check_avail.remote_short_sha.as_deref(), Some("4952bb1"));
    assert!(!check_avail.updated);

    // 2. Matching build SHA sees up_to_date
    let check_current = check_release_updates_from_url(&latest_url, "4952bb1", 5.0);
    assert_eq!(check_current.status, "up_to_date");
    assert_eq!(check_current.behind_count, 0);
    assert!(!check_current.updated);

    // 3. Apply valid release update: backs up DB and atomically replaces binary
    let tmp = tempdir().expect("tempdir");
    let target_exe = tmp.path().join("notflix-bin");
    let original_bytes = b"ORIGINAL-BINARY-V1".to_vec();
    fs::write(&target_exe, &original_bytes).expect("write original binary");

    let db_path = tmp.path().join("notflix.db");
    {
        let conn = Connection::open(&db_path).expect("open db");
        conn.execute("CREATE TABLE state (k TEXT, v TEXT)", [])
            .expect("create table");
        conn.execute("INSERT INTO state VALUES ('ep', 'S01E03')", [])
            .expect("insert row");
    }

    let applied =
        apply_release_update_to_path(&latest_url, &target_exe, "1111111", Some(&db_path), 5.0);
    assert_eq!(applied.status, "updated");
    assert!(applied.updated);
    assert!(applied.restart_required);
    assert_eq!(applied.branch.as_deref(), Some("release"));
    assert_eq!(applied.remote_sha.as_deref(), Some(tag));

    let backup_path = PathBuf::from(applied.backup_path.expect("backup_path present"));
    assert!(backup_path.is_file());
    let backup_conn = Connection::open(&backup_path).expect("open backup db");
    let preserved: String = backup_conn
        .query_row("SELECT v FROM state WHERE k = 'ep'", [], |row| row.get(0))
        .expect("query backup row");
    assert_eq!(preserved, "S01E03");

    let replaced_bytes = fs::read(&target_exe).expect("read updated binary");
    assert_eq!(replaced_bytes, expected_binary);
    assert_is_executable(&target_exe);

    // 4. Corrupt / non-magic payload is rejected safely without modifying target_exe
    let safe_exe = tmp.path().join("notflix-safe-bin");
    fs::write(&safe_exe, &original_bytes).expect("write safe binary");
    let rejected = apply_release_update_to_path(&corrupt_url, &safe_exe, "1111111", None, 5.0);
    assert_eq!(rejected.status, "error");
    assert!(!rejected.updated);
    assert!(rejected.message.contains("integrity"));
    assert_eq!(
        fs::read(&safe_exe).expect("read untouched binary"),
        original_bytes
    );
}

#[test]
fn check_release_updates_unreachable_port_returns_offline() {
    let port = unused_local_port();

    let url = format!("http://127.0.0.1:{port}/releases/latest");
    let res = check_release_updates_from_url(&url, "1111111", 2.0);
    assert_eq!(res.status, "offline");
    assert!(!res.updated);
}
