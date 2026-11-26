#!/usr/bin/env python3
"""Launch the Notflix local TV show player server."""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

from notflix.server import NotflixHTTPServer
from notflix.storage import NotflixStore


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
    args = parser.parse_args()

    store = NotflixStore(db_path=args.db)
    server = NotflixHTTPServer((args.host, args.port), store=store, quiet=args.quiet)
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
