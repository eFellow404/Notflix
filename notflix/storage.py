"""SQLite persistence for TV shows, persisted doubly linked lists, and playback state."""

from __future__ import annotations

import os
import sqlite3
import threading
import uuid
from collections import defaultdict
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

from notflix.intelligence import AnalyzedEpisode, scan_and_order_folder
from notflix.intro_detector import IntroSpan
from notflix.linked_list import EpisodeLinkedList, EpisodeNode

WATCHED_THRESHOLD = 0.90

_SCHEMA_SQL = """
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
    last_watched_at TEXT,
    intro_scanned_at TEXT
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
    intro_start_sec REAL,
    intro_end_sec REAL,
    intro_confidence REAL NOT NULL DEFAULT 0.0,
    intro_source TEXT NOT NULL DEFAULT '',
    UNIQUE(show_id, relative_path)
);

CREATE INDEX IF NOT EXISTS idx_episodes_show_id ON episodes(show_id);
"""

_MIGRATIONS: tuple[tuple[str, str, str], ...] = (
    ("shows", "intro_scanned_at", "TEXT"),
    ("episodes", "intro_start_sec", "REAL"),
    ("episodes", "intro_end_sec", "REAL"),
    ("episodes", "intro_confidence", "REAL NOT NULL DEFAULT 0.0"),
    ("episodes", "intro_source", "TEXT NOT NULL DEFAULT ''"),
)

_EPISODE_INSERT_COLUMNS = (
    "id", "show_id", "file_path", "relative_path", "filename", "title",
    "season", "episode_number", "disc", "is_extra", "confidence", "sort_reason",
    "prev_episode_id", "next_episode_id", "position_sec", "duration_sec",
    "watched", "file_size", "intro_start_sec", "intro_end_sec",
    "intro_confidence", "intro_source",
)

_EPISODE_INSERT_SQL = (
    f"INSERT INTO episodes ({', '.join(_EPISODE_INSERT_COLUMNS)}) "
    f"VALUES ({', '.join('?' for _ in _EPISODE_INSERT_COLUMNS)})"
)

