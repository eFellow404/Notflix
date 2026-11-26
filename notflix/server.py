"""Threaded HTTP server, REST API, and static frontend host for Notflix."""

from __future__ import annotations

import json
import mimetypes
import re
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Optional
from urllib.parse import parse_qs, unquote, urlparse

from notflix.intelligence import VIDEO_EXTENSIONS
from notflix.media import detect_ffmpeg, stream_video_file
from notflix.storage import NotflixStore

SHOW_DETAIL_RE = re.compile(r"^/api/shows/([^/]+)$")
SHOW_ACTION_RE = re.compile(r"^/api/shows/([^/]+)/(rescan|next|prev|select|reorder)$")
EPISODE_PROGRESS_RE = re.compile(r"^/api/episodes/([^/]+)/progress$")
EPISODE_STREAM_RE = re.compile(r"^/api/episodes/([^/]+)/stream$")


def default_static_dir() -> Path:
    return Path(__file__).resolve().parent.parent / "static"


class NotflixHTTPServer(ThreadingHTTPServer):
    """Multi-threaded HTTP server carrying shared NotflixStore state."""

    daemon_threads = True
    allow_reuse_address = True

    def __init__(
        self,
        server_address: tuple[str, int],
        store: NotflixStore,
        static_dir: Optional[Path] = None,
        quiet: bool = False,
    ) -> None:
        self.store = store
        self.static_dir = (static_dir or default_static_dir()).resolve()
        self.quiet = quiet
        super().__init__(server_address, NotflixRequestHandler)


