"""Integration tests for the Notflix HTTP server, REST API, and byte-range streaming."""

from __future__ import annotations

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


if __name__ == "__main__":
    unittest.main()
