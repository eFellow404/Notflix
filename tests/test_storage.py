"""Unit tests for SQLite persistence, linked-list roundtrip, and rescan retention."""

from __future__ import annotations

import sqlite3
import tempfile
import unittest
from pathlib import Path

from notflix.intro_detector import IntroSpan
from notflix.storage import NotflixStore


class TestNotflixStorage(unittest.TestCase):
    def test_add_persist_progress_and_rescan_preserves_timeplace(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            show_dir = base / "Firefly"
            show_dir.mkdir()
            for name in ["S01E02 - The Train Job.mp4", "S01E01 - Serenity.mp4", "S01E03 - Bushwhacked.mp4"]:
                (show_dir / name).write_bytes(b"video")

            db_path = base / "notflix.db"
            store = NotflixStore(db_path=db_path)

            show, ll = store.add_show(str(show_dir))
            self.assertEqual(show["name"], "Firefly")
            self.assertTrue(ll.validate_invariants())
            self.assertEqual(
                [n.filename for n in ll],
                [
                    "S01E01 - Serenity.mp4",
                    "S01E02 - The Train Job.mp4",
                    "S01E03 - Bushwhacked.mp4",
                ],
            )

            # Save timeplace on episode 1, then advance to episode 2 and save timeplace
            ep1 = ll.head
            assert ep1 is not None
            updated1 = store.update_progress(ep1.id, position_sec=142.5, duration_sec=2500.0)
            assert updated1 is not None
            self.assertAlmostEqual(updated1.position_sec, 142.5)
            self.assertFalse(updated1.watched)

            nxt_res = store.advance_next(show["id"])
            assert nxt_res is not None
            ep2, _ = nxt_res
            self.assertEqual(ep2.filename, "S01E02 - The Train Job.mp4")
            store.update_progress(ep2.id, position_sec=615.0, duration_sec=2600.0)

            # Reopen fresh NotflixStore instance from disk and verify full linked-list recovery
            store2 = NotflixStore(db_path=db_path)
            loaded = store2.get_show(show["id"])
            assert loaded is not None
            _, ll2 = loaded
            self.assertTrue(ll2.validate_invariants())
            assert ll2.current is not None
            self.assertEqual(ll2.current.filename, "S01E02 - The Train Job.mp4")
            self.assertAlmostEqual(ll2.current.position_sec, 615.0)
            assert ll2.current.prev is not None
            self.assertAlmostEqual(ll2.current.prev.position_sec, 142.5)

            # Add a new episode file and delete episode 3, then rescan
            (show_dir / "S01E04 - Shindig.mp4").write_bytes(b"video")
            (show_dir / "S01E03 - Bushwhacked.mp4").unlink()

            rescan = store2.rescan_show(show["id"])
            assert rescan is not None
            _, ll3, diff = rescan
            self.assertTrue(ll3.validate_invariants())
            self.assertEqual(diff["added"], 1)
            self.assertEqual(diff["removed"], 1)
            self.assertEqual(
                [n.filename for n in ll3],
                [
                    "S01E01 - Serenity.mp4",
                    "S01E02 - The Train Job.mp4",
                    "S01E04 - Shindig.mp4",
                ],
            )
            # Current episode and saved timestamps must survive rescan
            assert ll3.current is not None
            self.assertEqual(ll3.current.filename, "S01E02 - The Train Job.mp4")
            self.assertAlmostEqual(ll3.current.position_sec, 615.0)

    def test_manual_reorder_persists_and_reset_restores_smart_order(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            show_dir = base / "MiniShow"
            show_dir.mkdir()
            for i in (1, 2, 3):
                (show_dir / f"Episode {i}.mp4").write_bytes(b"video")

            store = NotflixStore(db_path=base / "db.sqlite")
            show, ll = store.add_show(str(show_dir))
            ep3_id = ll.tail.id  # type: ignore[union-attr]

            reordered = store.reorder_episode(show["id"], ep3_id, 0)
            assert reordered is not None
            self.assertTrue(reordered.validate_invariants())
            self.assertEqual(
                [n.filename for n in reordered],
                ["Episode 3.mp4", "Episode 1.mp4", "Episode 2.mp4"],
            )

            # Normal rescan preserves manual order
            res = store.rescan_show(show["id"], reset_order=False)
            assert res is not None
            _, ll_kept, _ = res
            self.assertEqual(
                [n.filename for n in ll_kept],
                ["Episode 3.mp4", "Episode 1.mp4", "Episode 2.mp4"],
            )

            # Reset order restores smart sort
            res_reset = store.rescan_show(show["id"], reset_order=True)
            assert res_reset is not None
            _, ll_reset, _ = res_reset
            self.assertEqual(
                [n.filename for n in ll_reset],
                ["Episode 1.mp4", "Episode 2.mp4", "Episode 3.mp4"],
            )

    def test_intro_persistence_migration_and_rescan_preservation(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            db_path = base / "legacy.db"

            # Simulate an older Notflix database created before intro columns existed
            with sqlite3.connect(str(db_path)) as conn:
                conn.executescript(
                    """
                    CREATE TABLE shows (
                        id TEXT PRIMARY KEY,
                        name TEXT NOT NULL,
                        folder_path TEXT NOT NULL UNIQUE,
                        head_episode_id TEXT,
                        tail_episode_id TEXT,
                        current_episode_id TEXT,
                        manual_order INTEGER NOT NULL DEFAULT 0,
                        created_at TEXT NOT NULL,
                        last_scanned_at TEXT NOT NULL,
                        last_watched_at TEXT
                    );
                    CREATE TABLE episodes (
                        id TEXT PRIMARY KEY,
                        show_id TEXT NOT NULL REFERENCES shows(id) ON DELETE CASCADE,
                        file_path TEXT NOT NULL,
                        relative_path TEXT NOT NULL,
                        filename TEXT NOT NULL,
                        title TEXT NOT NULL,
                        season INTEGER,
                        episode_number INTEGER,
                        disc INTEGER,
                        is_extra INTEGER NOT NULL DEFAULT 0,
                        confidence REAL NOT NULL DEFAULT 0.0,
                        sort_reason TEXT NOT NULL DEFAULT '',
                        prev_episode_id TEXT,
                        next_episode_id TEXT,
                        position_sec REAL NOT NULL DEFAULT 0.0,
                        duration_sec REAL NOT NULL DEFAULT 0.0,
                        watched INTEGER NOT NULL DEFAULT 0,
                        file_size INTEGER NOT NULL DEFAULT 0,
                        UNIQUE(show_id, relative_path)
                    );
                    """
                )

            show_dir = base / "IntroShow"
            show_dir.mkdir()
            (show_dir / "S01E01.mp4").write_bytes(b"video")
            (show_dir / "S01E02.mp4").write_bytes(b"video")

            store = NotflixStore(db_path=db_path)
            show, ll = store.add_show(str(show_dir))
            eps = ll.to_list()
            ep1, ep2 = eps[0], eps[1]

            # Manually set ep1 intro and auto-save ep2 intro
            manual_ep = store.set_episode_intro(ep1.id, 12.0, 72.0, confidence=1.0, source="manual")
            assert manual_ep is not None
            self.assertAlmostEqual(manual_ep.intro_start_sec or 0.0, 12.0)
            self.assertEqual(manual_ep.intro_source, "manual")

            saved = store.save_show_intros(
                show["id"],
                {
                    ep1.id: IntroSpan(30.0, 90.0, 0.9, "visual"),
                    ep2.id: IntroSpan(45.0, 105.0, 0.92, "visual"),
                },
                preserve_manual=True,
                mark_scanned=True,
            )
            assert saved is not None
            updated_show, updated_ll = saved
            self.assertIsNotNone(updated_show["intro_scanned_at"])
            # Manual intro on ep1 preserved; ep2 got auto-detected intro
            self.assertAlmostEqual(updated_ll.get(ep1.id).intro_start_sec or 0.0, 12.0)  # type: ignore[union-attr]
            self.assertAlmostEqual(updated_ll.get(ep2.id).intro_start_sec or 0.0, 45.0)  # type: ignore[union-attr]

            # Rescanning folder preserves intro timestamps
            rescan = store.rescan_show(show["id"])
            assert rescan is not None
            _, rescanned_ll, _ = rescan
            self.assertAlmostEqual(rescanned_ll.get(ep1.id).intro_end_sec or 0.0, 72.0)  # type: ignore[union-attr]
            self.assertAlmostEqual(rescanned_ll.get(ep2.id).intro_end_sec or 0.0, 105.0)  # type: ignore[union-attr]

    def test_list_shows_returns_accurate_stats_across_multiple_shows(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            show_a_dir = base / "ShowAlpha"
            show_a_dir.mkdir()
            for name in [
                "S01E01 - Pilot.mp4",
                "S01E02 - Second.mp4",
                "S02E01 - Return.mp4",
                "Behind The Scenes Extra.mp4",
            ]:
                (show_a_dir / name).write_bytes(b"video")

            show_b_dir = base / "ShowBeta"
            show_b_dir.mkdir()
            for name in ["S01E01.mp4", "S01E02.mp4"]:
                (show_b_dir / name).write_bytes(b"video")

            show_c_dir = base / "ShowEmpty"
            show_c_dir.mkdir()

            store = NotflixStore(db_path=base / "multi.db")
            show_a, ll_a = store.add_show(str(show_a_dir))
            show_b, ll_b = store.add_show(str(show_b_dir))
            show_c, _ = store.add_show(str(show_c_dir))

            eps_a = ll_a.to_list()
            store.update_progress(eps_a[0].id, position_sec=1190.0, duration_sec=1200.0, watched=True)
            store.set_episode_intro(eps_a[0].id, 10.0, 65.0, confidence=1.0, source="manual")
            store.advance_next(show_a["id"])

            eps_b = ll_b.to_list()
            store.update_progress(eps_b[0].id, position_sec=50.0, duration_sec=1000.0)

            listed = store.list_shows()
            self.assertEqual(len(listed), 3)
            by_id = {s["id"]: s for s in listed}

            a = by_id[show_a["id"]]
            self.assertEqual(a["episode_count"], 4)
            self.assertEqual(a["main_episode_count"], 3)
            self.assertEqual(a["extra_count"], 1)
            self.assertEqual(a["season_count"], 2)
            self.assertEqual(a["watched_count"], 1)
            self.assertEqual(a["intros_detected_count"], 1)
            self.assertEqual(a["current_episode"]["id"], eps_a[1].id)
            self.assertEqual(a["next_episode"]["id"], eps_a[2].id)

            b = by_id[show_b["id"]]
            self.assertEqual(b["episode_count"], 2)
            self.assertEqual(b["main_episode_count"], 2)
            self.assertEqual(b["extra_count"], 0)
            self.assertEqual(b["season_count"], 1)
            self.assertEqual(b["watched_count"], 0)
            self.assertEqual(b["intros_detected_count"], 0)
            self.assertEqual(b["current_episode"]["id"], eps_b[0].id)
            self.assertEqual(b["next_episode"]["id"], eps_b[1].id)

            c = by_id[show_c["id"]]
            self.assertEqual(c["episode_count"], 0)
            self.assertEqual(c["main_episode_count"], 0)
            self.assertEqual(c["season_count"], 0)
            self.assertIsNone(c["current_episode"])
            self.assertIsNone(c["next_episode"])



if __name__ == "__main__":
    unittest.main()
