"""Threaded HTTP server, REST API, and static frontend host for Notflix."""

from __future__ import annotations

import json
import mimetypes
import re
import threading
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Optional
from urllib.parse import parse_qs, unquote, urlparse

from notflix import __version__
from notflix.intelligence import VIDEO_EXTENSIONS
from notflix.intro_detector import (
    build_fingerprint,
    detect_show_intros_server,
    find_shared_intros,
)
from notflix.linked_list import EpisodeNode
from notflix.media import detect_ffmpeg, stream_video_file
from notflix.storage import NotflixStore
from notflix.updater import (
    DEFAULT_FETCH_TIMEOUT_SEC,
    UpdateResult,
    apply_update,
    check_for_updates,
    default_repo_dir,
    get_local_repo_info,
)

SHOW_DETAIL_RE = re.compile(r"^/api/shows/([^/]+)$")
SHOW_ACTION_RE = re.compile(
    r"^/api/shows/([^/]+)/(rescan|next|prev|select|reorder|detect-intros|watched)$"
)
EPISODE_PROGRESS_RE = re.compile(r"^/api/episodes/([^/]+)/progress$")
EPISODE_INTRO_RE = re.compile(r"^/api/episodes/([^/]+)/intro$")
EPISODE_STREAM_RE = re.compile(r"^/api/episodes/([^/]+)/stream$")

STATIC_MIME_TYPES: dict[str, str] = {
    ".js": "application/javascript; charset=utf-8",
    ".mjs": "application/javascript; charset=utf-8",
    ".css": "text/css; charset=utf-8",
    ".html": "text/html; charset=utf-8",
    ".svg": "image/svg+xml",
    ".json": "application/json; charset=utf-8",
}

MAX_JSON_BODY_BYTES = 12 * 1024 * 1024


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
        repo_dir: Optional[Path] = None,
        initial_update_result: Optional[UpdateResult] = None,
    ) -> None:
        self.store = store
        self.static_dir = (static_dir or default_static_dir()).resolve()
        self.quiet = quiet
        self.repo_dir = (repo_dir or default_repo_dir()).resolve()
        self._update_lock = threading.Lock()
        self.last_update_result: Optional[UpdateResult] = initial_update_result
        super().__init__(server_address, NotflixRequestHandler)


