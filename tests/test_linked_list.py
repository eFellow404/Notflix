"""Unit tests for the doubly linked list episode container."""

from __future__ import annotations

import unittest

from notflix.linked_list import EpisodeLinkedList, EpisodeNode


def _make_node(ep_id: str, ep_num: int) -> EpisodeNode:
    return EpisodeNode(
        id=ep_id,
        show_id="show-1",
        file_path=f"/tv/ep{ep_num}.mp4",
        relative_path=f"ep{ep_num}.mp4",
        filename=f"ep{ep_num}.mp4",
        title=f"Episode {ep_num}",
        season=1,
        episode_number=ep_num,
    )


class TestEpisodeLinkedList(unittest.TestCase):
    def test_append_and_pointer_traversal(self) -> None:
        ll = EpisodeLinkedList()
        self.assertTrue(ll.validate_invariants())
        self.assertEqual(len(ll), 0)

        n1 = ll.append(_make_node("e1", 1))
        n2 = ll.append(_make_node("e2", 2))
        n3 = ll.append(_make_node("e3", 3))

        self.assertTrue(ll.validate_invariants())
        self.assertEqual(len(ll), 3)
        self.assertIs(ll.head, n1)
        self.assertIs(ll.tail, n3)
        self.assertIs(ll.current, n1)
        self.assertIsNone(n1.prev)
        self.assertIs(n1.next, n2)
        self.assertIs(n2.prev, n1)
        self.assertIs(n2.next, n3)
        self.assertIs(n3.prev, n2)
        self.assertIsNone(n3.next)

        # O(1) next / prev traversal
        self.assertIs(ll.advance_next(), n2)
        self.assertIs(ll.current, n2)
        self.assertIs(ll.advance_next(), n3)
        self.assertIsNone(ll.advance_next())
        self.assertIs(ll.current, n3)

        self.assertIs(ll.step_prev(), n2)
        self.assertIs(ll.step_prev(), n1)
        self.assertIsNone(ll.step_prev())

        self.assertEqual([n.id for n in ll.reversed_iter()], ["e3", "e2", "e1"])

    def test_move_node_relinks_pointers(self) -> None:
        ll = EpisodeLinkedList.from_ordered_nodes(
            [_make_node("e1", 1), _make_node("e2", 2), _make_node("e3", 3), _make_node("e4", 4)]
        )
        self.assertTrue(ll.move_node("e4", 0))
        self.assertTrue(ll.validate_invariants())
        self.assertEqual([n.id for n in ll], ["e4", "e1", "e2", "e3"])

        self.assertTrue(ll.move_node("e4", 2))
        self.assertTrue(ll.validate_invariants())
        self.assertEqual([n.id for n in ll], ["e1", "e2", "e4", "e3"])

        self.assertTrue(ll.move_node("e1", 3))
        self.assertTrue(ll.validate_invariants())
        self.assertEqual([n.id for n in ll], ["e2", "e4", "e3", "e1"])

    def test_remove_updates_head_tail_and_current(self) -> None:
        ll = EpisodeLinkedList.from_ordered_nodes(
            [_make_node("e1", 1), _make_node("e2", 2), _make_node("e3", 3)],
            current_id="e2",
        )
        removed = ll.remove("e2")
        self.assertIsNotNone(removed)
        self.assertTrue(ll.validate_invariants())
        self.assertEqual([n.id for n in ll], ["e1", "e3"])
        self.assertEqual(ll.current.id, "e3")  # type: ignore[union-attr]

        ll.remove("e3")
        self.assertTrue(ll.validate_invariants())
        self.assertEqual(ll.current.id, "e1")  # type: ignore[union-attr]

        ll.remove("e1")
        self.assertTrue(ll.validate_invariants())
        self.assertEqual(len(ll), 0)
        self.assertIsNone(ll.current)

    def test_rehydrate_from_persisted_pointers(self) -> None:
        nodes = [_make_node("e3", 3), _make_node("e1", 1), _make_node("e2", 2)]
        next_map = {"e1": "e2", "e2": "e3", "e3": None}
        ll = EpisodeLinkedList.from_persisted(
            nodes=nodes,
            next_map=next_map,
            head_id="e1",
            current_id="e2",
        )
        self.assertTrue(ll.validate_invariants())
        self.assertEqual([n.id for n in ll], ["e1", "e2", "e3"])
        self.assertEqual(ll.current.id, "e2")  # type: ignore[union-attr]


if __name__ == "__main__":
    unittest.main()
