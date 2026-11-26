"""Integration tests for the Notflix HTTP server, REST API, and byte-range streaming."""

from __future__ import annotations

import http.client
import json
import tempfile
import threading
import unittest
from pathlib import Path
from urllib.error import HTTPError
from urllib.request import Request, urlopen

from notflix.server import NotflixHTTPServer
from notflix.storage import NotflixStore


class TestNotflixServer(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.base = Path(self._tmp.name)
        self.show_dir = self.base / "SampleTV"
        self.show_dir.mkdir()
        (self.show_dir / "S01E02.mp4").write_bytes(b"SECOND-EPISODE-BYTES-0123456789")
        (self.show_dir / "S01E01.mp4").write_bytes(b"FIRST-EPISODE-BYTES-ABCDEFGHIJ")

        self.store = NotflixStore(db_path=self.base / "test.db")
        self.server = NotflixHTTPServer(("127.0.0.1", 0), store=self.store, quiet=True)
        self.port = self.server.server_address[1]
        self.base_url = f"http://127.0.0.1:{self.port}"
        self._thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self._thread.start()

    def tearDown(self) -> None:
        self.server.shutdown()
        self.server.server_close()
        self._thread.join(timeout=3)
        self._tmp.cleanup()

    def _req_json(self, method: str, path: str, payload: dict | None = None) -> tuple[int, dict]:
        data = json.dumps(payload).encode("utf-8") if payload is not None else None
        headers = {"Content-Type": "application/json"} if data is not None else {}
        req = Request(f"{self.base_url}{path}", data=data, headers=headers, method=method)
        with urlopen(req, timeout=5) as resp:
            return resp.status, json.loads(resp.read().decode("utf-8"))

    def test_full_api_flow_and_range_streaming(self) -> None:
        # 1. Health check & UI shell
        status, health = self._req_json("GET", "/api/health")
        self.assertEqual(status, 200)
        self.assertEqual(health["status"], "ok")

        with urlopen(f"{self.base_url}/", timeout=5) as resp:
            html = resp.read().decode("utf-8")
            self.assertIn("NOTFLIX", html)

        # 2. Browse filesystem endpoint
        status, browse = self._req_json("GET", f"/api/fs/browse?path={self.show_dir}")
        self.assertEqual(status, 200)
        self.assertEqual(browse["direct_video_count"], 2)

        # 3. Add TV folder -> intelligently ordered linked list
        status, created = self._req_json(
            "POST",
            "/api/shows",
            {"folder_path": str(self.show_dir), "name": "Sample Show"},
        )
        self.assertEqual(status, 201)
        show_id = created["show"]["id"]
        eps = created["linked_list"]["episodes"]
        self.assertEqual([e["filename"] for e in eps], ["S01E01.mp4", "S01E02.mp4"])
        self.assertEqual(eps[0]["next_id"], eps[1]["id"])
        self.assertEqual(eps[1]["prev_id"], eps[0]["id"])

        ep1_id = eps[0]["id"]
        ep2_id = eps[1]["id"]

        # 4. Save playback timeplace via PATCH and POST (sendBeacon compatibility)
        status, prog = self._req_json(
            "PATCH",
            f"/api/episodes/{ep1_id}/progress",
            {"position_sec": 42.75, "duration_sec": 1200.0},
        )
        self.assertEqual(status, 200)
        self.assertAlmostEqual(prog["episode"]["position_sec"], 42.75)

        status, prog2 = self._req_json(
            "POST",
            f"/api/episodes/{ep1_id}/progress",
            {"position_sec": 88.5, "duration_sec": 1200.0},
        )
        self.assertEqual(status, 200)
        self.assertAlmostEqual(prog2["episode"]["position_sec"], 88.5)

        # 5. Click Next Episode -> advances linked list current pointer
        status, nxt = self._req_json("POST", f"/api/shows/{show_id}/next", {})
        self.assertEqual(status, 200)
        self.assertEqual(nxt["episode"]["id"], ep2_id)
        self.assertEqual(nxt["linked_list"]["current_id"], ep2_id)

        # 6. Click Prev Episode -> returns to ep1 with saved timestamp intact
        status, prv = self._req_json("POST", f"/api/shows/{show_id}/prev", {})
        self.assertEqual(status, 200)
        self.assertEqual(prv["episode"]["id"], ep1_id)
        self.assertAlmostEqual(prv["episode"]["position_sec"], 88.5)

        # 7. HTTP 206 Partial Content byte-range streaming
        range_req = Request(
            f"{self.base_url}/api/episodes/{ep1_id}/stream",
            headers={"Range": "bytes=0-12"},
        )
        with urlopen(range_req, timeout=5) as resp:
            self.assertEqual(resp.status, 206)
            self.assertEqual(resp.headers.get("Accept-Ranges"), "bytes")
            self.assertEqual(resp.headers.get("Content-Range"), "bytes 0-12/30")
            self.assertEqual(resp.read(), b"FIRST-EPISODE")

        # 8. Invalid byte range returns 416
        bad_range = Request(
            f"{self.base_url}/api/episodes/{ep1_id}/stream",
            headers={"Range": "bytes=999-1000"},
        )
        with self.assertRaises(HTTPError) as ctx:
            urlopen(bad_range, timeout=5)
        self.assertEqual(ctx.exception.code, 416)

        # 9. Intro detection endpoint (server pass -> requests client visual scan without ffmpeg/chapters)
        status, det = self._req_json("POST", f"/api/shows/{show_id}/detect-intros", {})
        self.assertEqual(status, 200)
        self.assertIn("needs_client_visual_scan", det)

        # 10. Intro detection with client-supplied visual fingerprints
        import random

        rng = random.Random(7)
        tmpl = [[rng.uniform(20.0, 235.0) for _ in range(36)] for _ in range(20)]
        ep1_vecs = [[rng.uniform(20.0, 235.0) for _ in range(36)] for _ in range(80)]
        ep2_vecs = [[rng.uniform(20.0, 235.0) for _ in range(36)] for _ in range(80)]
        for idx, vec in enumerate(tmpl):
            ep1_vecs[10 + idx] = vec
            ep2_vecs[30 + idx] = vec

        status, vis = self._req_json(
            "POST",
            f"/api/shows/{show_id}/detect-intros",
            {
                "fingerprints": [
                    {"episode_id": ep1_id, "step_sec": 1.5, "vectors": ep1_vecs},
                    {"episode_id": ep2_id, "step_sec": 1.5, "vectors": ep2_vecs},
                ],
                "mark_scanned": True,
            },
        )
        self.assertEqual(status, 200)
        self.assertEqual(vis["method"], "visual")
        self.assertEqual(vis["detected_count"], 2)
        ep_by_id = {e["id"]: e for e in vis["linked_list"]["episodes"]}
        self.assertAlmostEqual(ep_by_id[ep1_id]["intro_start_sec"], 15.0, delta=2.0)
        self.assertAlmostEqual(ep_by_id[ep2_id]["intro_start_sec"], 45.0, delta=2.0)

        # 11. Manual intro override & clear
        status, manual = self._req_json(
            "PATCH",
            f"/api/episodes/{ep1_id}/intro",
            {"intro_start_sec": 20.0, "intro_end_sec": 80.0},
        )
        self.assertEqual(status, 200)
        self.assertAlmostEqual(manual["episode"]["intro_start_sec"], 20.0)
        self.assertEqual(manual["episode"]["intro_source"], "manual")
        self.assertEqual(manual["episode"]["next_id"], ep2_id)

        status, cleared = self._req_json(
            "PATCH",
            f"/api/episodes/{ep1_id}/intro",
            {"clear": True},
        )
        self.assertEqual(status, 200)
        self.assertIsNone(cleared["episode"]["intro_start_sec"])

        # 12. Progress PATCH preserves next_id and watched toggle does not hijack current_episode_id
        status, watched_ep2 = self._req_json(
            "PATCH",
            f"/api/episodes/{ep2_id}/progress",
            {"watched": True, "set_current": False},
        )
        self.assertEqual(status, 200)
        self.assertTrue(watched_ep2["episode"]["watched"])
        self.assertEqual(watched_ep2["episode"]["prev_id"], ep1_id)

        status, show_after = self._req_json("GET", f"/api/shows/{show_id}")
        self.assertEqual(status, 200)
        self.assertEqual(show_after["linked_list"]["current_id"], ep1_id)

        # 13. Bulk season watched endpoint + show rename + season summary stats
        status, bulk = self._req_json(
            "POST",
            f"/api/shows/{show_id}/watched",
            {"episode_ids": [ep1_id, ep2_id], "watched": False, "reset_position": True},
        )
        self.assertEqual(status, 200)
        self.assertTrue(all(not e["watched"] for e in bulk["linked_list"]["episodes"]))

        status, renamed = self._req_json(
            "PATCH",
            f"/api/shows/{show_id}",
            {"name": "Renamed Sample Show"},
        )
        self.assertEqual(status, 200)
        self.assertEqual(renamed["show"]["name"], "Renamed Sample Show")

        status, shows_list = self._req_json("GET", "/api/shows")
        self.assertEqual(status, 200)
        self.assertEqual(shows_list["shows"][0]["season_count"], 1)
        self.assertEqual(shows_list["shows"][0]["main_episode_count"], 2)

    def test_static_js_serving_missing_and_path_traversal(self) -> None:
        with urlopen(f"{self.base_url}/static/js/utils.js", timeout=5) as resp:
            self.assertEqual(resp.status, 200)
            self.assertIn("application/javascript", resp.headers.get("Content-Type", ""))
            body = resp.read().decode("utf-8")
            self.assertGreater(len(body), 0)

        with self.assertRaises(HTTPError) as missing_ctx:
            urlopen(f"{self.base_url}/static/js/does-not-exist.js", timeout=5)
        self.assertEqual(missing_ctx.exception.code, 404)

        conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=5)
        try:
            conn.request("GET", "/static/js/../../run.py")
            resp = conn.getresponse()
            resp.read()
            self.assertEqual(resp.status, 403)
        finally:
            conn.close()

    def test_update_endpoints_never_crash_when_non_repo_or_offline(self) -> None:
        # Point server repo_dir to a plain temp dir (non-git) and verify clean JSON responses
        self.server.repo_dir = self.base
        status, info = self._req_json("GET", "/api/update/status")
        self.assertEqual(status, 200)
        self.assertEqual(info["status"], "not_a_repo")

        status, checked = self._req_json("POST", "/api/update/check", {"timeout_sec": 2})
        self.assertEqual(status, 200)
        self.assertEqual(checked["status"], "not_a_repo")

        status, applied = self._req_json("POST", "/api/update/apply", {"timeout_sec": 2})
        self.assertEqual(status, 200)
        self.assertFalse(applied["updated"])


if __name__ == "__main__":
    unittest.main()