_SHOW_FIELDS = (
    "id", "name", "folder_path", "head_episode_id", "tail_episode_id",
    "current_episode_id", "created_at", "last_scanned_at", "last_watched_at",
    "intro_scanned_at",
)


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
        return conn

    def _init_schema(self) -> None:
        with self._lock, self._connect() as conn:
            conn.execute("PRAGMA journal_mode = WAL")
            conn.executescript(_SCHEMA_SQL)
            self._migrate_schema(conn)

    @staticmethod
    def _migrate_schema(conn: sqlite3.Connection) -> None:
        """Safely add new columns to existing SQLite databases."""
        table_cols: dict[str, set[str]] = {}
        for table, column, col_def in _MIGRATIONS:
            if table not in table_cols:
                table_cols[table] = {
                    row["name"] for row in conn.execute(f"PRAGMA table_info({table})").fetchall()
                }
            if column not in table_cols[table]:
                conn.execute(f"ALTER TABLE {table} ADD COLUMN {column} {col_def}")
                table_cols[table].add(column)

    @staticmethod
    def _fetch_show_row(conn: sqlite3.Connection, show_id: str) -> Optional[sqlite3.Row]:
        return conn.execute("SELECT * FROM shows WHERE id = ?", (show_id,)).fetchone()

    @staticmethod
    def _row_to_node(row: sqlite3.Row) -> EpisodeNode:
        intro_start = row["intro_start_sec"]
        intro_end = row["intro_end_sec"]
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
            intro_start_sec=float(intro_start) if intro_start is not None else None,
            intro_end_sec=float(intro_end) if intro_end is not None else None,
            intro_confidence=float(row["intro_confidence"] or 0.0),
            intro_source=row["intro_source"] or "",
        )

    @staticmethod
    def _row_to_show(row: sqlite3.Row, include_folder_exists: bool = False) -> dict:
        show = {field: row[field] for field in _SHOW_FIELDS}
        show["manual_order"] = bool(row["manual_order"])
        if include_folder_exists:
            show["folder_exists"] = Path(row["folder_path"]).is_dir()
        return show

    @staticmethod
    def _node_to_insert_row(show_id: str, node: EpisodeNode) -> tuple:
        return (
            node.id, show_id, node.file_path, node.relative_path, node.filename, node.title,
            node.season, node.episode_number, node.disc, int(node.is_extra), node.confidence,
            node.sort_reason, node.prev_id, node.next_id, node.position_sec, node.duration_sec,
            int(node.watched), node.file_size, node.intro_start_sec, node.intro_end_sec,
            node.intro_confidence, node.intro_source,
        )

    @classmethod
    def _build_linked_list(
        cls, show_row: sqlite3.Row, ep_rows: list[sqlite3.Row]
    ) -> EpisodeLinkedList:
        nodes = [cls._row_to_node(r) for r in ep_rows]
        next_map = {r["id"]: r["next_episode_id"] for r in ep_rows}
        return EpisodeLinkedList.from_persisted(
            nodes=nodes,
            next_map=next_map,
            head_id=show_row["head_episode_id"],
            current_id=show_row["current_episode_id"],
        )

    def _load_linked_list(self, conn: sqlite3.Connection, show_row: sqlite3.Row) -> EpisodeLinkedList:
        ep_rows = conn.execute(
            "SELECT * FROM episodes WHERE show_id = ?",
            (show_row["id"],),
        ).fetchall()
        return self._build_linked_list(show_row, ep_rows)

    def _persist_linked_list(
        self,
        conn: sqlite3.Connection,
        show_id: str,
        ll: EpisodeLinkedList,
        manual_order: Optional[bool] = None,
        scanned_at: Optional[str] = None,
    ) -> None:
        conn.execute("DELETE FROM episodes WHERE show_id = ?", (show_id,))
        rows = [self._node_to_insert_row(show_id, node) for node in ll]
        if rows:
            conn.executemany(_EPISODE_INSERT_SQL, rows)

        updates = ["head_episode_id = ?", "tail_episode_id = ?", "current_episode_id = ?"]
        params: list = [
            ll.head.id if ll.head else None,
            ll.tail.id if ll.tail else None,
            ll.current.id if ll.current else None,
        ]
        if manual_order is not None:
            updates.append("manual_order = ?")
            params.append(int(manual_order))
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
            intro_start_sec=existing.intro_start_sec if existing else None,
            intro_end_sec=existing.intro_end_sec if existing else None,
            intro_confidence=existing.intro_confidence if existing else 0.0,
            intro_source=existing.intro_source if existing else "",
        )

    def add_show(
        self, folder_path: str, name: Optional[str] = None
    ) -> tuple[dict, EpisodeLinkedList]:
        resolved = Path(folder_path).expanduser().resolve()
        if not resolved.is_dir():
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

            show_row = self._fetch_show_row(conn, show_id)
            assert show_row is not None
            return self._row_to_show(show_row), ll

    def list_shows(self) -> list[dict]:
        with self._lock, self._connect() as conn:
            show_rows = conn.execute(
                """
                SELECT * FROM shows
                ORDER BY
                    CASE WHEN last_watched_at IS NULL THEN 1 ELSE 0 END,
                    last_watched_at DESC,
                    created_at DESC
                """
            ).fetchall()

            if not show_rows:
                return []

            episodes_by_show: dict[str, list[sqlite3.Row]] = defaultdict(list)
            for ep_row in conn.execute("SELECT * FROM episodes").fetchall():
                episodes_by_show[ep_row["show_id"]].append(ep_row)

            results: list[dict] = []
            for row in show_rows:
                ll = self._build_linked_list(row, episodes_by_show.get(row["id"], []))
                show = self._row_to_show(row, include_folder_exists=True)
                episodes = ll.to_list()
                main_episodes = [ep for ep in episodes if not ep.is_extra]
                current_idx = ll.index_of(ll.current.id) if ll.current else -1
                next_ep = ll.peek_next()
                show.update(
                    {
                        "episode_count": len(ll),
                        "main_episode_count": len(main_episodes),
                        "watched_count": sum(1 for ep in episodes if ep.watched),
                        "season_count": len({ep.season for ep in main_episodes if ep.season is not None}),
                        "disc_count": len({ep.disc for ep in main_episodes if ep.disc is not None}),
                        "extra_count": len(episodes) - len(main_episodes),
                        "intros_detected_count": sum(
                            1
                            for ep in episodes
                            if ep.intro_start_sec is not None and ep.intro_end_sec is not None
                        ),
                        "current_episode": ll.current.to_dict(index=current_idx) if ll.current else None,
                        "next_episode": next_ep.to_dict(index=current_idx + 1) if next_ep else None,
                    }
                )
                results.append(show)
            return results

    def get_show(self, show_id: str) -> Optional[tuple[dict, EpisodeLinkedList]]:
        with self._lock, self._connect() as conn:
            row = self._fetch_show_row(conn, show_id)
            if row is None:
                return None
            return self._row_to_show(row, include_folder_exists=True), self._load_linked_list(conn, row)

    def delete_show(self, show_id: str) -> bool:
        with self._lock, self._connect() as conn:
            cur = conn.execute("DELETE FROM shows WHERE id = ?", (show_id,))
            return cur.rowcount > 0

    def rename_show(self, show_id: str, name: str) -> Optional[dict]:
        clean = name.strip()
        if not clean:
            raise ValueError("Show name cannot be empty")
        with self._lock, self._connect() as conn:
            cur = conn.execute("UPDATE shows SET name = ? WHERE id = ?", (clean, show_id))
            if cur.rowcount == 0:
                return None
            row = self._fetch_show_row(conn, show_id)
            assert row is not None
            return self._row_to_show(row, include_folder_exists=True)

    def rescan_show(
        self, show_id: str, reset_order: bool = False
    ) -> Optional[tuple[dict, EpisodeLinkedList, dict]]:
        """Re-check a show folder for new/removed files and update its linked list.

        Preserves saved playback timestamps, watched status, and current episode pointer.
        """
        with self._lock, self._connect() as conn:
            row = self._fetch_show_row(conn, show_id)
            if row is None:
                return None

            existing_ll = self._load_linked_list(conn, row)
            old_by_rel = {node.relative_path: node for node in existing_ll}
            old_current_rel = existing_ll.current.relative_path if existing_ll.current else None

            analyzed = scan_and_order_folder(row["folder_path"])
            new_by_rel = {item.relative_path: item for item in analyzed}

            added = sum(1 for r in new_by_rel if r not in old_by_rel)
            removed = sum(1 for r in old_by_rel if r not in new_by_rel)
            unchanged = len(new_by_rel) - added

            keep_manual = bool(row["manual_order"]) and not reset_order
            if keep_manual:
                # Preserve manual linked-list order for surviving files; append newly discovered files
                ordered_nodes = [
                    self._build_node(show_id, new_by_rel[old_node.relative_path], existing=old_node)
                    for old_node in existing_ll
                    if old_node.relative_path in new_by_rel
                ]
                ordered_nodes.extend(
                    self._build_node(show_id, item)
                    for item in analyzed
                    if item.relative_path not in old_by_rel
                )
            else:
                ordered_nodes = [
                    self._build_node(show_id, item, existing=old_by_rel.get(item.relative_path))
                    for item in analyzed
                ]

            new_current_id = next(
                (node.id for node in ordered_nodes if node.relative_path == old_current_rel),
                None,
            )
            new_ll = EpisodeLinkedList.from_ordered_nodes(ordered_nodes, current_id=new_current_id)
            self._persist_linked_list(
                conn,
                show_id,
                new_ll,
                manual_order=keep_manual,
                scanned_at=_utc_now(),
            )

            updated_row = self._fetch_show_row(conn, show_id)
            assert updated_row is not None
            diff = {
                "added": added,
                "removed": removed,
                "unchanged": unchanged,
                "total": len(new_ll),
            }
            return self._row_to_show(updated_row), new_ll, diff

    def select_episode(self, show_id: str, episode_id: str) -> Optional[EpisodeNode]:
        with self._lock, self._connect() as conn:
            row = self._fetch_show_row(conn, show_id)
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

    def _step_episode(
        self, show_id: str, forward: bool
    ) -> Optional[tuple[EpisodeNode, EpisodeLinkedList]]:
        with self._lock, self._connect() as conn:
            row = self._fetch_show_row(conn, show_id)
            if row is None:
                return None
            ll = self._load_linked_list(conn, row)
            node = ll.advance_next() if forward else ll.step_prev()
            if node is None:
                return None
            conn.execute(
                "UPDATE shows SET current_episode_id = ?, last_watched_at = ? WHERE id = ?",
                (node.id, _utc_now(), show_id),
            )
            return node, ll

    def advance_next(self, show_id: str) -> Optional[tuple[EpisodeNode, EpisodeLinkedList]]:
        return self._step_episode(show_id, forward=True)

    def step_prev(self, show_id: str) -> Optional[tuple[EpisodeNode, EpisodeLinkedList]]:
        return self._step_episode(show_id, forward=False)

    def reorder_episode(
        self, show_id: str, episode_id: str, target_index: int
    ) -> Optional[EpisodeLinkedList]:
        with self._lock, self._connect() as conn:
            row = self._fetch_show_row(conn, show_id)
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
            return self._row_to_node(row) if row is not None else None

    def get_episode_links(self, episode_id: str) -> tuple[Optional[str], Optional[str]]:
        """Return persisted ``(prev_episode_id, next_episode_id)`` for an episode."""
        with self._lock, self._connect() as conn:
            row = conn.execute(
                "SELECT prev_episode_id, next_episode_id FROM episodes WHERE id = ?",
                (episode_id,),
            ).fetchone()
            if row is None:
                return None, None
            return row["prev_episode_id"], row["next_episode_id"]

    def get_episode_dict(self, episode_id: str) -> Optional[dict]:
        """Fetch an episode as a dict populated with its persisted prev_id/next_id."""
        with self._lock, self._connect() as conn:
            row = conn.execute("SELECT * FROM episodes WHERE id = ?", (episode_id,)).fetchone()
            if row is None:
                return None
            ep_dict = self._row_to_node(row).to_dict()
            ep_dict["prev_id"] = row["prev_episode_id"]
            ep_dict["next_id"] = row["next_episode_id"]
            return ep_dict

    def episode_to_dict_with_links(self, node: EpisodeNode) -> dict:
        """Serialize a standalone EpisodeNode with its persisted prev_id/next_id."""
        ep_dict = node.to_dict()
        ep_dict["prev_id"], ep_dict["next_id"] = self.get_episode_links(node.id)
        return ep_dict

    def update_progress(
        self,
        episode_id: str,
        position_sec: Optional[float] = None,
        duration_sec: Optional[float] = None,
        watched: Optional[bool] = None,
        set_current: Optional[bool] = None,
        reset_position: bool = False,
    ) -> Optional[EpisodeNode]:
        with self._lock, self._connect() as conn:
            row = conn.execute("SELECT * FROM episodes WHERE id = ?", (episode_id,)).fetchone()
            if row is None:
                return None

            if reset_position:
                pos = 0.0
            elif position_sec is not None:
                pos = max(0.0, float(position_sec))
            else:
                pos = float(row["position_sec"])

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
                "UPDATE episodes SET position_sec = ?, duration_sec = ?, watched = ? WHERE id = ?",
                (pos, dur, int(is_watched), episode_id),
            )
            should_set_current = (position_sec is not None) if set_current is None else bool(set_current)
            if should_set_current:
                conn.execute(
                    "UPDATE shows SET current_episode_id = ?, last_watched_at = ? WHERE id = ?",
                    (episode_id, _utc_now(), row["show_id"]),
                )
            updated = conn.execute("SELECT * FROM episodes WHERE id = ?", (episode_id,)).fetchone()
            return self._row_to_node(updated)

    def set_episodes_watched(
        self,
        show_id: str,
        episode_ids: list[str],
        watched: bool,
        reset_position: bool = False,
    ) -> Optional[EpisodeLinkedList]:
        """Bulk mark episodes watched/unwatched without changing active playback pointer."""
        with self._lock, self._connect() as conn:
            row = self._fetch_show_row(conn, show_id)
            if row is None:
                return None
            if episode_ids:
                flag = int(watched)
                sql = (
                    "UPDATE episodes SET watched = ?, position_sec = 0.0 WHERE id = ? AND show_id = ?"
                    if reset_position
                    else "UPDATE episodes SET watched = ? WHERE id = ? AND show_id = ?"
                )
                conn.executemany(sql, [(flag, ep_id, show_id) for ep_id in episode_ids])
            return self._load_linked_list(conn, row)

    def set_episode_intro(
        self,
        episode_id: str,
        intro_start_sec: Optional[float],
        intro_end_sec: Optional[float],
        confidence: float = 1.0,
        source: str = "manual",
    ) -> Optional[EpisodeNode]:
        """Save or clear an episode's intro timestamp range."""
        with self._lock, self._connect() as conn:
            row = conn.execute("SELECT * FROM episodes WHERE id = ?", (episode_id,)).fetchone()
            if row is None:
                return None

            if intro_start_sec is None or intro_end_sec is None:
                conn.execute(
                    """
                    UPDATE episodes
                    SET intro_start_sec = NULL, intro_end_sec = NULL,
                        intro_confidence = 0.0, intro_source = ''
                    WHERE id = ?
                    """,
                    (episode_id,),
                )
            else:
                start = max(0.0, float(intro_start_sec))
                end = max(0.0, float(intro_end_sec))
                if end <= start + 1.0:
                    raise ValueError("intro_end_sec must be greater than intro_start_sec")
                conn.execute(
                    """
                    UPDATE episodes
                    SET intro_start_sec = ?, intro_end_sec = ?,
                        intro_confidence = ?, intro_source = ?
                    WHERE id = ?
                    """,
                    (start, end, max(0.0, min(1.0, float(confidence))), source, episode_id),
                )

            updated = conn.execute("SELECT * FROM episodes WHERE id = ?", (episode_id,)).fetchone()
            return self._row_to_node(updated)

    def save_show_intros(
        self,
        show_id: str,
        intros: dict[str, IntroSpan],
        preserve_manual: bool = True,
        mark_scanned: bool = True,
    ) -> Optional[tuple[dict, EpisodeLinkedList]]:
        """Persist detected intros for episodes in a show."""
        with self._lock, self._connect() as conn:
            row = self._fetch_show_row(conn, show_id)
            if row is None:
                return None

            if intros:
                existing_sources = {
                    ep_row["id"]: (ep_row["intro_source"] or "")
                    for ep_row in conn.execute(
                        "SELECT id, intro_source FROM episodes WHERE show_id = ?",
                        (show_id,),
                    ).fetchall()
                }
                updates = [
                    (
                        max(0.0, float(span.start_sec)),
                        max(0.0, float(span.end_sec)),
                        max(0.0, min(1.0, float(span.confidence))),
                        span.source,
                        ep_id,
                        show_id,
                    )
                    for ep_id, span in intros.items()
                    if ep_id in existing_sources
                    and not (preserve_manual and existing_sources[ep_id] == "manual")
                ]
                if updates:
                    conn.executemany(
                        """
                        UPDATE episodes
                        SET intro_start_sec = ?, intro_end_sec = ?,
                            intro_confidence = ?, intro_source = ?
                        WHERE id = ? AND show_id = ?
                        """,
                        updates,
                    )

            if mark_scanned:
                conn.execute(
                    "UPDATE shows SET intro_scanned_at = ? WHERE id = ?",
                    (_utc_now(), show_id),
                )

            updated_row = self._fetch_show_row(conn, show_id)
            assert updated_row is not None
            return (
                self._row_to_show(updated_row, include_folder_exists=True),
                self._load_linked_list(conn, updated_row),
            )
