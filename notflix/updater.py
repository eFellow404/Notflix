"""Offline-safe automatic Git updater with pre-update SQLite backups.

Checks the configured Git remote for newer commits and applies fast-forward
updates cleanly without ever crashing the app when offline or touching user data.
"""

from __future__ import annotations

import os
import shutil
import sqlite3
import subprocess
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

from notflix import __version__

DEFAULT_FETCH_TIMEOUT_SEC = 5.0
DEFAULT_BACKUP_KEEP = 5

_NETWORK_ERROR_MARKERS = (
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
)


def default_repo_dir() -> Path:
    return Path(__file__).resolve().parent.parent


def _utc_now() -> str:
    return datetime.now(timezone.utc).isoformat()


def _short_sha(sha: Optional[str]) -> Optional[str]:
    return sha[:7] if sha else None


@dataclass(frozen=True)
class UpdateResult:
    """Structured outcome of an update status check or update attempt."""

    status: str  # up_to_date, update_available, updated, offline, dirty, diverged, not_a_repo, no_git, no_remote, error
    message: str
    branch: Optional[str] = None
    remote: Optional[str] = None
    local_sha: Optional[str] = None
    remote_sha: Optional[str] = None
    old_sha: Optional[str] = None
    behind_count: int = 0
    ahead_count: int = 0
    updated: bool = False
    restart_required: bool = False
    backup_path: Optional[str] = None
    checked_at: str = ""
    version: str = __version__

    def to_dict(self) -> dict:
        return {
            "status": self.status,
            "message": self.message,
            "branch": self.branch,
            "remote": self.remote,
            "local_sha": self.local_sha,
            "local_short_sha": _short_sha(self.local_sha),
            "remote_sha": self.remote_sha,
            "remote_short_sha": _short_sha(self.remote_sha),
            "old_sha": self.old_sha,
            "old_short_sha": _short_sha(self.old_sha),
            "behind_count": self.behind_count,
            "ahead_count": self.ahead_count,
            "updated": self.updated,
            "restart_required": self.restart_required,
            "backup_path": self.backup_path,
            "checked_at": self.checked_at or _utc_now(),
            "version": self.version,
        }


def _git_env() -> dict[str, str]:
    env = os.environ.copy()
    env["GIT_TERMINAL_PROMPT"] = "0"
    env["GIT_ASKPASS"] = ""
    env["LC_ALL"] = "C"
    return env


def _run_git(
    repo_dir: Path,
    args: list[str],
    timeout_sec: float = DEFAULT_FETCH_TIMEOUT_SEC,
) -> tuple[int, str, str, bool]:
    """Run a non-interactive git command safely.

    Returns ``(returncode, stdout, stderr, timed_out)``. Never raises.
    """
    try:
        proc = subprocess.run(
            ["git", *args],
            cwd=str(repo_dir),
            capture_output=True,
            text=True,
            timeout=timeout_sec,
            env=_git_env(),
            check=False,
        )
        return proc.returncode, proc.stdout.strip(), proc.stderr.strip(), False
    except subprocess.TimeoutExpired:
        return -1, "", f"git {args[0]} timed out after {timeout_sec:.1f}s", True
    except (FileNotFoundError, OSError) as exc:
        return -1, "", str(exc), False


def _looks_like_network_error(stderr: str, timed_out: bool) -> bool:
    if timed_out:
        return True
    low = stderr.lower()
    return any(marker in low for marker in _NETWORK_ERROR_MARKERS)


