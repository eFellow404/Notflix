"""Unit tests for the offline-safe automatic Git updater and SQLite DB backups."""

from __future__ import annotations

import shutil
import sqlite3
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from notflix.storage import NotflixStore
from notflix.updater import (
    apply_update,
    auto_update_on_startup,
    backup_database,
    check_for_updates,
    get_local_repo_info,
)


def _git(cwd: Path, *args: str) -> str:
    env = {
        "PATH": "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        "GIT_AUTHOR_NAME": "Test User",
        "GIT_AUTHOR_EMAIL": "test@example.com",
        "GIT_COMMITTER_NAME": "Test User",
        "GIT_COMMITTER_EMAIL": "test@example.com",
        "GIT_TERMINAL_PROMPT": "0",
    }
    proc = subprocess.run(
        ["git", *args],
        cwd=str(cwd),
        capture_output=True,
        text=True,
        env=env,
        check=True,
    )
    return proc.stdout.strip()


def _create_bare_and_clone(base: Path) -> tuple[Path, Path, Path]:
    """Create a bare remote repo, an upstream author clone, and a local user clone."""
    remote_dir = base / "remote.git"
    author_dir = base / "author"
    local_dir = base / "local"
    remote_dir.mkdir()
    author_dir.mkdir()

    _git(remote_dir, "init", "--bare", "-b", "main")
    _git(author_dir, "init", "-b", "main")
    (author_dir / "app.txt").write_text("v1\n", encoding="utf-8")
    _git(author_dir, "add", "app.txt")
    _git(author_dir, "commit", "-m", "Initial commit")
    _git(author_dir, "remote", "add", "origin", str(remote_dir))
    _git(author_dir, "push", "-u", "origin", "main")

    _git(base, "clone", str(remote_dir), str(local_dir))
    return remote_dir, author_dir, local_dir


