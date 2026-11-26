"""Pure-Python container chapter extraction (MKV/WebM EBML and MP4 ``chpl``)."""

from __future__ import annotations

import re
import struct
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING, BinaryIO, Iterable, Optional, Sequence

if TYPE_CHECKING:
    from notflix.intro_detector import IntroSpan

INTRO_CHAPTER_RE = re.compile(
    r"(?:^|[\s\-_:/(\[])"
    r"(?:intro(?:duction)?|opening(?:\s+(?:credits?|titles?|theme|sequence))?|"
    r"main\s+titles?|title\s+sequence|theme\s+song|op(?:\s*\d+)?)"
    r"(?:$|[\s\-_:/)\]])",
    re.IGNORECASE,
)

# Matroska / EBML element IDs
_EBML_HEADER_MAGIC = b"\x1a\x45\xdf\xa3"
_EBML_SEGMENT_ID = 0x18538067
_EBML_SEEKHEAD_ID = 0x114D9B74
_EBML_CHAPTERS_ID = 0x1043A770
_EBML_CHAPTERS_ID_BYTES = b"\x10\x43\xa7\x70"
_EBML_CLUSTER_ID = 0x1F43B675
_EBML_SEEK_ID = 0x4DBB
_EBML_SEEK_TARGET_ID = 0x53AB
_EBML_SEEK_POSITION_ID = 0x53AC
_EBML_EDITION_ENTRY_ID = 0x45B9
_EBML_CHAPTER_ATOM_ID = 0xB6
_EBML_CHAPTER_TIME_START_ID = 0x91
_EBML_CHAPTER_TIME_END_ID = 0x92
_EBML_CHAPTER_DISPLAY_ID = 0x80
_EBML_CHAPTER_STRING_ID = 0x85


@dataclass(frozen=True)
class ChapterMarker:
    """A chapter entry extracted from an MKV/WebM or MP4 container."""

    start_sec: float
    end_sec: Optional[float]
    title: str


def _vint_length(first: int) -> int:
    """Determine EBML VINT byte length from its leading byte (0 if invalid)."""
    if first == 0:
        return 0
    mask = 0x80
    length = 1
    while length <= 8 and not (first & mask):
        mask >>= 1
        length += 1
    return length if length <= 8 else 0


def _read_ebml_vint(data: bytes, pos: int, keep_marker: bool = False) -> tuple[Optional[int], int]:
    """Read an EBML variable-length integer from ``data`` at ``pos``."""
    if pos >= len(data):
        return None, pos
    first = data[pos]
    length = _vint_length(first)
    if length == 0 or pos + length > len(data):
        return None, pos
    mask = 0x80 >> (length - 1)
    val = first if keep_marker else (first & (mask - 1))
    for i in range(1, length):
        val = (val << 8) | data[pos + i]
    if not keep_marker and val == (1 << (7 * length)) - 1:
        return None, pos + length
    return val, pos + length


def _read_ebml_vint_file(fp: BinaryIO, keep_marker: bool = False) -> tuple[Optional[int], int]:
    """Read an EBML VINT directly from a binary file stream."""
    b = fp.read(1)
    if not b:
        return None, 0
    first = b[0]
    length = _vint_length(first)
    if length == 0:
        return None, 1
    rest = fp.read(length - 1)
    if len(rest) < length - 1:
        return None, 1 + len(rest)
    mask = 0x80 >> (length - 1)
    val = first if keep_marker else (first & (mask - 1))
    for byte in rest:
        val = (val << 8) | byte
    if not keep_marker and val == (1 << (7 * length)) - 1:
        return None, length
    return val, length


def _iter_ebml_elements(buf: bytes) -> Iterable[tuple[int, bytes]]:
    pos = 0
    end = len(buf)
    while pos < end:
        eid, pos = _read_ebml_vint(buf, pos, keep_marker=True)
        if eid is None:
            break
        size, pos = _read_ebml_vint(buf, pos, keep_marker=False)
        if size is None or pos + size > end:
            break
        yield eid, buf[pos : pos + size]
        pos += size


