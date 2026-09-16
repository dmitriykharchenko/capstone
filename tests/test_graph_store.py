from __future__ import annotations

import subprocess
import sys
from pathlib import Path

import pytest

from src.graph_store import (
    CozoGraphStore,
    GraphNeighborhood,
    GraphStoreClosedError,
    GraphStoreConfigurationError,
    GraphStorePersistenceError,
    normalize_node_name,
    stable_node_id,
)


def test_normalization_and_ids_are_deterministic():
    assert (
        normalize_node_name("  Graph\u3000Neural  Networks ") == "graph neural networks"
    )
    assert stable_node_id("topic", "Graph Neural Networks") == stable_node_id(
        "topic", " graph  neural networks "
    )
    assert stable_node_id("author", "Ada") != stable_node_id("topic", "Ada")


def test_schema_reopens_and_persists_graph(tmp_path):
    path = tmp_path / "graph.db"
    with CozoGraphStore(path) as store:
        expected = store.replace_paper(
            "paper-1", "A Graph Paper", ["Ada Lovelace"], ["Graph Databases"]
        )

    with CozoGraphStore(path) as reopened:
        assert reopened.get_paper("paper-1") == expected


def test_replacement_is_idempotent_and_cleans_stale_orphans(tmp_path):
    with CozoGraphStore(tmp_path / "graph.db") as store:
        first = store.replace_paper(
            "paper-1",
            "First",
            ["Ada Lovelace", " Grace  Hopper ", "ada lovelace"],
            ["Graphs", "Databases", " graphs "],
        )
        repeated = store.replace_paper(
            "paper-1",
            "First",
            ["Ada Lovelace", "Grace Hopper"],
            ["Graphs", "Databases"],
        )
        replaced = store.replace_paper(
            "paper-1", "Updated", ["Ada Lovelace"], ["Knowledge Graphs"]
        )

        assert repeated == first
        assert replaced.title == "Updated"
        assert [author.name for author in replaced.authors] == ["Ada Lovelace"]
        assert [topic.name for topic in replaced.topics] == ["Knowledge Graphs"]
        stale_author_id = stable_node_id("author", "Grace Hopper")
        stale_topic_id = stable_node_id("topic", "Databases")
        assert store.neighborhood(stale_author_id) == GraphNeighborhood((), ())
        assert store.neighborhood(stale_topic_id) == GraphNeighborhood((), ())


def test_shared_nodes_survive_replacement_and_remove(tmp_path):
    ada_id = stable_node_id("author", "Ada")
    graph_id = stable_node_id("topic", "Graphs")
    with CozoGraphStore(tmp_path / "graph.db") as store:
        store.replace_paper("p1", "One", ["Ada"], ["Graphs"])
        store.replace_paper("p2", "Two", ["Ada", "Grace"], ["Graphs", "Compilers"])

        store.replace_paper("p1", "One revised", ["Alan"], ["Logic"])
        assert [paper.paper_id for paper in store.papers_for_author(ada_id)] == ["p2"]
        assert [paper.paper_id for paper in store.papers_for_topic(graph_id)] == ["p2"]
        assert store.remove_paper("p1") is True
        assert store.remove_paper("p1") is False
        assert store.get_paper("p2") is not None
        assert store.neighborhood(ada_id).nodes


def test_direct_derived_and_bounded_traversal_queries(tmp_path):
    ada_id = stable_node_id("author", "Ada")
    grace_id = stable_node_id("author", "Grace")
    graph_id = stable_node_id("topic", "Graphs")
    logic_id = stable_node_id("topic", "Logic")
    with CozoGraphStore(tmp_path / "graph.db") as store:
        store.replace_paper("p1", "One", ["Ada", "Grace"], ["Graphs"])
        store.replace_paper("p2", "Two", ["Ada", "Alan"], ["Logic"])

        assert [author.author_id for author in store.coauthors_for_author(ada_id)] == [
            stable_node_id("author", "Alan"),
            grace_id,
        ]
        assert {topic.topic_id for topic in store.topics_for_author(ada_id)} == {
            graph_id,
            logic_id,
        }
        depth_zero = store.neighborhood(ada_id, max_depth=0)
        assert [node.node_id for node in depth_zero.nodes] == [ada_id]
        depth_one = store.neighborhood(ada_id, max_depth=1)
        assert {node.node_id for node in depth_one.nodes} == {ada_id, "p1", "p2"}
        depth_two = store.neighborhood(ada_id, max_depth=2)
        assert {graph_id, logic_id, grace_id}.issubset(
            {node.node_id for node in depth_two.nodes}
        )
        assert {edge.kind for edge in depth_two.edges} == {"authored_by", "has_topic"}