def backup_database(
    db_path: Path,
    label: str = "pre-update",
    keep: int = DEFAULT_BACKUP_KEEP,
) -> Optional[Path]:
    """Create a consistent WAL-safe SQLite backup before applying an update.

    Returns ``None`` if ``db_path`` does not exist yet. Prunes older backups
    beyond ``keep`` entries.
    """
    resolved = Path(db_path).expanduser().resolve()
    if not resolved.is_file():
        return None

    backup_dir = resolved.parent / "backups"
    backup_dir.mkdir(parents=True, exist_ok=True)

    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    safe_label = "".join(ch for ch in label if ch.isalnum() or ch in ("-", "_"))[:24] or "snapshot"
    dest = backup_dir / f"{resolved.stem}-{safe_label}-{stamp}.db"
    suffix = 1
    while dest.exists():
        dest = backup_dir / f"{resolved.stem}-{safe_label}-{stamp}-{suffix}.db"
        suffix += 1

    src_conn: Optional[sqlite3.Connection] = None
    dst_conn: Optional[sqlite3.Connection] = None
    try:
        src_conn = sqlite3.connect(str(resolved), timeout=10.0)
        dst_conn = sqlite3.connect(str(dest), timeout=10.0)
        src_conn.backup(dst_conn)
    finally:
        if dst_conn is not None:
            dst_conn.close()
        if src_conn is not None:
            src_conn.close()

    # Retain only the newest `keep` backups for this database
    if keep > 0:
        existing = sorted(
            backup_dir.glob(f"{resolved.stem}-*.db"),
            key=lambda p: (p.stat().st_mtime, p.name),
            reverse=True,
        )
        for stale in existing[keep:]:
            try:
                stale.unlink()
            except OSError:
                pass

    return dest


