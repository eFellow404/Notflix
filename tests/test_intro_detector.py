"""Unit tests for cross-episode intro detection and container chapter parsing."""

from __future__ import annotations

import random
import struct
import tempfile
import unittest
from pathlib import Path

from notflix.intro_detector import (
    ChapterMarker,
    build_fingerprint,
    detect_intro_from_chapters,
    detect_show_intros_server,
    extract_mkv_chapters,
    extract_mp4_chapters,
    find_shared_intros,
)


def _rand_vec(rng: random.Random, dim: int = 36) -> list[float]:
    return [rng.uniform(20.0, 235.0) for _ in range(dim)]


def _perturb_vec(rng: random.Random, vec: list[float], noise: float = 8.0) -> list[float]:
    return [max(0.0, min(255.0, v + rng.uniform(-noise, noise))) for v in vec]


def _make_episode_vectors(
    rng: random.Random,
    total_frames: int,
    intro_template: list[list[float]] | None = None,
    intro_offset: int = 0,
    noise: float = 8.0,
) -> list[list[float]]:
    frames = [_rand_vec(rng) for _ in range(total_frames)]
    if intro_template is not None:
        for idx, base in enumerate(intro_template):
            pos = intro_offset + idx
            if 0 <= pos < total_frames:
                frames[pos] = _perturb_vec(rng, base, noise=noise)
    return frames


def _ebml_vint_size(n: int) -> bytes:
    if n < 0x7F:
        return bytes([0x80 | n])
    if n < 0x3FFF:
        return struct.pack(">H", 0x4000 | n)
    return struct.pack(">I", 0x10000000 | n)


def _ebml_elem(eid_bytes: bytes, payload: bytes) -> bytes:
    return eid_bytes + _ebml_vint_size(len(payload)) + payload


def _build_minimal_mkv_with_chapters(chapters: list[tuple[float, float, str]]) -> bytes:
    atoms = b""
    for start_s, end_s, title in chapters:
        start_ns = int(start_s * 1_000_000_000)
        end_ns = int(end_s * 1_000_000_000)
        disp = _ebml_elem(b"\x80", _ebml_elem(b"\x85", title.encode("utf-8")))
        body = (
            _ebml_elem(b"\x91", start_ns.to_bytes(8, "big"))
            + _ebml_elem(b"\x92", end_ns.to_bytes(8, "big"))
            + disp
        )
        atoms += _ebml_elem(b"\xb6", body)
    edition = _ebml_elem(b"\x45\xb9", atoms)
    chapters_el = _ebml_elem(b"\x10\x43\xa7\x70", edition)
    ebml_header = _ebml_elem(b"\x1a\x45\xdf\xa3", _ebml_elem(b"\x42\x82", b"matroska"))
    segment = _ebml_elem(b"\x18\x53\x80\x67", chapters_el)
    return ebml_header + segment


def _mp4_box(box_type: bytes, payload: bytes) -> bytes:
    return struct.pack(">I4s", 8 + len(payload), box_type) + payload


def _build_minimal_mp4_with_chpl(chapters: list[tuple[float, str]]) -> bytes:
    entries = bytearray()
    entries.extend(b"\x01\x00\x00\x00")  # version 1 + flags
    entries.extend(b"\x00\x00\x00\x00")  # reserved
    entries.append(len(chapters))
    for start_s, title in chapters:
        t_100ns = int(start_s * 10_000_000)
        title_b = title.encode("utf-8")
        entries.extend(struct.pack(">Q", t_100ns))
        entries.append(len(title_b))
        entries.extend(title_b)
    chpl = _mp4_box(b"chpl", bytes(entries))
    udta = _mp4_box(b"udta", chpl)
    moov = _mp4_box(b"moov", udta)
    ftyp = _mp4_box(b"ftyp", b"isom\x00\x00\x02\x00isom")
    return ftyp + moov


