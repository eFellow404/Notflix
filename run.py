#!/usr/bin/env python3
"""Launch the Notflix local TV show player server."""

from __future__ import annotations

import argparse
import os
import sys
from pathlib import Path
from typing import Optional

from notflix.server import NotflixHTTPServer
from notflix.storage import NotflixStore, default_db_path
from notflix.updater import (
    DEFAULT_FETCH_TIMEOUT_SEC,
    UpdateResult,
    auto_update_on_startup,
    check_for_updates,
    default_repo_dir,
)


def _auto_update_enabled(cli_disabled: bool) -> bool:
    if cli_disabled:
        return False
    if os.environ.get("NOTFLIX_SKIP_AUTO_UPDATE") == "1":
        return False
    env_flag = os.environ.get("NOTFLIX_AUTO_UPDATE", "1").strip().lower()
    return env_flag not in {"0", "false", "no", "off"}


def main() -> int:
    parser = argparse.ArgumentParser(description="Notflix — Local DVD-Rip TV Show Player")
    parser.add_argument("--host", default="127.0.0.1", help="Bind host (default: 127.0.0.1)")
    parser.add_argument("--port", type=int, default=8080, help="Bind port (default: 8080)")
    parser.add_argument(
        "--db",
        type=Path,
        default=None,
        help="Optional path to SQLite database file",
    )
    parser.add_argument("--quiet", action="store_true", help="Suppress HTTP access logs")
    parser.add_argument(
        "--no-auto-update",
        action="store_true",
        help="Skip automatic remote Git update check on startup",
    )
    parser.add_argument(
        "--update-timeout",
        type=float,
        default=DEFAULT_FETCH_TIMEOUT_SEC,
        help=f"Seconds to wait when contacting remote Git server (default: {DEFAULT_FETCH_TIMEOUT_SEC})",
    )
    parser.add_argument(
        "--check-update",
        action="store_true",
        help="Check remote Git repository for updates, print status, and exit",
    )
    args = parser.parse_args()

    repo_dir = default_repo_dir()
    resolved_db = args.db.expanduser().resolve() if args.db else default_db_path()

    if args.check_update:
        result = check_for_updates(repo_dir=repo_dir, timeout_sec=args.update_timeout)
        print(f"[{result.status}] {result.message}")
        return 0

    startup_update: Optional[UpdateResult] = None
    if _auto_update_enabled(args.no_auto_update):
        startup_update = auto_update_on_startup(
            repo_dir=repo_dir,
            db_path=resolved_db,
            timeout_sec=args.update_timeout,
        )
        if startup_update.updated:
            print(f"[updater] {startup_update.message}")
            if startup_update.backup_path:
                print(f"[updater] Database backup saved to: {startup_update.backup_path}")
            print("[updater] Restarting Notflix to load updated code...")
            os.environ["NOTFLIX_SKIP_AUTO_UPDATE"] = "1"
            try:
                os.execv(sys.executable, [sys.executable, *sys.argv])
            except OSError as exc:
                print(f"[updater] Could not auto-restart ({exc}); continuing with current process.")
        elif startup_update.status in {"offline", "dirty", "diverged", "error"}:
            print(f"[updater] {startup_update.message}")

    store = NotflixStore(db_path=resolved_db)
    server = NotflixHTTPServer(
        (args.host, args.port),
        store=store,
        quiet=args.quiet,
        repo_dir=repo_dir,
        initial_update_result=startup_update,
    )
    url = f"http://{args.host}:{args.port}"

    print(f"Notflix is running at {url}")
    print(f"Database: {store.db_path}")
    print("Press Ctrl+C to stop.")

    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\nShutting down Notflix...")
    finally:
        server.server_close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