def _resolve_repo_target(
    repo_dir: Path,
) -> tuple[Optional[tuple[str, str, str, str]], Optional[UpdateResult]]:
    """Inspect local Git state without hitting the network.

    Returns ``((branch, remote, remote_branch, local_sha), None)`` on success,
    or ``(None, UpdateResult)`` if git/repo/branch/remote cannot be used.
    """
    now = _utc_now()
    if shutil.which("git") is None:
        return None, UpdateResult(
            status="no_git",
            message="Git is not installed; automatic updates unavailable.",
            checked_at=now,
        )

    if not repo_dir.is_dir():
        return None, UpdateResult(
            status="not_a_repo",
            message="Application folder not found.",
            checked_at=now,
        )

    rc, out, _, _ = _run_git(repo_dir, ["rev-parse", "--is-inside-work-tree"], timeout_sec=3.0)
    if rc != 0 or out.lower() != "true":
        return None, UpdateResult(
            status="not_a_repo",
            message="Not a Git installation; automatic updates unavailable.",
            checked_at=now,
        )

    rc_sha, local_sha, _, _ = _run_git(repo_dir, ["rev-parse", "HEAD"], timeout_sec=3.0)
    local_sha = local_sha if rc_sha == 0 else None

    rc_br, branch, _, _ = _run_git(repo_dir, ["symbolic-ref", "--short", "HEAD"], timeout_sec=3.0)
    if rc_br != 0 or not branch:
        return None, UpdateResult(
            status="error",
            message="Detached HEAD; switch to a branch to enable automatic updates.",
            local_sha=local_sha,
            checked_at=now,
        )

    # Prefer configured upstream (@{u}), fall back to origin/<branch>
    remote: Optional[str] = None
    remote_branch: str = branch
    rc_up, upstream, _, _ = _run_git(
        repo_dir,
        ["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
        timeout_sec=3.0,
    )
    if rc_up == 0 and "/" in upstream:
        remote, remote_branch = upstream.split("/", 1)
    else:
        rc_rem, remotes_out, _, _ = _run_git(repo_dir, ["remote"], timeout_sec=3.0)
        remotes = [r.strip() for r in remotes_out.splitlines() if r.strip()] if rc_rem == 0 else []
        if "origin" in remotes:
            remote = "origin"
        elif remotes:
            remote = remotes[0]

    if not remote:
        return None, UpdateResult(
            status="no_remote",
            message="No Git remote configured for updates.",
            branch=branch,
            local_sha=local_sha,
            checked_at=now,
        )

    return (branch, remote, remote_branch, local_sha or ""), None


def get_local_repo_info(repo_dir: Optional[Path] = None) -> UpdateResult:
    """Return local commit/branch info without making any network requests."""
    root = (repo_dir or default_repo_dir()).resolve()
    target, err_res = _resolve_repo_target(root)
    if err_res is not None:
        return err_res
    assert target is not None
    branch, remote, _, local_sha = target
    return UpdateResult(
        status="idle",
        message=f"Running {branch} ({_short_sha(local_sha) or 'unknown'})",
        branch=branch,
        remote=remote,
        local_sha=local_sha or None,
        checked_at=_utc_now(),
    )


def check_for_updates(
    repo_dir: Optional[Path] = None,
    timeout_sec: float = DEFAULT_FETCH_TIMEOUT_SEC,
) -> UpdateResult:
    """Fetch remote branch metadata and report whether local repo is behind.

    Guaranteed never to raise even when offline, timed out, or misconfigured.
    """
    now = _utc_now()
    try:
        root = (repo_dir or default_repo_dir()).resolve()
        target, err_res = _resolve_repo_target(root)
        if err_res is not None:
            return err_res
        assert target is not None
        branch, remote, remote_branch, local_sha = target

        rc_fetch, _, fetch_err, timed_out = _run_git(
            root,
            ["fetch", "--quiet", remote, remote_branch],
            timeout_sec=timeout_sec,
        )
        if rc_fetch != 0:
            if _looks_like_network_error(fetch_err, timed_out):
                return UpdateResult(
                    status="offline",
                    message="Could not reach update server; continuing offline.",
                    branch=branch,
                    remote=remote,
                    local_sha=local_sha or None,
                    checked_at=now,
                )
            return UpdateResult(
                status="error",
                message=f"Update check failed: {fetch_err or 'git fetch returned an error'}",
                branch=branch,
                remote=remote,
                local_sha=local_sha or None,
                checked_at=now,
            )

        remote_ref = f"refs/remotes/{remote}/{remote_branch}"
        rc_rsha, remote_sha, _, _ = _run_git(root, ["rev-parse", remote_ref], timeout_sec=3.0)
        if rc_rsha != 0 or not remote_sha:
            # Fallback to FETCH_HEAD when refspec didn't update refs/remotes/
            rc_rsha, remote_sha, _, _ = _run_git(root, ["rev-parse", "FETCH_HEAD"], timeout_sec=3.0)
            remote_ref = "FETCH_HEAD"
        if rc_rsha != 0 or not remote_sha:
            return UpdateResult(
                status="error",
                message="Could not determine remote commit.",
                branch=branch,
                remote=remote,
                local_sha=local_sha or None,
                checked_at=now,
            )

        rc_cnt, cnt_out, _, _ = _run_git(
            root,
            ["rev-list", "--left-right", "--count", f"HEAD...{remote_ref}"],
            timeout_sec=3.0,
        )
        ahead, behind = 0, 0
        if rc_cnt == 0 and cnt_out:
            parts = cnt_out.split()
            if len(parts) == 2:
                ahead, behind = int(parts[0]), int(parts[1])

        if behind == 0:
            return UpdateResult(
                status="up_to_date",
                message=f"Notflix is up to date ({_short_sha(local_sha)}).",
                branch=branch,
                remote=remote,
                local_sha=local_sha or None,
                remote_sha=remote_sha,
                behind_count=0,
                ahead_count=ahead,
                checked_at=now,
            )

        if ahead > 0:
            return UpdateResult(
                status="diverged",
                message=(
                    f"Local branch has {ahead} unpushed commit(s) and is {behind} behind "
                    f"{remote}/{remote_branch}; skipping automatic merge."
                ),
                branch=branch,
                remote=remote,
                local_sha=local_sha or None,
                remote_sha=remote_sha,
                behind_count=behind,
                ahead_count=ahead,
                checked_at=now,
            )

        noun = "commit" if behind == 1 else "commits"
        return UpdateResult(
            status="update_available",
            message=f"Update available: {behind} new {noun} on {remote}/{remote_branch}.",
            branch=branch,
            remote=remote,
            local_sha=local_sha or None,
            remote_sha=remote_sha,
            behind_count=behind,
            ahead_count=0,
            checked_at=now,
        )
    except Exception as exc:  # noqa: BLE001 — updater must never crash caller
        return UpdateResult(
            status="error",
            message=f"Unexpected update check error: {exc}",
            checked_at=now,
        )


def _has_dirty_tracked_files(repo_dir: Path) -> bool:
    rc, out, _, _ = _run_git(
        repo_dir,
        ["status", "--porcelain", "--untracked-files=no"],
        timeout_sec=3.0,
    )
    return rc != 0 or bool(out.strip())


def apply_update(
    repo_dir: Optional[Path] = None,
    db_path: Optional[Path] = None,
    timeout_sec: float = DEFAULT_FETCH_TIMEOUT_SEC,
) -> UpdateResult:
    """Safely back up user data and fast-forward the repository if behind remote.

    Never overwrites uncommitted tracked edits, never touches untracked user
    files, and never raises if the remote is unreachable.
    """
    now = _utc_now()
    try:
        root = (repo_dir or default_repo_dir()).resolve()
        check = check_for_updates(root, timeout_sec=timeout_sec)
        if check.status != "update_available":
            return check

        if _has_dirty_tracked_files(root):
            return UpdateResult(
                status="dirty",
                message="Local uncommitted changes detected; skipping automatic update to protect your edits.",
                branch=check.branch,
                remote=check.remote,
                local_sha=check.local_sha,
                remote_sha=check.remote_sha,
                behind_count=check.behind_count,
                ahead_count=check.ahead_count,
                checked_at=now,
            )

        # Snapshot existing SQLite database before touching code files
        backup_str: Optional[str] = None
        if db_path is not None:
            try:
                backup = backup_database(
                    db_path,
                    label=f"pre-{_short_sha(check.local_sha) or 'update'}",
                )
                if backup is not None:
                    backup_str = str(backup)
            except (sqlite3.Error, OSError) as exc:
                return UpdateResult(
                    status="error",
                    message=f"Aborted update because database backup failed: {exc}",
                    branch=check.branch,
                    remote=check.remote,
                    local_sha=check.local_sha,
                    remote_sha=check.remote_sha,
                    behind_count=check.behind_count,
                    checked_at=now,
                )

        target_ref = check.remote_sha or "FETCH_HEAD"
        rc_merge, _, merge_err, _ = _run_git(
            root,
            ["merge", "--ff-only", target_ref],
            timeout_sec=max(10.0, timeout_sec),
        )
        if rc_merge != 0:
            return UpdateResult(
                status="error",
                message=f"Fast-forward update failed safely: {merge_err or 'git merge failed'}",
                branch=check.branch,
                remote=check.remote,
                local_sha=check.local_sha,
                remote_sha=check.remote_sha,
                behind_count=check.behind_count,
                backup_path=backup_str,
                checked_at=now,
            )

        rc_new, new_sha, _, _ = _run_git(root, ["rev-parse", "HEAD"], timeout_sec=3.0)
        final_sha = new_sha if rc_new == 0 and new_sha else check.remote_sha

        return UpdateResult(
            status="updated",
            message=(
                f"Updated Notflix from {_short_sha(check.local_sha)} "
                f"to {_short_sha(final_sha)} ({check.behind_count} commit"
                f"{'' if check.behind_count == 1 else 's'})."
            ),
            branch=check.branch,
            remote=check.remote,
            local_sha=final_sha,
            remote_sha=check.remote_sha,
            old_sha=check.local_sha,
            behind_count=0,
            ahead_count=0,
            updated=True,
            restart_required=True,
            backup_path=backup_str,
            checked_at=now,
        )
    except Exception as exc:  # noqa: BLE001 — updater must never crash caller
        return UpdateResult(
            status="error",
            message=f"Unexpected update error: {exc}",
            checked_at=now,
        )


def auto_update_on_startup(
    repo_dir: Optional[Path] = None,
    db_path: Optional[Path] = None,
    timeout_sec: float = DEFAULT_FETCH_TIMEOUT_SEC,
) -> UpdateResult:
    """Fail-safe startup entry point: check and apply fast-forward updates."""
    return apply_update(repo_dir=repo_dir, db_path=db_path, timeout_sec=timeout_sec)
