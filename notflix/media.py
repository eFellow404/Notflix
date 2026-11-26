"""Media MIME helpers and HTTP 206 byte-range video streaming."""

from __future__ import annotations

import mimetypes
import re
import shutil
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler
from pathlib import Path
from typing import Optional

VIDEO_MIME_TYPES: dict[str, str] = {
    ".mp4": "video/mp4",
    ".m4v": "video/mp4",
    ".webm": "video/webm",
    ".mkv": "video/x-matroska",
    ".mov": "video/quicktime",
    ".avi": "video/x-msvideo",
    ".ogv": "video/ogg",
    ".mpg": "video/mpeg",
    ".mpeg": "video/mpeg",
    ".vob": "video/mpeg",
    ".ts": "video/mp2t",
    ".m2ts": "video/mp2t",
    ".wmv": "video/x-ms-wmv",
    ".flv": "video/x-flv",
}

CHUNK_SIZE = 256 * 1024  # 256 KiB chunks for smooth seeking and low memory footprint
RANGE_RE = re.compile(r"^bytes=(\d*)-(\d*)$")


def detect_ffmpeg() -> bool:
    return shutil.which("ffmpeg") is not None


def guess_video_mime(file_path: Path) -> str:
    if mime := VIDEO_MIME_TYPES.get(file_path.suffix.lower()):
        return mime
    guessed, _ = mimetypes.guess_type(str(file_path))
    return guessed or "application/octet-stream"


def parse_range_header(range_header: str, file_size: int) -> Optional[tuple[int, int]]:
    """Parse an HTTP Range header into an inclusive (start, end) byte tuple.

    Returns ``None`` if no range header is provided or if multi-range is ignored.
    Raises ``ValueError`` if the requested byte range is invalid or unsatisfiable.
    """
    if not range_header:
        return None

    spec = range_header.split(",")[0].strip()
    match = RANGE_RE.match(spec)
    if not match:
        raise ValueError(f"Malformed Range header: {range_header}")

    start_str, end_str = match.group(1), match.group(2)
    if not start_str and not end_str:
        raise ValueError("Empty byte range")
    if file_size <= 0:
        raise ValueError("Cannot satisfy byte range for empty file")

    if not start_str:
        # Suffix byte range: bytes=-500 -> last 500 bytes
        suffix = int(end_str)
        if suffix <= 0:
            raise ValueError("Invalid suffix byte range")
        return max(0, file_size - suffix), file_size - 1

    start = int(start_str)
    if start >= file_size:
        raise ValueError("Range start past end of file")
    end = min(int(end_str), file_size - 1) if end_str else file_size - 1
    if end < start:
        raise ValueError("Range end before start")
    return start, end


def stream_video_file(
    handler: BaseHTTPRequestHandler,
    file_path: Path,
    head_only: bool = False,
) -> None:
    """Serve a local video file with full HTTP 206 Partial Content support."""
    if not file_path.is_file():
        handler.send_error(HTTPStatus.NOT_FOUND, "Video file not found on disk")
        return

    file_size = file_path.stat().st_size
    byte_range: Optional[tuple[int, int]] = None
    if range_header := handler.headers.get("Range"):
        try:
            byte_range = parse_range_header(range_header, file_size)
        except ValueError:
            handler.send_response(HTTPStatus.REQUESTED_RANGE_NOT_SATISFIABLE)
            handler.send_header("Content-Range", f"bytes */{file_size}")
            handler.send_header("Content-Length", "0")
            handler.end_headers()
            return

    if byte_range is not None:
        start, end = byte_range
        content_length = end - start + 1
        handler.send_response(HTTPStatus.PARTIAL_CONTENT)
        handler.send_header("Content-Range", f"bytes {start}-{end}/{file_size}")
    else:
        start = 0
        content_length = file_size
        handler.send_response(HTTPStatus.OK)

    handler.send_header("Content-Type", guess_video_mime(file_path))
    handler.send_header("Content-Length", str(content_length))
    handler.send_header("Accept-Ranges", "bytes")
    handler.send_header("Cache-Control", "no-store")
    handler.end_headers()

    if head_only or content_length == 0:
        return

    try:
        with file_path.open("rb") as fp:
            fp.seek(start)
            remaining = content_length
            while remaining > 0:
                chunk = fp.read(min(CHUNK_SIZE, remaining))
                if not chunk:
                    break
                handler.wfile.write(chunk)
                remaining -= len(chunk)
    except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
        # Browsers routinely close range requests mid-stream while scrubbing
        return