class TestUpdater(unittest.TestCase):
    def test_check_and_apply_update_fast_forwards_and_preserves_user_db(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            _, author_dir, local_dir = _create_bare_and_clone(base)

            # Create realistic Notflix user database (inside repo to prove untracked DB is never wiped!)
            db_path = local_dir / "user_notflix.db"
            show_dir = base / "MyShow"
            show_dir.mkdir()
            (show_dir / "S01E01 - Pilot.mp4").write_bytes(b"video1")
            (show_dir / "S01E02 - Next.mp4").write_bytes(b"video2")

            store = NotflixStore(db_path=db_path)
            show, ll = store.add_show(str(show_dir))
            assert ll.head is not None
            store.update_progress(ll.head.id, position_sec=123.5, duration_sec=1800.0)

            # Initially up to date
            res_initial = check_for_updates(local_dir)
            self.assertEqual(res_initial.status, "up_to_date")
            self.assertEqual(res_initial.behind_count, 0)

            # Push 2 new commits to remote
            (author_dir / "app.txt").write_text("v2\n", encoding="utf-8")
            _git(author_dir, "add", "app.txt")
            _git(author_dir, "commit", "-m", "Update 1")
            (author_dir / "feature.txt").write_text("new feature\n", encoding="utf-8")
            _git(author_dir, "add", "feature.txt")
            _git(author_dir, "commit", "-m", "Update 2")
            _git(author_dir, "push", "origin", "main")

            # Check detects 2 commits behind
            res_check = check_for_updates(local_dir)
            self.assertEqual(res_check.status, "update_available")
            self.assertEqual(res_check.behind_count, 2)
            self.assertEqual(res_check.ahead_count, 0)

            # Apply update backs up DB and fast-forwards code
            res_apply = auto_update_on_startup(repo_dir=local_dir, db_path=db_path)
            self.assertEqual(res_apply.status, "updated")
            self.assertTrue(res_apply.updated)
            self.assertTrue(res_apply.restart_required)
            self.assertIsNotNone(res_apply.backup_path)
            backup_file = Path(res_apply.backup_path)  # type: ignore[arg-type]
            self.assertTrue(backup_file.is_file())

            # Code updated cleanly
            self.assertEqual((local_dir / "app.txt").read_text(encoding="utf-8"), "v2\n")
            self.assertEqual((local_dir / "feature.txt").read_text(encoding="utf-8"), "new feature\n")

            # User database AND backup both preserve exact show + playback timeplace
            for check_db in (db_path, backup_file):
                reloaded_store = NotflixStore(db_path=check_db)
                loaded = reloaded_store.get_show(show["id"])
                assert loaded is not None
                _, reloaded_ll = loaded
                assert reloaded_ll.head is not None
                self.assertAlmostEqual(reloaded_ll.head.position_sec, 123.5)

    def test_offline_unreachable_remote_never_crashes(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            _, _, local_dir = _create_bare_and_clone(base)
            # Point origin at an unreachable nonexistent host / port
            _git(local_dir, "remote", "set-url", "origin", "http://127.0.0.1:1/unreachable.git")

            res_check = check_for_updates(local_dir, timeout_sec=2.0)
            self.assertEqual(res_check.status, "offline")
            self.assertFalse(res_check.updated)

            res_apply = auto_update_on_startup(local_dir, timeout_sec=2.0)
            self.assertEqual(res_apply.status, "offline")
            self.assertFalse(res_apply.updated)

    def test_timeout_returns_offline_safely(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            _, _, local_dir = _create_bare_and_clone(base)

            real_run = subprocess.run

            def fake_run(cmd, *args, **kwargs):
                if len(cmd) >= 2 and cmd[0] == "git" and cmd[1] == "fetch":
                    raise subprocess.TimeoutExpired(cmd=cmd, timeout=1.0)
                return real_run(cmd, *args, **kwargs)

            with mock.patch("notflix.updater.subprocess.run", side_effect=fake_run):
                res = check_for_updates(local_dir, timeout_sec=1.0)
                self.assertEqual(res.status, "offline")

    def test_dirty_working_tree_skips_update_without_overwriting_edits(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            _, author_dir, local_dir = _create_bare_and_clone(base)

            (author_dir / "app.txt").write_text("remote-v2\n", encoding="utf-8")
            _git(author_dir, "add", "app.txt")
            _git(author_dir, "commit", "-m", "Remote change")
            _git(author_dir, "push", "origin", "main")

            # User has local uncommitted edit on tracked file
            (local_dir / "app.txt").write_text("my-local-edit\n", encoding="utf-8")

            res = apply_update(local_dir)
            self.assertEqual(res.status, "dirty")
            self.assertFalse(res.updated)
            self.assertEqual((local_dir / "app.txt").read_text(encoding="utf-8"), "my-local-edit\n")

    def test_diverged_history_skips_update_safely(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            _, author_dir, local_dir = _create_bare_and_clone(base)

            (author_dir / "remote.txt").write_text("remote\n", encoding="utf-8")
            _git(author_dir, "add", "remote.txt")
            _git(author_dir, "commit", "-m", "Remote commit")
            _git(author_dir, "push", "origin", "main")

            (local_dir / "local.txt").write_text("local\n", encoding="utf-8")
            _git(local_dir, "add", "local.txt")
            _git(local_dir, "commit", "-m", "Local unpushed commit")

            res = apply_update(local_dir)
            self.assertEqual(res.status, "diverged")
            self.assertEqual(res.ahead_count, 1)
            self.assertEqual(res.behind_count, 1)
            self.assertFalse(res.updated)

    def test_non_git_directory_and_missing_git_binary(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            plain_dir = Path(tmp)
            res_plain = check_for_updates(plain_dir)
            self.assertEqual(res_plain.status, "not_a_repo")
            self.assertEqual(get_local_repo_info(plain_dir).status, "not_a_repo")

            with mock.patch("notflix.updater.shutil.which", return_value=None):
                res_nogit = check_for_updates(plain_dir)
                self.assertEqual(res_nogit.status, "no_git")

    def test_database_backup_prunes_old_snapshots(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            db_path = Path(tmp) / "notflix.db"
            with sqlite3.connect(str(db_path)) as conn:
                conn.execute("CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT)")
                conn.execute("INSERT INTO items (name) VALUES ('saved-progress')")

            created = []
            for idx in range(5):
                b = backup_database(db_path, label=f"step{idx}", keep=3)
                assert b is not None
                created.append(b)

            backup_files = list((Path(tmp) / "backups").glob("notflix-*.db"))
            self.assertEqual(len(backup_files), 3)
            # Latest backup is valid SQLite with preserved row
            with sqlite3.connect(str(created[-1])) as conn:
                row = conn.execute("SELECT name FROM items").fetchone()
                self.assertEqual(row[0], "saved-progress")


if __name__ == "__main__":
    unittest.main()
