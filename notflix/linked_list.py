"""Doubly linked list data structure for ordered TV show episodes."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Iterator, Optional


@dataclass
class EpisodeNode:
    """A single episode node inside a TV show's doubly linked list."""

    id: str
    show_id: str
    file_path: str
    relative_path: str
    filename: str
    title: str
    season: Optional[int] = None
    episode_number: Optional[int] = None
    disc: Optional[int] = None
    is_extra: bool = False
    confidence: float = 0.0
    sort_reason: str = ""
    position_sec: float = 0.0
    duration_sec: float = 0.0
    watched: bool = False
    file_size: int = 0
    intro_start_sec: Optional[float] = None
    intro_end_sec: Optional[float] = None
    intro_confidence: float = 0.0
    intro_source: str = ""
    prev: Optional["EpisodeNode"] = field(default=None, repr=False, compare=False)
    next: Optional["EpisodeNode"] = field(default=None, repr=False, compare=False)

    @property
    def prev_id(self) -> Optional[str]:
        return self.prev.id if self.prev is not None else None

    @property
    def next_id(self) -> Optional[str]:
        return self.next.id if self.next is not None else None

    @property
    def progress_percent(self) -> float:
        if self.watched:
            return 100.0
        if self.duration_sec <= 0:
            return 0.0
        pct = (self.position_sec / self.duration_sec) * 100.0
        return round(max(0.0, min(100.0, pct)), 1)

    @property
    def display_code(self) -> str:
        if self.is_extra:
            return "EXTRA"
        if self.season is not None and self.episode_number is not None:
            return f"S{self.season:02d}E{self.episode_number:02d}"
        if self.disc is not None and self.episode_number is not None:
            return f"D{self.disc:02d}·T{self.episode_number:02d}"
        if self.episode_number is not None:
            return f"EP {self.episode_number:02d}"
        if self.season is not None:
            return f"S{self.season:02d}"
        if self.disc is not None:
            return f"DISC {self.disc:02d}"
        return "EP"

    def to_dict(self, index: Optional[int] = None) -> dict:
        data = {
            "id": self.id,
            "show_id": self.show_id,
            "file_path": self.file_path,
            "relative_path": self.relative_path,
            "filename": self.filename,
            "title": self.title,
            "season": self.season,
            "episode_number": self.episode_number,
            "disc": self.disc,
            "is_extra": self.is_extra,
            "confidence": round(self.confidence, 2),
            "sort_reason": self.sort_reason,
            "position_sec": round(self.position_sec, 2),
            "duration_sec": round(self.duration_sec, 2),
            "watched": self.watched,
            "progress_percent": self.progress_percent,
            "file_size": self.file_size,
            "intro_start_sec": round(self.intro_start_sec, 2)
            if self.intro_start_sec is not None
            else None,
            "intro_end_sec": round(self.intro_end_sec, 2)
            if self.intro_end_sec is not None
            else None,
            "intro_confidence": round(self.intro_confidence, 2),
            "intro_source": self.intro_source,
            "display_code": self.display_code,
            "prev_id": self.prev_id,
            "next_id": self.next_id,
        }
        if index is not None:
            data["index"] = index
        return data


