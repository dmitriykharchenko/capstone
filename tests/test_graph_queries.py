from __future__ import annotations

import pytest

from src.graph_store import CozoGraphStore, GraphNeighborhood, stable_node_id


def test_typed_queries_are_deterministic_and_persist_across_reopen(tmp_path):
    path = tmp_path / "graph.db"
    ada_id = stable_node_id("author", "ada")
    bob_id = stable_node_id("author", "Bob")
    graphs_id = stable_node_id("topic", "Graphs")
    with CozoGraphStore(path) as store:
        store.replace_paper("p2", "Second", ["ada", "Bob"], ["Logic", "Graphs"])
        store.replace_paper("p1", "First", ["ada", "carol"], ["Graphs"])

    with CozoGraphStore(path) as store:
        paper = store.get_paper("p2")
        assert paper is not None
        assert paper.paper_id == "p2"
        assert [author.name for author in paper.authors] == ["ada", "Bob"]
        assert [topic.name for topic in paper.topics] == ["Graphs", "Logic"]
        assert [item.paper_id for item in store.papers_for_author(ada_id)] == [
            "p1",
            "p2",
        ]
        assert [item.paper_id for item in store.papers_for_topic(graphs_id)] == [
            "p1",
            "p2",
        ]
        assert [item.name for item in store.topics_for_author(ada_id)] == [
            "Graphs",
            "Logic",
        ]
        assert [item.author_id for item in store.coauthors_for_author(ada_id)] == [
            bob_id,
            stable_node_id("author", "carol"),
        ]


def test_missing_nodes_return_empty_typed_results(tmp_path):
    with CozoGraphStore(tmp_path / "graph.db") as store:
        assert store.get_paper("missing") is None
        assert store.papers_for_author("missing") == ()
        assert store.papers_for_topic("missing") == ()
        assert store.topics_for_author("missing") == ()
        assert store.coauthors_for_author("missing") == ()
        assert store.neighborhood("missing") == GraphNeighborhood((), ())


def test_neighborhood_is_cycle_safe_deterministic_and_depth_bounded(tmp_path):
    ada_id = stable_node_id("author", "Ada")
    graphs_id = stable_node_id("topic", "Graphs")
    with CozoGraphStore(tmp_path / "graph.db") as store:
        store.replace_paper("p2", "Second", ["Ada", "Bob"], ["Graphs"])
        store.replace_paper("p1", "First", ["Ada", "Bob"], ["Graphs"])

        first = store.neighborhood(ada_id, max_depth=5)
        second = store.neighborhood(ada_id, max_depth=5)
        assert first == second
        assert {node.node_id for node in first.nodes} == {
            "p1",
            "p2",
            ada_id,
            stable_node_id("author", "Bob"),
            graphs_id,
        }
        assert len(first.edges) == 6
        assert [
            node.node_id for node in store.neighborhood(ada_id, max_depth=0).nodes
        ] == [ada_id]


@pytest.mark.parametrize(
    "method", ["get_paper", "papers_for_author", "papers_for_topic"]
)
def test_direct_queries_reject_invalid_identifiers(tmp_path, method):
    with CozoGraphStore(tmp_path / "graph.db") as store:
        with pytest.raises(ValueError, match="must not be empty"):
            getattr(store, method)("  ")
        with pytest.raises(TypeError, match="non-empty string"):
            getattr(store, method)(None)


@pytest.mark.parametrize("depth", [-1, 6, True, 1.5, "1"])
def test_neighborhood_rejects_invalid_bounds(tmp_path, depth):
    with CozoGraphStore(tmp_path / "graph.db") as store:
        with pytest.raises((TypeError, ValueError), match="max_depth"):
            store.neighborhood("missing", max_depth=depth)