class TestIntroDetector(unittest.TestCase):
    def test_detects_shared_intro_with_variable_cold_opens(self) -> None:
        rng = random.Random(42)
        step = 1.5
        # 30-frame intro = 45 seconds
        intro_template = [_rand_vec(rng) for _ in range(30)]

        # Different cold-open offsets across 4 episodes:
        # Ep1: 15s (frame 10), Ep2: 60s (frame 40), Ep3: 0s (frame 0), Ep4: 90s (frame 60)
        offsets = {"ep1": 10, "ep2": 40, "ep3": 0, "ep4": 60}
        fps = [
            build_fingerprint(
                episode_id=ep_id,
                step_sec=step,
                raw_vectors=_make_episode_vectors(
                    rng, total_frames=160, intro_template=intro_template, intro_offset=off
                ),
                source="visual",
                min_std=8.0,
            )
            for ep_id, off in offsets.items()
        ]

        detected = find_shared_intros(fps)
        self.assertEqual(set(detected.keys()), set(offsets.keys()))
        for ep_id, off in offsets.items():
            span = detected[ep_id]
            expected_start = off * step
            expected_end = (off + 30) * step
            self.assertAlmostEqual(span.start_sec, expected_start, delta=3.0)
            self.assertAlmostEqual(span.end_sec, expected_end, delta=3.0)
            self.assertGreaterEqual(span.confidence, 0.80)
            self.assertEqual(span.source, "visual")

    def test_pilot_without_intro_not_falsely_tagged(self) -> None:
        rng = random.Random(123)
        step = 1.5
        intro_template = [_rand_vec(rng) for _ in range(28)]  # 42s intro

        fps = [
            # Ep 1 (Pilot): completely unique, no intro!
            build_fingerprint(
                "pilot",
                step,
                _make_episode_vectors(rng, 140, intro_template=None),
                source="visual",
            ),
            # Ep 2, 3, 4: all share the show's standard intro at different cold-open offsets
            build_fingerprint(
                "ep2",
                step,
                _make_episode_vectors(rng, 140, intro_template=intro_template, intro_offset=12),
                source="visual",
            ),
            build_fingerprint(
                "ep3",
                step,
                _make_episode_vectors(rng, 140, intro_template=intro_template, intro_offset=34),
                source="visual",
            ),
            build_fingerprint(
                "ep4",
                step,
                _make_episode_vectors(rng, 140, intro_template=intro_template, intro_offset=4),
                source="visual",
            ),
        ]

        detected = find_shared_intros(fps)
        self.assertNotIn("pilot", detected)
        self.assertIn("ep2", detected)
        self.assertIn("ep3", detected)
        self.assertIn("ep4", detected)
        self.assertAlmostEqual(detected["ep2"].start_sec, 12 * step, delta=3.0)
        self.assertAlmostEqual(detected["ep3"].start_sec, 34 * step, delta=3.0)
        self.assertAlmostEqual(detected["ep4"].start_sec, 4 * step, delta=3.0)

    def test_black_flat_frames_do_not_produce_false_match(self) -> None:
        rng = random.Random(99)
        step = 1.5
        black = [2.0] * 36

        def make_with_black_gap() -> list[list[float]]:
            frames = [_rand_vec(rng) for _ in range(120)]
            for idx in range(20, 60):  # 60s of solid black
                frames[idx] = list(black)
            return frames

        fps = [
            build_fingerprint("ep1", step, make_with_black_gap(), source="visual", min_std=8.0),
            build_fingerprint("ep2", step, make_with_black_gap(), source="visual", min_std=8.0),
        ]
        detected = find_shared_intros(fps)
        self.assertEqual(detected, {})

    def test_mkv_and_mp4_chapter_extraction_and_server_detection(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            mkv_path = base / "S01E01.mkv"
            mkv_path.write_bytes(
                _build_minimal_mkv_with_chapters(
                    [
                        (0.0, 25.0, "Cold Open"),
                        (25.0, 88.0, "Opening Credits"),
                        (88.0, 1200.0, "Act I"),
                    ]
                )
            )
            mkv_chapters = extract_mkv_chapters(mkv_path)
            self.assertEqual(len(mkv_chapters), 3)
            self.assertEqual(mkv_chapters[1].title, "Opening Credits")
            mkv_span = detect_intro_from_chapters(mkv_chapters, duration_sec=1200.0)
            assert mkv_span is not None
            self.assertAlmostEqual(mkv_span.start_sec, 25.0)
            self.assertAlmostEqual(mkv_span.end_sec, 88.0)
            self.assertEqual(mkv_span.source, "chapters")

            mp4_path = base / "S01E02.mp4"
            mp4_path.write_bytes(
                _build_minimal_mp4_with_chpl(
                    [
                        (0.0, "Teaser"),
                        (42.0, "Intro"),
                        (102.0, "Main Episode"),
                    ]
                )
            )
            mp4_chapters = extract_mp4_chapters(mp4_path)
            self.assertEqual(len(mp4_chapters), 3)
            mp4_span = detect_intro_from_chapters(mp4_chapters, duration_sec=1200.0)
            assert mp4_span is not None
            self.assertAlmostEqual(mp4_span.start_sec, 42.0)
            self.assertAlmostEqual(mp4_span.end_sec, 102.0)

            detected, method, needs_visual = detect_show_intros_server(
                [("ep1", mkv_path, 1200.0), ("ep2", mp4_path, 1200.0)],
                use_ffmpeg=False,
            )
            self.assertEqual(method, "chapters")
            self.assertFalse(needs_visual)
            self.assertEqual(set(detected.keys()), {"ep1", "ep2"})

    def test_chapter_bounds_reject_unreasonable_lengths(self) -> None:
        too_short = [ChapterMarker(0.0, 5.0, "Intro"), ChapterMarker(5.0, 1000.0, "Episode")]
        too_long = [ChapterMarker(0.0, 400.0, "Opening"), ChapterMarker(400.0, 1000.0, "Act 2")]
        self.assertIsNone(detect_intro_from_chapters(too_short, duration_sec=1000.0))
        self.assertIsNone(detect_intro_from_chapters(too_long, duration_sec=1000.0))

    def test_chapters_direct_import_and_intro_detector_reexports(self) -> None:
        import notflix.chapters as chapters_mod
        import notflix.intro_detector as detector_mod

        reexported = (
            "ChapterMarker",
            "INTRO_CHAPTER_RE",
            "extract_mkv_chapters",
            "extract_mp4_chapters",
            "extract_chapters",
            "detect_intro_from_chapters",
        )
        for name in reexported:
            self.assertIs(
                getattr(detector_mod, name),
                getattr(chapters_mod, name),
                f"Expected notflix.intro_detector.{name} to re-export notflix.chapters.{name}",
            )

        self.assertIsNotNone(chapters_mod.INTRO_CHAPTER_RE.search("Opening Credits"))
        with tempfile.TemporaryDirectory() as tmp:
            mkv_path = Path(tmp) / "ep.mkv"
            mkv_path.write_bytes(
                _build_minimal_mkv_with_chapters(
                    [(0.0, 20.0, "Cold Open"), (20.0, 75.0, "Intro"), (75.0, 1200.0, "Act 1")]
                )
            )
            extracted = chapters_mod.extract_chapters(mkv_path)
            self.assertEqual([c.title for c in extracted], ["Cold Open", "Intro", "Act 1"])
            span = chapters_mod.detect_intro_from_chapters(extracted, duration_sec=1200.0)
            assert span is not None
            self.assertAlmostEqual(span.start_sec, 20.0)
            self.assertAlmostEqual(span.end_sec, 75.0)



if __name__ == "__main__":
    unittest.main()
