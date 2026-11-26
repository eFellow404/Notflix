"""Unit tests for SQLite persistence, linked-list roundtrip, and rescan retention."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

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


if __name__ == "__main__":
    unittest.main()