class NotflixRequestHandler(BaseHTTPRequestHandler):
    """HTTP request handler serving the Notflix REST API, video streams, and SPA."""

    server: NotflixHTTPServer  # type: ignore[assignment]
    protocol_version = "HTTP/1.1"

    def log_message(self, format: str, *args: Any) -> None:  # noqa: A002
        if not self.server.quiet:
            super().log_message(format, *args)

    def _send_json(self, payload: Any, status: HTTPStatus = HTTPStatus.OK) -> None:
        body = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def _send_error_json(self, message: str, status: HTTPStatus) -> None:
        self._send_json({"error": message}, status=status)

    def _read_json_body(self) -> dict[str, Any]:
        raw_len = self.headers.get("Content-Length", "0")
        try:
            length = int(raw_len)
        except ValueError:
            raise ValueError("Invalid Content-Length")
        if length <= 0:
            return {}
        if length > 2 * 1024 * 1024:
            raise ValueError("Request body too large")
        raw = self.rfile.read(length)
        if not raw.strip():
            return {}
        data = json.loads(raw.decode("utf-8"))
        if not isinstance(data, dict):
            raise ValueError("Expected JSON object")
        return data

    def _serve_static(self, rel_path: str, head_only: bool = False) -> None:
        static_root = self.server.static_dir
        clean = rel_path.lstrip("/") or "index.html"
        target = (static_root / clean).resolve()
        try:
            target.relative_to(static_root)
        except ValueError:
            self._send_error_json("Forbidden path", HTTPStatus.FORBIDDEN)
            return

        if not target.exists() or not target.is_file():
            # SPA fallback for non-API routes
            if not clean.startswith("api/"):
                target = static_root / "index.html"
            if not target.exists():
                self._send_error_json("Not found", HTTPStatus.NOT_FOUND)
                return

        mime, _ = mimetypes.guess_type(str(target))
        if target.suffix == ".js":
            mime = "application/javascript; charset=utf-8"
        elif target.suffix == ".css":
            mime = "text/css; charset=utf-8"
        elif target.suffix == ".html":
            mime = "text/html; charset=utf-8"
        elif target.suffix == ".svg":
            mime = "image/svg+xml"

        content = target.read_bytes()
        self.send_response(HTTPStatus.OK)
        self.send_header("Content-Type", mime or "application/octet-stream")
        self.send_header("Content-Length", str(len(content)))
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        if not head_only:
            self.wfile.write(content)

    def _handle_fs_browse(self, query: dict[str, list[str]]) -> None:
        raw_path = query.get("path", [""])[0].strip()
        target = Path(raw_path).expanduser() if raw_path else Path.home()
        try:
            resolved = target.resolve()
        except OSError as exc:
            self._send_error_json(f"Cannot resolve path: {exc}", HTTPStatus.BAD_REQUEST)
            return

        if not resolved.exists() or not resolved.is_dir():
            self._send_error_json(f"Directory not found: {resolved}", HTTPStatus.NOT_FOUND)
            return

        dirs: list[dict[str, Any]] = []
        direct_video_count = 0
        try:
            entries = sorted(resolved.iterdir(), key=lambda p: p.name.lower())
        except PermissionError:
            self._send_error_json("Permission denied reading directory", HTTPStatus.FORBIDDEN)
            return

        for entry in entries:
            if entry.name.startswith("."):
                continue
            try:
                if entry.is_dir():
                    dirs.append({"name": entry.name, "path": str(entry)})
                elif entry.is_file() and entry.suffix.lower() in VIDEO_EXTENSIONS:
                    direct_video_count += 1
            except OSError:
                continue

        parent = str(resolved.parent) if resolved.parent != resolved else None
        self._send_json(
            {
                "path": str(resolved),
                "parent": parent,
                "directories": dirs,
                "direct_video_count": direct_video_count,
            }
        )

    def do_HEAD(self) -> None:
        parsed = urlparse(self.path)
        path = unquote(parsed.path)

        m_stream = EPISODE_STREAM_RE.match(path)
        if m_stream:
            ep = self.server.store.get_episode(m_stream.group(1))
            if ep is None:
                self.send_error(HTTPStatus.NOT_FOUND, "Episode not found")
                return
            stream_video_file(self, Path(ep.file_path), head_only=True)
            return

        if path == "/" or path.startswith("/static/"):
            rel = "index.html" if path == "/" else path[len("/static/"):]
            self._serve_static(rel, head_only=True)
            return

        self.send_error(HTTPStatus.NOT_FOUND)

    def do_GET(self) -> None:
        parsed = urlparse(self.path)
        path = unquote(parsed.path)
        query = parse_qs(parsed.query)

        if path == "/api/health":
            self._send_json({"status": "ok", "ffmpeg_available": detect_ffmpeg()})
            return

        if path == "/api/fs/browse":
            self._handle_fs_browse(query)
            return

        if path == "/api/shows":
            self._send_json({"shows": self.server.store.list_shows()})
            return

        m_show = SHOW_DETAIL_RE.match(path)
        if m_show:
            show_id = m_show.group(1)
            autosync = query.get("autosync", ["0"])[0] in ("1", "true", "yes")
            if autosync:
                try:
                    res = self.server.store.rescan_show(show_id, reset_order=False)
                except FileNotFoundError:
                    res = None
                if res is not None:
                    show, ll, diff = res
                    self._send_json({"show": show, "linked_list": ll.to_dict(), "diff": diff})
                    return

            loaded = self.server.store.get_show(show_id)
            if loaded is None:
                self._send_error_json("Show not found", HTTPStatus.NOT_FOUND)
                return
            show, ll = loaded
            self._send_json({"show": show, "linked_list": ll.to_dict()})
            return

        m_stream = EPISODE_STREAM_RE.match(path)
        if m_stream:
            ep = self.server.store.get_episode(m_stream.group(1))
            if ep is None:
                self._send_error_json("Episode not found", HTTPStatus.NOT_FOUND)
                return
            stream_video_file(self, Path(ep.file_path), head_only=False)
            return

        if path == "/":
            self._serve_static("index.html")
            return
        if path.startswith("/static/"):
            self._serve_static(path[len("/static/"):])
            return
        if not path.startswith("/api/"):
            self._serve_static(path)
            return

        self._send_error_json("Endpoint not found", HTTPStatus.NOT_FOUND)

    def do_POST(self) -> None:
        parsed = urlparse(self.path)
        path = unquote(parsed.path)

        try:
            body = self._read_json_body()
        except (ValueError, json.JSONDecodeError) as exc:
            self._send_error_json(f"Invalid JSON body: {exc}", HTTPStatus.BAD_REQUEST)
            return

        if path == "/api/shows":
            folder_path = str(body.get("folder_path", "")).strip()
            name = body.get("name")
            if not folder_path:
                self._send_error_json("folder_path is required", HTTPStatus.BAD_REQUEST)
                return
            try:
                show, ll = self.server.store.add_show(folder_path, name=name)
            except FileNotFoundError as exc:
                self._send_error_json(str(exc), HTTPStatus.NOT_FOUND)
                return
            except ValueError as exc:
                self._send_error_json(str(exc), HTTPStatus.CONFLICT)
                return
            self._send_json({"show": show, "linked_list": ll.to_dict()}, status=HTTPStatus.CREATED)
            return

        m_action = SHOW_ACTION_RE.match(path)
        if m_action:
            show_id, action = m_action.group(1), m_action.group(2)
            self._handle_show_action(show_id, action, body)
            return

        m_prog = EPISODE_PROGRESS_RE.match(path)
        if m_prog:
            self._handle_progress_update(m_prog.group(1), body)
            return

        self._send_error_json("Endpoint not found", HTTPStatus.NOT_FOUND)

    def do_PATCH(self) -> None:
        parsed = urlparse(self.path)
        path = unquote(parsed.path)

        try:
            body = self._read_json_body()
        except (ValueError, json.JSONDecodeError) as exc:
            self._send_error_json(f"Invalid JSON body: {exc}", HTTPStatus.BAD_REQUEST)
            return

        m_prog = EPISODE_PROGRESS_RE.match(path)
        if m_prog:
            self._handle_progress_update(m_prog.group(1), body)
            return

        self._send_error_json("Endpoint not found", HTTPStatus.NOT_FOUND)

    def do_DELETE(self) -> None:
        parsed = urlparse(self.path)
        path = unquote(parsed.path)

        m_show = SHOW_DETAIL_RE.match(path)
        if m_show:
            deleted = self.server.store.delete_show(m_show.group(1))
            if not deleted:
                self._send_error_json("Show not found", HTTPStatus.NOT_FOUND)
                return
            self._send_json({"deleted": True})
            return

        self._send_error_json("Endpoint not found", HTTPStatus.NOT_FOUND)

    def _handle_show_action(self, show_id: str, action: str, body: dict[str, Any]) -> None:
        if action == "rescan":
            reset_order = bool(body.get("reset_order", False))
            try:
                res = self.server.store.rescan_show(show_id, reset_order=reset_order)
            except FileNotFoundError as exc:
                self._send_error_json(str(exc), HTTPStatus.NOT_FOUND)
                return
            if res is None:
                self._send_error_json("Show not found", HTTPStatus.NOT_FOUND)
                return
            show, ll, diff = res
            self._send_json({"show": show, "linked_list": ll.to_dict(), "diff": diff})
            return

        if action == "next":
            res = self.server.store.advance_next(show_id)
            if res is None:
                loaded = self.server.store.get_show(show_id)
                if loaded is None:
                    self._send_error_json("Show not found", HTTPStatus.NOT_FOUND)
                    return
                _, ll = loaded
                self._send_json({"episode": None, "at_end": True, "linked_list": ll.to_dict()})
                return
            nxt, ll = res
            self._send_json(
                {
                    "episode": nxt.to_dict(index=ll.index_of(nxt.id)),
                    "at_end": nxt.next is None,
                    "linked_list": ll.to_dict(),
                }
            )
            return

        if action == "prev":
            res = self.server.store.step_prev(show_id)
            if res is None:
                loaded = self.server.store.get_show(show_id)
                if loaded is None:
                    self._send_error_json("Show not found", HTTPStatus.NOT_FOUND)
                    return
                _, ll = loaded
                self._send_json({"episode": None, "at_start": True, "linked_list": ll.to_dict()})
                return
            prv, ll = res
            self._send_json(
                {
                    "episode": prv.to_dict(index=ll.index_of(prv.id)),
                    "at_start": prv.prev is None,
                    "linked_list": ll.to_dict(),
                }
            )
            return

        if action == "select":
            episode_id = str(body.get("episode_id", "")).strip()
            if not episode_id:
                self._send_error_json("episode_id is required", HTTPStatus.BAD_REQUEST)
                return
            node = self.server.store.select_episode(show_id, episode_id)
            if node is None:
                self._send_error_json("Show or episode not found", HTTPStatus.NOT_FOUND)
                return
            loaded = self.server.store.get_show(show_id)
            assert loaded is not None
            _, ll = loaded
            self._send_json(
                {
                    "episode": node.to_dict(index=ll.index_of(node.id)),
                    "linked_list": ll.to_dict(),
                }
            )
            return

        if action == "reorder":
            episode_id = str(body.get("episode_id", "")).strip()
            target_index = body.get("target_index")
            if not episode_id or not isinstance(target_index, int):
                self._send_error_json(
                    "episode_id and integer target_index are required",
                    HTTPStatus.BAD_REQUEST,
                )
                return
            ll = self.server.store.reorder_episode(show_id, episode_id, target_index)
            if ll is None:
                self._send_error_json("Show or episode not found", HTTPStatus.NOT_FOUND)
                return
            self._send_json({"linked_list": ll.to_dict()})
            return

    def _handle_progress_update(self, episode_id: str, body: dict[str, Any]) -> None:
        if "position_sec" not in body:
            self._send_error_json("position_sec is required", HTTPStatus.BAD_REQUEST)
            return
        try:
            position_sec = float(body["position_sec"])
            duration_sec = (
                float(body["duration_sec"]) if body.get("duration_sec") is not None else None
            )
        except (TypeError, ValueError):
            self._send_error_json("Invalid numeric playback values", HTTPStatus.BAD_REQUEST)
            return

        watched = body.get("watched")
        if watched is not None:
            watched = bool(watched)

        updated = self.server.store.update_progress(
            episode_id=episode_id,
            position_sec=position_sec,
            duration_sec=duration_sec,
            watched=watched,
        )
        if updated is None:
            self._send_error_json("Episode not found", HTTPStatus.NOT_FOUND)
            return
        self._send_json({"episode": updated.to_dict()})