def _parse_mkv_chapters_blob(blob: bytes) -> list[ChapterMarker]:
    chapters: list[ChapterMarker] = []
    # Chapters (0x1043A770) -> EditionEntry (0x45B9) -> ChapterAtom (0xB6)
    for eid, payload in _iter_ebml_elements(blob):
        if eid != _EBML_EDITION_ENTRY_ID:
            continue
        for sub_id, sub_payload in _iter_ebml_elements(payload):
            if sub_id != _EBML_CHAPTER_ATOM_ID:
                continue
            start_ns: Optional[int] = None
            end_ns: Optional[int] = None
            title = ""
            for atom_id, atom_val in _iter_ebml_elements(sub_payload):
                if atom_id == _EBML_CHAPTER_TIME_START_ID and atom_val:
                    start_ns = int.from_bytes(atom_val, "big")
                elif atom_id == _EBML_CHAPTER_TIME_END_ID and atom_val:
                    end_ns = int.from_bytes(atom_val, "big")
                elif atom_id == _EBML_CHAPTER_DISPLAY_ID:
                    for disp_id, disp_val in _iter_ebml_elements(atom_val):
                        if disp_id == _EBML_CHAPTER_STRING_ID and not title:
                            title = disp_val.decode("utf-8", errors="replace").strip()
            if start_ns is not None:
                chapters.append(
                    ChapterMarker(
                        start_sec=start_ns / 1_000_000_000.0,
                        end_sec=(end_ns / 1_000_000_000.0) if end_ns is not None else None,
                        title=title,
                    )
                )
    chapters.sort(key=lambda c: c.start_sec)
    return chapters


def _find_chapters_offset_in_seekhead(seek_blob: bytes, seg_data_start: int) -> Optional[int]:
    """Return absolute file offset of the Chapters element from a SeekHead payload."""
    chapters_offset: Optional[int] = None
    for sid, spayload in _iter_ebml_elements(seek_blob):
        if sid != _EBML_SEEK_ID:
            continue
        target_id: Optional[bytes] = None
        target_pos: Optional[int] = None
        for fid, fval in _iter_ebml_elements(spayload):
            if fid == _EBML_SEEK_TARGET_ID:
                target_id = fval
            elif fid == _EBML_SEEK_POSITION_ID and fval:
                target_pos = int.from_bytes(fval, "big")
        if target_id == _EBML_CHAPTERS_ID_BYTES and target_pos is not None:
            chapters_offset = seg_data_start + target_pos
    return chapters_offset


def extract_mkv_chapters(file_path: Path) -> list[ChapterMarker]:
    """Extract chapter markers from an MKV/WebM file without external tools.

    Skips media ``Cluster`` elements via ``seek()`` and follows ``SeekHead``
    pointers when present so scanning multi-GB files takes milliseconds.
    """
    try:
        file_size = file_path.stat().st_size
        with file_path.open("rb") as fp:
            if fp.read(4) != _EBML_HEADER_MAGIC:
                return []
            fp.seek(0)
            # Read EBML header and skip past it
            _, _ = _read_ebml_vint_file(fp, keep_marker=True)
            ebml_size, _ = _read_ebml_vint_file(fp, keep_marker=False)
            if ebml_size is None:
                return []
            fp.seek(ebml_size, 1)

            # Segment element (0x18538067)
            seg_id, _ = _read_ebml_vint_file(fp, keep_marker=True)
            if seg_id != _EBML_SEGMENT_ID:
                return []
            seg_size, _ = _read_ebml_vint_file(fp, keep_marker=False)
            seg_data_start = fp.tell()
            seg_end = (
                min(file_size, seg_data_start + seg_size)
                if seg_size is not None
                else file_size
            )

            chapters_offset: Optional[int] = None
            scanned = 0
            while fp.tell() < seg_end and scanned < 512:
                scanned += 1
                eid, _ = _read_ebml_vint_file(fp, keep_marker=True)
                if eid is None:
                    break
                size, _ = _read_ebml_vint_file(fp, keep_marker=False)
                elem_start = fp.tell()

                if eid == _EBML_CHAPTERS_ID and size is not None and size <= 4 * 1024 * 1024:
                    return _parse_mkv_chapters_blob(fp.read(size))

                if eid == _EBML_SEEKHEAD_ID and size is not None and size <= 1024 * 1024:
                    found = _find_chapters_offset_in_seekhead(fp.read(size), seg_data_start)
                    if found is not None:
                        chapters_offset = found
                    if chapters_offset is not None and chapters_offset < file_size:
                        fp.seek(chapters_offset)
                        cid, _ = _read_ebml_vint_file(fp, keep_marker=True)
                        csize, _ = _read_ebml_vint_file(fp, keep_marker=False)
                        if cid == _EBML_CHAPTERS_ID and csize is not None and csize <= 4 * 1024 * 1024:
                            return _parse_mkv_chapters_blob(fp.read(csize))
                        fp.seek(elem_start + size)
                    continue

                if size is None:
                    break
                # Stop once we hit media clusters unless SeekHead already pointed us
                if eid == _EBML_CLUSTER_ID and chapters_offset is None:
                    break
                fp.seek(elem_start + size)
    except OSError:
        return []
    return []