def test_failed_replacement_rolls_back_existing_graph(tmp_path, monkeypatch):
    path = tmp_path / "graph.db"
    with CozoGraphStore(path) as store:
        original = store.replace_paper("p1", "Original", ["Ada"], ["Graphs"])
        real_client = store._client
        real_multi_transact = real_client.multi_transact

        class FailingTransaction:
            def __init__(self):
                self.tx = real_multi_transact(True)
                self.calls = 0

            def run(self, *args, **kwargs):
                self.calls += 1
                if self.calls == 4:
                    raise RuntimeError("injected transaction failure")
                return self.tx.run(*args, **kwargs)

            def commit(self):
                return self.tx.commit()

            def abort(self):
                return self.tx.abort()

        monkeypatch.setattr(
            real_client, "multi_transact", lambda _write: FailingTransaction()
        )
        with pytest.raises(GraphStorePersistenceError, match="atomically replace"):
            store.replace_paper("p1", "Changed", ["Grace"], ["Compilers"])
        assert store.get_paper("p1") == original


def test_invalid_replacement_retains_existing_graph(tmp_path):
    with CozoGraphStore(tmp_path / "graph.db") as store:
        original = store.replace_paper("p1", "Original", ["Ada"], ["Graphs"])
        with pytest.raises(ValueError, match="authors"):
            store.replace_paper("p1", "Changed", [], ["Compilers"])
        assert store.get_paper("p1") == original


def test_incompatible_existing_schema_has_clear_error(tmp_path):
    pycozo = pytest.importorskip("pycozo")
    path = tmp_path / "graph.db"
    client = pycozo.Client("sqlite", str(path), dataframe=False)
    client.run(":create paper {wrong_id: String}")
    client.close()

    with pytest.raises(GraphStoreConfigurationError, match="incompatible.*paper"):
        CozoGraphStore(path)


def test_client_load_failure_is_a_configuration_error(tmp_path):
    def unavailable(*_args, **_kwargs):
        raise OSError("native library missing")

    with pytest.raises(GraphStoreConfigurationError, match=r"pycozo\[embedded\]"):
        CozoGraphStore(tmp_path / "graph.db", client_factory=unavailable)


def test_close_is_idempotent_and_rejects_later_operations(tmp_path):
    store = CozoGraphStore(tmp_path / "graph.db")
    store.close()
    store.close()
    with pytest.raises(GraphStoreClosedError, match="closed"):
        store.get_paper("p1")


def test_database_open_is_serialized_across_processes(tmp_path):
    path = tmp_path / "graph.db"
    script = (
        "from src.graph_store import CozoGraphStore, GraphStoreLockedError; "
        "import sys; "
        "path=sys.argv[1]; "
        "store=CozoGraphStore(path, lock_timeout=0.1); "
        "store.close()"
    )
    with CozoGraphStore(path):
        blocked = subprocess.run(
            [sys.executable, "-c", script, str(path)],
            cwd=Path(__file__).parents[1],
            capture_output=True,
            text=True,
            check=False,
        )
    assert blocked.returncode != 0
    assert "GraphStoreLockedError" in blocked.stderr

    released = subprocess.run(
        [sys.executable, "-c", script, str(path)],
        cwd=Path(__file__).parents[1],
        capture_output=True,
        text=True,
        check=False,
    )
    assert released.returncode == 0, released.stderr


@pytest.mark.parametrize("depth", [-1, 6, True, 1.5])
def test_neighborhood_depth_is_bounded(tmp_path, depth):
    with CozoGraphStore(tmp_path / "graph.db") as store:
        store.replace_paper("p1", "One", ["Ada"], ["Graphs"])
        with pytest.raises((TypeError, ValueError), match="max_depth"):
            store.neighborhood("p1", max_depth=depth)