class NotflixRequestHandler(BaseHTTPRequestHandler):
    """HTTP request handler serving the Notflix REST API, video streams, and SPA."""

    server: NotflixHTTPServer  # type: ignore[assignment]
    protocol_version = "HTTP/1.1"

    def log_message(self, format: str, *args: Any) -> None:  # noqa: A002
        if not self.server.quiet:
            super().log_message(format, *args)

    def _parse_url(self) -> tuple[str, dict[str, list[str]]]:
        parsed = urlparse(self.path)
        return unquote(parsed.path), parse_qs(parsed.query)

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

    def _send_episode_or_404(self, node: Optional[EpisodeNode]) -> None:
        if node is None:
            self._send_error_json("Episode not found", HTTPStatus.NOT_FOUND)
            return
        self._send_json({"episode": self.server.store.episode_to_dict_with_links(node)})

    def _read_json_body(self) -> dict[str, Any]:
        try:
            length = int(self.headers.get("Content-Length", "0"))
        except ValueError:
            raise ValueError("Invalid Content-Length")
        if length <= 0:
            return {}
        if length > MAX_JSON_BODY_BYTES:
            raise ValueError("Request body too large")
        raw = self.rfile.read(length)
        if not raw.strip():
            return {}
        data = json.loads(raw.decode("utf-8"))
        if not isinstance(data, dict):
            raise ValueError("Expected JSON object")
        return data

    def _parse_json_or_400(self) -> Optional[dict[str, Any]]:
        try:
            return self._read_json_body()
        except (ValueError, json.JSONDecodeError) as exc:
            self._send_error_json(f"Invalid JSON body: {exc}", HTTPStatus.BAD_REQUEST)
            return None

    def _serve_static(
        self, rel_path: str, head_only: bool = False, spa_fallback: bool = False
    ) -> None:
        static_root = self.server.static_dir
        clean = rel_path.lstrip("/") or "index.html"
        try:
            target = (static_root / clean).resolve()
            target.relative_to(static_root)
        except (ValueError, OSError):
            self._send_error_json("Forbidden path", HTTPStatus.FORBIDDEN)
            return

        if not target.is_file():
            if spa_fallback and not clean.startswith("api/"):
                target = static_root / "index.html"
            if not target.is_file():
                self._send_error_json("Not found", HTTPStatus.NOT_FOUND)
                return

        mime = STATIC_MIME_TYPES.get(target.suffix.lower())
        if mime is None:
            mime, _ = mimetypes.guess_type(str(target))

        content = target.read_bytes()
        self.send_response(HTTPStatus.OK)
        self.send_header("Content-Type", mime or "application/octet-stream")
        self.send_header("Content-Length", str(len(content)))
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        if not head_only:
            self.wfile.write(content)

    def _handle_episode_stream(self, episode_id: str, head_only: bool) -> None:
        ep = self.server.store.get_episode(episode_id)
        if ep is None:
            if head_only:
                self.send_error(HTTPStatus.NOT_FOUND, "Episode not found")
            else:
                self._send_error_json("Episode not found", HTTPStatus.NOT_FOUND)
            return
        stream_video_file(self, Path(ep.file_path), head_only=head_only)

    def _handle_fs_browse(self, query: dict[str, list[str]]) -> None:
        raw_path = query.get("path", [""])[0].strip()
        target = Path(raw_path).expanduser() if raw_path else Path.home()
        try:
            resolved = target.resolve()
        except OSError as exc:
            self._send_error_json(f"Cannot resolve path: {exc}", HTTPStatus.BAD_REQUEST)
            return

        if not resolved.is_dir():
            self._send_error_json(f"Directory not found: {resolved}", HTTPStatus.NOT_FOUND)
            return

        try:
            entries = sorted(resolved.iterdir(), key=lambda p: p.name.lower())
        except PermissionError:
            self._send_error_json("Permission denied reading directory", HTTPStatus.FORBIDDEN)
            return

        dirs: list[dict[str, Any]] = []
        direct_video_count = 0
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
        path, _ = self._parse_url()

        if m := EPISODE_STREAM_RE.match(path):
            self._handle_episode_stream(m.group(1), head_only=True)
            return

        if path == "/" or path.startswith("/static/"):
            rel = "index.html" if path == "/" else path[len("/static/"):]
            self._serve_static(rel, head_only=True)
            return

        self.send_error(HTTPStatus.NOT_FOUND)

    def do_GET(self) -> None:
        path, query = self._parse_url()

        if path == "/api/health":
            local_info = get_local_repo_info(self.server.repo_dir)
            self._send_json(
                {
                    "status": "ok",
                    "version": __version__,
                    "commit": local_info.local_sha[:7] if local_info.local_sha else None,
                    "ffmpeg_available": detect_ffmpeg(),
                }
            )
            return
        if path == "/api/update/status":
            res = self.server.last_update_result or get_local_repo_info(self.server.repo_dir)
            self._send_json(res.to_dict())
            return
        if path == "/api/fs/browse":
            self._handle_fs_browse(query)
            return
        if path == "/api/shows":
            self._send_json({"shows": self.server.store.list_shows()})
            return

        if m := SHOW_DETAIL_RE.match(path):
            self._handle_get_show(m.group(1), query)
            return
        if m := EPISODE_STREAM_RE.match(path):
            self._handle_episode_stream(m.group(1), head_only=False)
            return

        if path == "/":
            self._serve_static("index.html")
            return
        if path.startswith("/static/"):
            self._serve_static(path[len("/static/"):])
            return
        if not path.startswith("/api/"):
            self._serve_static(path, spa_fallback=True)
            return

        self._send_error_json("Endpoint not found", HTTPStatus.NOT_FOUND)

    def do_POST(self) -> None:
        path, _ = self._parse_url()
        body = self._parse_json_or_400()
        if body is None:
            return

        if path == "/api/shows":
            self._handle_create_show(body)
            return
        if path == "/api/update/check":
            self._handle_update_check(body)
            return
        if path == "/api/update/apply":
            self._handle_update_apply(body)
            return
        if m := SHOW_ACTION_RE.match(path):
            self._handle_show_action(m.group(1), m.group(2), body)
            return
        if self._dispatch_update_route(path, body):
            return

        self._send_error_json("Endpoint not found", HTTPStatus.NOT_FOUND)

    def do_PATCH(self) -> None:
        path, _ = self._parse_url()
        body = self._parse_json_or_400()
        if body is None:
            return

        if self._dispatch_update_route(path, body):
            return

        self._send_error_json("Endpoint not found", HTTPStatus.NOT_FOUND)

    def do_DELETE(self) -> None:
        path, _ = self._parse_url()

        if m := SHOW_DETAIL_RE.match(path):
            if not self.server.store.delete_show(m.group(1)):
                self._send_error_json("Show not found", HTTPStatus.NOT_FOUND)
                return
            self._send_json({"deleted": True})
            return

        self._send_error_json("Endpoint not found", HTTPStatus.NOT_FOUND)

    def _dispatch_update_route(self, path: str, body: dict[str, Any]) -> bool:
        if m := SHOW_DETAIL_RE.match(path):
            self._handle_show_rename(m.group(1), body)
            return True
        if m := EPISODE_PROGRESS_RE.match(path):
            self._handle_progress_update(m.group(1), body)
            return True
        if m := EPISODE_INTRO_RE.match(path):
            self._handle_intro_update(m.group(1), body)
            return True
        return False

    def _handle_update_check(self, body: dict[str, Any]) -> None:
        try:
            timeout_sec = float(body.get("timeout_sec", DEFAULT_FETCH_TIMEOUT_SEC))
        except (TypeError, ValueError):
            timeout_sec = DEFAULT_FETCH_TIMEOUT_SEC
        timeout_sec = max(1.0, min(30.0, timeout_sec))
        with self.server._update_lock:
            result = check_for_updates(self.server.repo_dir, timeout_sec=timeout_sec)
            self.server.last_update_result = result
        self._send_json(result.to_dict())

    def _handle_update_apply(self, body: dict[str, Any]) -> None:
        try:
            timeout_sec = float(body.get("timeout_sec", DEFAULT_FETCH_TIMEOUT_SEC))
        except (TypeError, ValueError):
            timeout_sec = DEFAULT_FETCH_TIMEOUT_SEC
        timeout_sec = max(1.0, min(30.0, timeout_sec))
        with self.server._update_lock:
            result = apply_update(
                repo_dir=self.server.repo_dir,
                db_path=self.server.store.db_path,
                timeout_sec=timeout_sec,
            )
            self.server.last_update_result = result
        self._send_json(result.to_dict())

    def _handle_get_show(self, show_id: str, query: dict[str, list[str]]) -> None:
        if query.get("autosync", ["0"])[0] in ("1", "true", "yes"):
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

    def _handle_create_show(self, body: dict[str, Any]) -> None:
        folder_path = str(body.get("folder_path", "")).strip()
        if not folder_path:
            self._send_error_json("folder_path is required", HTTPStatus.BAD_REQUEST)
            return
        try:
            show, ll = self.server.store.add_show(folder_path, name=body.get("name"))
        except FileNotFoundError as exc:
            self._send_error_json(str(exc), HTTPStatus.NOT_FOUND)
            return
        except ValueError as exc:
            self._send_error_json(str(exc), HTTPStatus.CONFLICT)
            return
        self._send_json({"show": show, "linked_list": ll.to_dict()}, status=HTTPStatus.CREATED)

    def _handle_show_rename(self, show_id: str, body: dict[str, Any]) -> None:
        name = str(body.get("name", "")).strip()
        if not name:
            self._send_error_json("name is required", HTTPStatus.BAD_REQUEST)
            return
        try:
            updated_show = self.server.store.rename_show(show_id, name)
        except ValueError as exc:
            self._send_error_json(str(exc), HTTPStatus.BAD_REQUEST)
            return
        if updated_show is None:
            self._send_error_json("Show not found", HTTPStatus.NOT_FOUND)
            return
        self._send_json({"show": updated_show})

    def _handle_show_action(self, show_id: str, action: str, body: dict[str, Any]) -> None:
        if action == "rescan":
            self._handle_rescan(show_id, body)
        elif action == "next":
            self._handle_step(show_id, forward=True)
        elif action == "prev":
            self._handle_step(show_id, forward=False)
        elif action == "select":
            self._handle_select(show_id, body)
        elif action == "reorder":
            self._handle_reorder(show_id, body)
        elif action == "detect-intros":
            self._handle_detect_intros(show_id, body)
        elif action == "watched":
            self._handle_bulk_watched(show_id, body)

    def _handle_rescan(self, show_id: str, body: dict[str, Any]) -> None:
        try:
            res = self.server.store.rescan_show(
                show_id, reset_order=bool(body.get("reset_order", False))
            )
        except FileNotFoundError as exc:
            self._send_error_json(str(exc), HTTPStatus.NOT_FOUND)
            return
        if res is None:
            self._send_error_json("Show not found", HTTPStatus.NOT_FOUND)
            return
        show, ll, diff = res
        self._send_json({"show": show, "linked_list": ll.to_dict(), "diff": diff})

    def _handle_step(self, show_id: str, forward: bool) -> None:
        boundary_key = "at_end" if forward else "at_start"
        res = (
            self.server.store.advance_next(show_id)
            if forward
            else self.server.store.step_prev(show_id)
        )
        if res is None:
            loaded = self.server.store.get_show(show_id)
            if loaded is None:
                self._send_error_json("Show not found", HTTPStatus.NOT_FOUND)
                return
            _, ll = loaded
            self._send_json({"episode": None, boundary_key: True, "linked_list": ll.to_dict()})
            return

        node, ll = res
        at_boundary = (node.next is None) if forward else (node.prev is None)
        self._send_json(
            {
                "episode": node.to_dict(index=ll.index_of(node.id)),
                boundary_key: at_boundary,
                "linked_list": ll.to_dict(),
            }
        )

    def _handle_select(self, show_id: str, body: dict[str, Any]) -> None:
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

    def _handle_reorder(self, show_id: str, body: dict[str, Any]) -> None:
        episode_id = str(body.get("episode_id", "")).strip()
        target_index = body.get("target_index")
        if not episode_id or not isinstance(target_index, int) or isinstance(target_index, bool):
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

    def _handle_bulk_watched(self, show_id: str, body: dict[str, Any]) -> None:
        raw_ids = body.get("episode_ids")
        if not isinstance(raw_ids, list) or not raw_ids:
            self._send_error_json("episode_ids must be a non-empty list", HTTPStatus.BAD_REQUEST)
            return
        ll = self.server.store.set_episodes_watched(
            show_id=show_id,
            episode_ids=[str(eid) for eid in raw_ids],
            watched=bool(body.get("watched", True)),
            reset_position=bool(body.get("reset_position", False)),
        )
        if ll is None:
            self._send_error_json("Show not found", HTTPStatus.NOT_FOUND)
            return
        self._send_json({"linked_list": ll.to_dict()})

    @staticmethod
    def _parse_client_fingerprints(raw_fps: list[Any], valid_ids: set[str]) -> list[Any]:
        parsed_fps = []
        for item in raw_fps:
            if not isinstance(item, dict):
                continue
            ep_id = str(item.get("episode_id", "")).strip()
            vectors = item.get("vectors")
            if ep_id not in valid_ids or not isinstance(vectors, list) or not vectors:
                continue
            try:
                step_sec = float(item.get("step_sec", 1.5))
                duration_sec = float(item.get("duration_sec", 0.0) or 0.0)
            except (TypeError, ValueError):
                continue
            flat_flags = item.get("flat_flags")
            parsed_fps.append(
                build_fingerprint(
                    episode_id=ep_id,
                    step_sec=step_sec,
                    raw_vectors=vectors,
                    flat_flags=flat_flags if isinstance(flat_flags, list) else None,
                    duration_sec=duration_sec,
                    source="visual",
                    min_std=8.0,
                )
            )
        return parsed_fps

    def _handle_detect_intros(self, show_id: str, body: dict[str, Any]) -> None:
        loaded = self.server.store.get_show(show_id)
        if loaded is None:
            self._send_error_json("Show not found", HTTPStatus.NOT_FOUND)
            return
        _, ll = loaded
        force = bool(body.get("force", False))

        raw_fps = body.get("fingerprints")
        if raw_fps is not None:
            if not isinstance(raw_fps, list):
                self._send_error_json("fingerprints must be a list", HTTPStatus.BAD_REQUEST)
                return
            parsed_fps = self._parse_client_fingerprints(raw_fps, {node.id for node in ll})
            detected = find_shared_intros(parsed_fps)
            method = "visual"
            needs_visual = False
            should_mark_scanned = bool(body.get("mark_scanned", True))
        else:
            # Server-side pass: container chapter markers + optional ffmpeg audio matching
            non_extras = [n for n in ll if not n.is_extra]
            targets = non_extras if len(non_extras) >= 2 else ll.to_list()
            ep_specs = [(n.id, Path(n.file_path), n.duration_sec) for n in targets]
            detected, method, needs_visual = detect_show_intros_server(ep_specs)
            should_mark_scanned = not needs_visual

        saved = self.server.store.save_show_intros(
            show_id,
            detected,
            preserve_manual=not force,
            mark_scanned=should_mark_scanned,
        )
        assert saved is not None
        updated_show, updated_ll = saved
        self._send_json(
            {
                "show": updated_show,
                "linked_list": updated_ll.to_dict(),
                "detected_count": len(detected),
                "method": method,
                "needs_client_visual_scan": needs_visual,
            }
        )

    def _handle_intro_update(self, episode_id: str, body: dict[str, Any]) -> None:
        clear = bool(body.get("clear", False))
        start_val = body.get("intro_start_sec")
        end_val = body.get("intro_end_sec")

        if clear or (start_val is None and end_val is None):
            updated = self.server.store.set_episode_intro(episode_id, None, None)
        else:
            try:
                start_sec = float(start_val)
                end_sec = float(end_val)
            except (TypeError, ValueError):
                self._send_error_json(
                    "intro_start_sec and intro_end_sec must be numbers",
                    HTTPStatus.BAD_REQUEST,
                )
                return
            try:
                updated = self.server.store.set_episode_intro(
                    episode_id=episode_id,
                    intro_start_sec=start_sec,
                    intro_end_sec=end_sec,
                    confidence=1.0,
                    source="manual",
                )
            except ValueError as exc:
                self._send_error_json(str(exc), HTTPStatus.BAD_REQUEST)
                return

        self._send_episode_or_404(updated)

    def _handle_progress_update(self, episode_id: str, body: dict[str, Any]) -> None:
        has_pos = body.get("position_sec") is not None
        has_watched = body.get("watched") is not None
        if not has_pos and not has_watched:
            self._send_error_json(
                "position_sec or watched is required", HTTPStatus.BAD_REQUEST
            )
            return
        try:
            position_sec = float(body["position_sec"]) if has_pos else None
            duration_sec = (
                float(body["duration_sec"]) if body.get("duration_sec") is not None else None
            )
        except (TypeError, ValueError):
            self._send_error_json("Invalid numeric playback values", HTTPStatus.BAD_REQUEST)
            return

        set_current = body.get("set_current")
        updated = self.server.store.update_progress(
            episode_id=episode_id,
            position_sec=position_sec,
            duration_sec=duration_sec,
            watched=bool(body["watched"]) if has_watched else None,
            set_current=bool(set_current) if set_current is not None else None,
            reset_position=bool(body.get("reset_position", False)),
        )
        self._send_episode_or_404(updated)
