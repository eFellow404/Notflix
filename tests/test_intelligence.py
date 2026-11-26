"""Unit tests for the intelligent DVD-rip episode ordering engine."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from notflix.intelligence import scan_and_order_folder


def _populate(root: Path, rel_paths: list[str]) -> None:
    for rel in rel_paths:
        target = root / rel
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(b"fake-video-data")


class TestEpisodeIntelligence(unittest.TestCase):
    def test_standard_sxxexx_and_noise_stripping(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _populate(
                root,
                [
                    "Show.2004.1080p.x264.S01E10.Finale.mkv",
                    "Show.2004.720p.DVDRip.S01E02.Second.mkv",
                    "Show.2004.480p.AC3.S01E01.Pilot.mkv",
                    "Show.2004.S02E01.Return.mkv",
                    "Show.2004.1x03.Third.mp4",
                ],
            )
            ordered = scan_and_order_folder(root)
            self.assertEqual(
                [e.filename for e in ordered],
                [
                    "Show.2004.480p.AC3.S01E01.Pilot.mkv",
                    "Show.2004.720p.DVDRip.S01E02.Second.mkv",
                    "Show.2004.1x03.Third.mp4",
                    "Show.2004.1080p.x264.S01E10.Finale.mkv",
                    "Show.2004.S02E01.Return.mkv",
                ],
            )
            self.assertEqual(ordered[0].season, 1)
            self.assertEqual(ordered[0].episode_number, 1)
            self.assertEqual(ordered[-1].season, 2)
            self.assertEqual(ordered[-1].episode_number, 1)

    def test_makemkv_disc_folders(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _populate(
                root,
                [
                    "Season 1/Disc 2/title_t01.mkv",
                    "Season 1/Disc 1/title_t02.mkv",
                    "Season 1/Disc 1/title_t00.mkv",
                    "Season 1/Disc 2/title_t00.mkv",
                    "Season 1/Disc 1/title_t01.mkv",
                    "Season 1/Extras/Behind_the_Scenes.mkv",
                ],
            )
            ordered = scan_and_order_folder(root)
            self.assertEqual(
                [e.relative_path for e in ordered],
                [
                    "Season 1/Disc 1/title_t00.mkv",
                    "Season 1/Disc 1/title_t01.mkv",
                    "Season 1/Disc 1/title_t02.mkv",
                    "Season 1/Disc 2/title_t00.mkv",
                    "Season 1/Disc 2/title_t01.mkv",
                    "Season 1/Extras/Behind_the_Scenes.mkv",
                ],
            )
            self.assertTrue(ordered[-1].is_extra)
            self.assertFalse(ordered[0].is_extra)

    def test_natural_number_ordering_beats_lexicographic(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _populate(root, ["ep10.mp4", "ep2.mp4", "ep1.mp4", "ep11.mp4", "ep3.mp4"])
            ordered = scan_and_order_folder(root)
            self.assertEqual(
                [e.filename for e in ordered],
                ["ep1.mp4", "ep2.mp4", "ep3.mp4", "ep10.mp4", "ep11.mp4"],
            )

    def test_three_digit_see_consensus(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _populate(
                root,
                [
                    "202 - Winter.mp4",
                    "102 - Gathering.mp4",
                    "201 - New Dawn.mp4",
                    "101 - Beginning.mp4",
                ],
            )
            ordered = scan_and_order_folder(root)
            self.assertEqual(
                [(e.season, e.episode_number, e.filename) for e in ordered],
                [
                    (1, 1, "101 - Beginning.mp4"),
                    (1, 2, "102 - Gathering.mp4"),
                    (2, 1, "201 - New Dawn.mp4"),
                    (2, 2, "202 - Winter.mp4"),
                ],
            )

    def test_numeric_show_title_ignored_via_sibling_consensus(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _populate(
                root,
                [
                    "24 - 10 - Ten AM.mp4",
                    "24 - 02 - Two AM.mp4",
                    "24 - 01 - One AM.mp4",
                    "24 - 03 - Three AM.mp4",
                ],
            )
            ordered = scan_and_order_folder(root)
            self.assertEqual(
                [e.filename for e in ordered],
                [
                    "24 - 01 - One AM.mp4",
                    "24 - 02 - Two AM.mp4",
                    "24 - 03 - Three AM.mp4",
                    "24 - 10 - Ten AM.mp4",
                ],
            )
            self.assertEqual([e.episode_number for e in ordered], [1, 2, 3, 10])

    def test_roman_numerals_pilot_and_bonus_separation(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _populate(
                root,
                [
                    "Deleted Scenes.mp4",
                    "Miniseries Part III.mp4",
                    "Miniseries Part I.mp4",
                    "Miniseries Part II.mp4",
                    "Pilot.mp4",
                ],
            )
            ordered = scan_and_order_folder(root)
            self.assertEqual(
                [e.filename for e in ordered],
                [
                    "Pilot.mp4",
                    "Miniseries Part I.mp4",
                    "Miniseries Part II.mp4",
                    "Miniseries Part III.mp4",
                    "Deleted Scenes.mp4",
                ],
            )
            self.assertTrue(ordered[-1].is_extra)


if __name__ == "__main__":
    unittest.main()