class EpisodeLinkedList:
    """Doubly linked list storing a show's ordered episodes and active pointer.

    Traversal to next/previous episodes is O(1) via ``node.next`` / ``node.prev``.
    An internal ID index enables O(1) jumping when a user selects any episode
    from the episode drawer.
    """

    def __init__(self) -> None:
        self.head: Optional[EpisodeNode] = None
        self.tail: Optional[EpisodeNode] = None
        self.current: Optional[EpisodeNode] = None
        self._index: dict[str, EpisodeNode] = {}
        self._length: int = 0

    def __len__(self) -> int:
        return self._length

    def __bool__(self) -> bool:
        return self._length > 0

    def __iter__(self) -> Iterator[EpisodeNode]:
        node = self.head
        visited = 0
        while node is not None and visited <= self._length:
            yield node
            node = node.next
            visited += 1

    def reversed_iter(self) -> Iterator[EpisodeNode]:
        node = self.tail
        visited = 0
        while node is not None and visited <= self._length:
            yield node
            node = node.prev
            visited += 1

    def append(self, node: EpisodeNode) -> EpisodeNode:
        """Append an EpisodeNode to the tail of the doubly linked list."""
        if node.id in self._index:
            raise ValueError(f"Episode node {node.id!r} already exists in linked list")

        node.prev = self.tail
        node.next = None

        if self.tail is not None:
            self.tail.next = node
        else:
            self.head = node

        self.tail = node
        self._index[node.id] = node
        self._length += 1

        if self.current is None:
            self.current = self.head

        return node

    def get(self, episode_id: str) -> Optional[EpisodeNode]:
        return self._index.get(episode_id)

    def index_of(self, episode_id: str) -> int:
        for idx, node in enumerate(self):
            if node.id == episode_id:
                return idx
        return -1

    def set_current(self, episode_id: str) -> Optional[EpisodeNode]:
        node = self._index.get(episode_id)
        if node is not None:
            self.current = node
        return node

    def peek_next(self) -> Optional[EpisodeNode]:
        if self.current is None:
            return self.head
        return self.current.next

    def peek_prev(self) -> Optional[EpisodeNode]:
        if self.current is None:
            return None
        return self.current.prev

    def advance_next(self) -> Optional[EpisodeNode]:
        """Move the active pointer forward by one node (O(1) pointer step)."""
        if self.current is None:
            self.current = self.head
            return self.current
        if self.current.next is None:
            return None
        self.current = self.current.next
        return self.current

    def step_prev(self) -> Optional[EpisodeNode]:
        """Move the active pointer backward by one node (O(1) pointer step)."""
        if self.current is None or self.current.prev is None:
            return None
        self.current = self.current.prev
        return self.current

    def _unlink(self, node: EpisodeNode) -> None:
        """Detach ``node`` from its neighbors and clear its prev/next links."""
        if node.prev is not None:
            node.prev.next = node.next
        else:
            self.head = node.next

        if node.next is not None:
            node.next.prev = node.prev
        else:
            self.tail = node.prev

        node.prev = None
        node.next = None

    def remove(self, episode_id: str) -> Optional[EpisodeNode]:
        """Unlink and remove a node by ID while keeping head/tail/current valid."""
        node = self._index.pop(episode_id, None)
        if node is None:
            return None

        if self.current is node:
            self.current = node.next if node.next is not None else node.prev

        self._unlink(node)
        self._length -= 1
        return node

    def move_node(self, episode_id: str, target_index: int) -> bool:
        """Relocate an existing node to ``target_index`` by re-linking pointers."""
        node = self._index.get(episode_id)
        if node is None or self._length <= 1:
            return False

        clamped = max(0, min(self._length - 1, target_index))
        current_idx = self.index_of(episode_id)
        if current_idx == clamped:
            return True

        self._unlink(node)

        # Splice node into the clamped target index
        if clamped == 0:
            node.next = self.head
            if self.head is not None:
                self.head.prev = node
            else:
                self.tail = node
            self.head = node
        elif clamped >= self._length - 1:
            node.prev = self.tail
            if self.tail is not None:
                self.tail.next = node
            else:
                self.head = node
            self.tail = node
        else:
            cursor = self.head
            for _ in range(clamped):
                assert cursor is not None
                cursor = cursor.next
            assert cursor is not None
            before = cursor.prev
            node.prev = before
            node.next = cursor
            if before is not None:
                before.next = node
            cursor.prev = node

        return True

    def to_list(self) -> list[EpisodeNode]:
        return list(self)

    def to_dict(self) -> dict:
        return {
            "head_id": self.head.id if self.head else None,
            "tail_id": self.tail.id if self.tail else None,
            "current_id": self.current.id if self.current else None,
            "length": self._length,
            "episodes": [node.to_dict(index=i) for i, node in enumerate(self)],
        }

    def validate_invariants(self) -> bool:
        """Verify structural integrity of the doubly linked list."""
        if self._length == 0:
            return (
                self.head is None
                and self.tail is None
                and self.current is None
                and len(self._index) == 0
            )

        if self.head is None or self.tail is None:
            return False
        if self.head.prev is not None or self.tail.next is not None:
            return False

        seen: set[str] = set()
        node = self.head
        prev_node: Optional[EpisodeNode] = None
        while node is not None:
            if node.id in seen:
                return False
            if node.prev is not prev_node:
                return False
            if self._index.get(node.id) is not node:
                return False
            seen.add(node.id)
            prev_node = node
            node = node.next

        if prev_node is not self.tail:
            return False
        if len(seen) != self._length or len(self._index) != self._length:
            return False
        if self.current is not None and self.current.id not in seen:
            return False
        return True

    @classmethod
    def from_ordered_nodes(
        cls,
        nodes: list[EpisodeNode],
        current_id: Optional[str] = None,
    ) -> "EpisodeLinkedList":
        ll = cls()
        for node in nodes:
            node.prev = None
            node.next = None
            ll.append(node)
        if current_id is not None:
            ll.set_current(current_id)
        return ll

    @classmethod
    def from_persisted(
        cls,
        nodes: list[EpisodeNode],
        next_map: dict[str, Optional[str]],
        head_id: Optional[str],
        current_id: Optional[str],
    ) -> "EpisodeLinkedList":
        """Rehydrate the doubly linked list by walking persisted head -> next pointers."""
        by_id = {n.id: n for n in nodes}
        ordered: list[EpisodeNode] = []
        visited: set[str] = set()

        cursor_id = head_id
        while cursor_id is not None and cursor_id in by_id and cursor_id not in visited:
            visited.add(cursor_id)
            ordered.append(by_id[cursor_id])
            cursor_id = next_map.get(cursor_id)

        # Recover any orphaned nodes deterministically
        for node in nodes:
            if node.id not in visited:
                visited.add(node.id)
                ordered.append(node)

        return cls.from_ordered_nodes(ordered, current_id=current_id)
