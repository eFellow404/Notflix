"""SQLite persistence for TV shows, persisted doubly linked lists, and playback state."""

from __future__ import annotations

import os
import sqlite3
import threading
import uuid
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

from notflix.intelligence import AnalyzedEpisode, scan_and_order_folder
from notflix.linked_list import EpisodeLinkedList, EpisodeNode

WATCHED_THRESHOLD = 0.90


def _utc_now() -> str:
    return datetime.now(timezone.utc).isoformat()


def default_db_path() -> Path:
    env_path = os.environ.get("NOTFLIX_DB_PATH")
    if env_path:
        return Path(env_path).expanduser().resolve()
    return Path.home() / ".local" / "share" / "notflix" / "notflix.db"


class NotflixStore:
    """Thread-safe SQLite store persisting shows, episode linked lists, and timestamps."""

    def __init__(self, db_path: Optional[str | Path] = None) -> None:
        self.db_path = Path(db_path).expanduser().resolve() if db_path else default_db_path()
        self.db_path.parent.mkdir(parents=True, exist_ok=True)
        self._lock = threading.RLock()
        self._init_schema()

    def _connect(self) -> sqlite3.Connection:
        conn = sqlite3.connect(str(self.db_path), timeout=15.0)
        conn.row_factory = sqlite3.Row
        conn.execute("PRAGMA foreign_keys = ON")
        conn.execute("PRAGMA journal_mode = WAL")
        return conn

    def _init_schema(self) -> None:
        with self._lock, self._connect() as conn:
            conn.executescript(
                """
                CREATE TABLE IF NOT EXISTS shows (
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

                CREATE TABLE IF NOT EXISTS episodes (
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

                CREATE INDEX IF NOT EXISTS idx_episodes_show_id ON episodes(show_id);
                """
            )

    @staticmethod
    def _row_to_node(row: sqlite3.Row) -> EpisodeNode:
        return EpisodeNode(
            id=row["id"],
            show_id=row["show_id"],
            file_path=row["file_path"],
            relative_path=row["relative_path"],
            filename=row["filename"],
            title=row["title"],
            season=row["season"],
            episode_number=row["episode_number"],
            disc=row["disc"],
            is_extra=bool(row["is_extra"]),
            confidence=float(row["confidence"]),
            sort_reason=row["sort_reason"],
            position_sec=float(row["position_sec"]),
            duration_sec=float(row["duration_sec"]),
            watched=bool(row["watched"]),
            file_size=int(row["file_size"]),
        )

    @staticmethod
    def _row_to_show(row: sqlite3.Row) -> dict:
        return {
            "id": row["id"],
            "name": row["name"],
            "folder_path": row["folder_path"],
            "head_episode_id": row["head_episode_id"],
            "tail_episode_id": row["tail_episode_id"],
            "current_episode_id": row["current_episode_id"],
            "manual_order": bool(row["manual_order"]),
            "created_at": row["created_at"],
            "last_scanned_at": row["last_scanned_at"],
            "last_watched_at": row["last_watched_at"],
        }

    def _load_linked_list(self, conn: sqlite3.Connection, show_row: sqlite3.Row) -> EpisodeLinkedList:
        ep_rows = conn.execute(
            "SELECT * FROM episodes WHERE show_id = ?",
            (show_row["id"],),
        ).fetchall()
        nodes = [self._row_to_node(r) for r in ep_rows]
        next_map = {r["id"]: r["next_episode_id"] for r in ep_rows}
        return EpisodeLinkedList.from_persisted(
            nodes=nodes,
            next_map=next_map,
            head_id=show_row["head_episode_id"],
            current_id=show_row["current_episode_id"],
        )

    def _persist_linked_list(
        self,
        conn: sqlite3.Connection,
        show_id: str,
        ll: EpisodeLinkedList,
        manual_order: Optional[bool] = None,
        scanned_at: Optional[str] = None,
    ) -> None:
        conn.execute("DELETE FROM episodes WHERE show_id = ?", (show_id,))
        for node in ll:
            conn.execute(
                """
                INSERT INTO episodes (
                    id, show_id, file_path, relative_path, filename, title,
                    season, episode_number, disc, is_extra, confidence, sort_reason,
                    prev_episode_id, next_episode_id, position_sec, duration_sec,
                    watched, file_size
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    node.id,
                    show_id,
                    node.file_path,
                    node.relative_path,
                    node.filename,
                    node.title,
                    node.season,
                    node.episode_number,
                    node.disc,
                    1 if node.is_extra else 0,
                    node.confidence,
                    node.sort_reason,
                    node.prev_id,
                    node.next_id,
                    node.position_sec,
                    node.duration_sec,
                    1 if node.watched else 0,
                    node.file_size,
                ),
            )

        updates = [
            "head_episode_id = ?",
            "tail_episode_id = ?",
            "current_episode_id = ?",
        ]
        params: list = [
            ll.head.id if ll.head else None,
            ll.tail.id if ll.tail else None,
            ll.current.id if ll.current else None,
        ]
        if manual_order is not None:
            updates.append("manual_order = ?")
            params.append(1 if manual_order else 0)
        if scanned_at is not None:
            updates.append("last_scanned_at = ?")
            params.append(scanned_at)
        params.append(show_id)

        conn.execute(f"UPDATE shows SET {', '.join(updates)} WHERE id = ?", params)

    @staticmethod
    def _build_node(
        show_id: str,
        analyzed: AnalyzedEpisode,
        existing: Optional[EpisodeNode] = None,
    ) -> EpisodeNode:
        return EpisodeNode(
            id=existing.id if existing else str(uuid.uuid4()),
            show_id=show_id,
            file_path=analyzed.file_path,
            relative_path=analyzed.relative_path,
            filename=analyzed.filename,
            title=analyzed.title,
            season=analyzed.season,
            episode_number=analyzed.episode_number,
            disc=analyzed.disc,
            is_extra=analyzed.is_extra,
            confidence=analyzed.confidence,
            sort_reason=analyzed.sort_reason,
            position_sec=existing.position_sec if existing else 0.0,
            duration_sec=existing.duration_sec if existing else 0.0,
            watched=existing.watched if existing else False,
            file_size=analyzed.file_size,
        )

    def add_show(
        self, folder_path: str, name: Optional[str] = None
    ) -> tuple[dict, EpisodeLinkedList]:
        resolved = Path(folder_path).expanduser().resolve()
        if not resolved.exists() or not resolved.is_dir():
            raise FileNotFoundError(f"Folder not found: {folder_path}")

        analyzed = scan_and_order_folder(resolved)
        show_name = (name or "").strip() or resolved.name
        now = _utc_now()

        with self._lock, self._connect() as conn:
            existing = conn.execute(
                "SELECT id FROM shows WHERE folder_path = ?",
                (str(resolved),),
            ).fetchone()
            if existing is not None:
                raise ValueError(f"Folder is already in your library: {resolved}")

            show_id = str(uuid.uuid4())
            conn.execute(
                """
                INSERT INTO shows (
                    id, name, folder_path, head_episode_id, tail_episode_id,
                    current_episode_id, manual_order, created_at, last_scanned_at, last_watched_at
                ) VALUES (?, ?, ?, NULL, NULL, NULL, 0, ?, ?, NULL)
                """,
                (show_id, show_name, str(resolved), now, now),
            )

            nodes = [self._build_node(show_id, item) for item in analyzed]
            ll = EpisodeLinkedList.from_ordered_nodes(nodes)
            self._persist_linked_list(conn, show_id, ll, manual_order=False, scanned_at=now)

            show_row = conn.execute("SELECT * FROM shows WHERE id = ?", (show_id,)).fetchone()
            return self._row_to_show(show_row), ll

    def list_shows(self) -> list[dict]:
        with self._lock, self._connect() as conn:
            rows = conn.execute(
                """
                SELECT * FROM shows
                ORDER BY
                    CASE WHEN last_watched_at IS NULL THEN 1 ELSE 0 END,
                    last_watched_at DESC,
                    created_at DESC
                """
            ).fetchall()

            results: list[dict] = []
            for row in rows:
                ll = self._load_linked_list(conn, row)
                show = self._row_to_show(row)
                episodes = ll.to_list()
                watched_count = sum(1 for ep in episodes if ep.watched)
                current_idx = ll.index_of(ll.current.id) if ll.current else -1
                show.update(
                    {
                        "episode_count": len(ll),
                        "watched_count": watched_count,
                        "current_episode": ll.current.to_dict(index=current_idx)
                        if ll.current
                        else None,
                        "next_episode": ll.peek_next().to_dict(index=current_idx + 1)
                        if ll.peek_next()
                        else None,
                        "folder_exists": Path(row["folder_path"]).is_dir(),
                    }
                )
                results.append(show)
            return results

    def get_show(self, show_id: str) -> Optional[tuple[dict, EpisodeLinkedList]]:
        with self._lock, self._connect() as conn:
            row = conn.execute("SELECT * FROM shows WHERE id = ?", (show_id,)).fetchone()
            if row is None:
                return None
            ll = self._load_linked_list(conn, row)
            show = self._row_to_show(row)
            show["folder_exists"] = Path(row["folder_path"]).is_dir()
            return show, ll

    def delete_show(self, show_id: str) -> bool:
        with self._lock, self._connect() as conn:
            cur = conn.execute("DELETE FROM shows WHERE id = ?", (show_id,))
            return cur.rowcount > 0

    def rescan_show(
        self, show_id: str, reset_order: bool = False
    ) -> Optional[tuple[dict, EpisodeLinkedList, dict]]:
        """Re-check a show folder for new/removed files and update its linked list.

        Preserves saved playback timestamps, watched status, and current episode pointer.
        """
        with self._lock, self._connect() as conn:
            row = conn.execute("SELECT * FROM shows WHERE id = ?", (show_id,)).fetchone()
            if row is None:
                return None

            existing_ll = self._load_linked_list(conn, row)
            old_by_rel = {node.relative_path: node for node in existing_ll}
            old_current_rel = existing_ll.current.relative_path if existing_ll.current else None

            analyzed = scan_and_order_folder(row["folder_path"])
            new_by_rel = {item.relative_path: item for item in analyzed}

            added = [r for r in new_by_rel if r not in old_by_rel]
            removed = [r for r in old_by_rel if r not in new_by_rel]
            unchanged = [r for r in new_by_rel if r in old_by_rel]

            keep_manual = bool(row["manual_order"]) and not reset_order

            if keep_manual:
                # Preserve the user's manual linked-list order for surviving files,
                # and append newly discovered files in their smart-sorted order.
                ordered_nodes: list[EpisodeNode] = []
                for old_node in existing_ll:
                    if old_node.relative_path in new_by_rel:
                        item = new_by_rel[old_node.relative_path]
                        ordered_nodes.append(self._build_node(show_id, item, existing=old_node))
                for item in analyzed:
                    if item.relative_path not in old_by_rel:
                        ordered_nodes.append(self._build_node(show_id, item))
                manual_flag = True
            else:
                ordered_nodes = [
                    self._build_node(show_id, item, existing=old_by_rel.get(item.relative_path))
                    for item in analyzed
                ]
                manual_flag = False

            new_current_id: Optional[str] = None
            if old_current_rel is not None:
                for node in ordered_nodes:
                    if node.relative_path == old_current_rel:
                        new_current_id = node.id
                        break

            new_ll = EpisodeLinkedList.from_ordered_nodes(ordered_nodes, current_id=new_current_id)
            now = _utc_now()
            self._persist_linked_list(
                conn,
                show_id,
                new_ll,
                manual_order=manual_flag,
                scanned_at=now,
            )

            updated_row = conn.execute("SELECT * FROM shows WHERE id = ?", (show_id,)).fetchone()
            diff = {
                "added": len(added),
                "removed": len(removed),
                "unchanged": len(unchanged),
                "total": len(new_ll),
            }
            return self._row_to_show(updated_row), new_ll, diff

    def select_episode(self, show_id: str, episode_id: str) -> Optional[EpisodeNode]:
        with self._lock, self._connect() as conn:
            row = conn.execute("SELECT * FROM shows WHERE id = ?", (show_id,)).fetchone()
            if row is None:
                return None
            ll = self._load_linked_list(conn, row)
            node = ll.set_current(episode_id)
            if node is None:
                return None
            conn.execute(
                "UPDATE shows SET current_episode_id = ?, last_watched_at = ? WHERE id = ?",
                (node.id, _utc_now(), show_id),
            )
            return node

    def advance_next(self, show_id: str) -> Optional[tuple[EpisodeNode, EpisodeLinkedList]]:
        with self._lock, self._connect() as conn:
            row = conn.execute("SELECT * FROM shows WHERE id = ?", (show_id,)).fetchone()
            if row is None:
                return None
            ll = self._load_linked_list(conn, row)
            nxt = ll.advance_next()
            if nxt is None:
                return None
            conn.execute(
                "UPDATE shows SET current_episode_id = ?, last_watched_at = ? WHERE id = ?",
                (nxt.id, _utc_now(), show_id),
            )
            return nxt, ll

    def step_prev(self, show_id: str) -> Optional[tuple[EpisodeNode, EpisodeLinkedList]]:
        with self._lock, self._connect() as conn:
            row = conn.execute("SELECT * FROM shows WHERE id = ?", (show_id,)).fetchone()
            if row is None:
                return None
            ll = self._load_linked_list(conn, row)
            prv = ll.step_prev()
            if prv is None:
                return None
            conn.execute(
                "UPDATE shows SET current_episode_id = ?, last_watched_at = ? WHERE id = ?",
                (prv.id, _utc_now(), show_id),
            )
            return prv, ll

    def reorder_episode(
        self, show_id: str, episode_id: str, target_index: int
    ) -> Optional[EpisodeLinkedList]:
        with self._lock, self._connect() as conn:
            row = conn.execute("SELECT * FROM shows WHERE id = ?", (show_id,)).fetchone()
            if row is None:
                return None
            ll = self._load_linked_list(conn, row)
            if not ll.move_node(episode_id, target_index):
                return None
            self._persist_linked_list(conn, show_id, ll, manual_order=True)
            return ll

    def get_episode(self, episode_id: str) -> Optional[EpisodeNode]:
        with self._lock, self._connect() as conn:
            row = conn.execute("SELECT * FROM episodes WHERE id = ?", (episode_id,)).fetchone()
            if row is None:
                return None
            return self._row_to_node(row)

    def update_progress(
        self,
        episode_id: str,
        position_sec: float,
        duration_sec: Optional[float] = None,
        watched: Optional[bool] = None,
    ) -> Optional[EpisodeNode]:
        with self._lock, self._connect() as conn:
            row = conn.execute("SELECT * FROM episodes WHERE id = ?", (episode_id,)).fetchone()
            if row is None:
                return None

            pos = max(0.0, float(position_sec))
            dur = (
                max(0.0, float(duration_sec))
                if duration_sec is not None and duration_sec > 0
                else float(row["duration_sec"])
            )
            if dur > 0 and pos > dur:
                pos = dur

            if watched is not None:
                is_watched = bool(watched)
            else:
                is_watched = bool(row["watched"]) or (dur > 0 and (pos / dur) >= WATCHED_THRESHOLD)

            conn.execute(
                """
                UPDATE episodes
                SET position_sec = ?, duration_sec = ?, watched = ?
                WHERE id = ?
                """,
                (pos, dur, 1 if is_watched else 0, episode_id),
            )
            conn.execute(
                """
                UPDATE shows
                SET current_episode_id = ?, last_watched_at = ?
                WHERE id = ?
                """,
                (episode_id, _utc_now(), row["show_id"]),
            )
            updated = conn.execute("SELECT * FROM episodes WHERE id = ?", (episode_id,)).fetchone()
            return self._row_to_node(updated)