def _parse_mp4_chpl(payload: bytes) -> list[ChapterMarker]:
    """Parse an MP4 Nero ``chpl`` atom body."""
    if len(payload) < 9:
        return []
    offset = 8 if payload[0] == 1 else 4
    if offset >= len(payload):
        return []
    count = payload[offset]
    offset += 1
    chapters: list[ChapterMarker] = []
    for _ in range(count):
        if offset + 9 > len(payload):
            break
        timestamp_100ns = struct.unpack(">Q", payload[offset : offset + 8])[0]
        offset += 8
        title_len = payload[offset]
        offset += 1
        if offset + title_len > len(payload):
            break
        title = payload[offset : offset + title_len].decode("utf-8", errors="replace").strip()
        offset += title_len
        chapters.append(
            ChapterMarker(
                start_sec=timestamp_100ns / 10_000_000.0,
                end_sec=None,
                title=title,
            )
        )
    chapters.sort(key=lambda c: c.start_sec)
    return chapters


def _find_mp4_box(fp: BinaryIO, start: int, end: int, target: bytes) -> Optional[tuple[int, int]]:
    """Locate an MP4 box within ``[start, end)`` and return its ``(body_start, box_end)``."""
    pos = start
    while pos + 8 <= end:
        fp.seek(pos)
        hdr = fp.read(8)
        if len(hdr) < 8:
            return None
        size, box_type = struct.unpack(">I4s", hdr)
        hdr_len = 8
        if size == 1:
            ext = fp.read(8)
            if len(ext) < 8:
                return None
            size = struct.unpack(">Q", ext)[0]
            hdr_len = 16
        elif size == 0:
            size = end - pos
        if size < hdr_len or pos + size > end:
            return None
        if box_type == target:
            return pos + hdr_len, pos + size
        pos += size
    return None


def extract_mp4_chapters(file_path: Path) -> list[ChapterMarker]:
    """Extract Nero ``chpl`` chapter markers from an MP4/M4V/MOV container."""
    try:
        file_size = file_path.stat().st_size
        with file_path.open("rb") as fp:
            moov = _find_mp4_box(fp, 0, file_size, b"moov")
            if moov is None:
                return []
            udta = _find_mp4_box(fp, moov[0], moov[1], b"udta")
            if udta is None:
                return []
            chpl = _find_mp4_box(fp, udta[0], udta[1], b"chpl")
            if chpl is None:
                return []
            length = chpl[1] - chpl[0]
            if length <= 0 or length > 2 * 1024 * 1024:
                return []
            fp.seek(chpl[0])
            return _parse_mp4_chpl(fp.read(length))
    except OSError:
        return []


def extract_chapters(file_path: Path) -> list[ChapterMarker]:
    """Extract container chapters for supported video formats (MKV, WebM, MP4, M4V, MOV)."""
    ext = file_path.suffix.lower()
    if ext in {".mkv", ".webm"}:
        return extract_mkv_chapters(file_path)
    if ext in {".mp4", ".m4v", ".mov"}:
        return extract_mp4_chapters(file_path)
    return []


def detect_intro_from_chapters(
    chapters: Sequence[ChapterMarker],
    duration_sec: float = 0.0,
) -> Optional[IntroSpan]:
    """Return an ``IntroSpan`` if chapter markers explicitly label an opening/intro."""
    if not chapters:
        return None
    from notflix.intro_detector import IntroSpan, MAX_INTRO_SEC, MIN_INTRO_SEC

    for idx, ch in enumerate(chapters):
        if not ch.title or not INTRO_CHAPTER_RE.search(ch.title):
            continue
        start = max(0.0, ch.start_sec)
        end = ch.end_sec
        if end is None and idx + 1 < len(chapters):
            end = chapters[idx + 1].start_sec
        if end is None:
            continue
        length = end - start
        if length < MIN_INTRO_SEC or length > MAX_INTRO_SEC:
            continue
        if start > 600.0:
            continue
        if duration_sec > 0 and start > duration_sec * 0.45:
            continue
        return IntroSpan(start_sec=start, end_sec=end, confidence=0.95, source="chapters")
    return None
