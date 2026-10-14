from __future__ import annotations

import os
import subprocess
import sys

import pytest

from src.indexing.fact_extraction import Fact
from src.storage.graph_store import (
    ChunkNode,
    CozoGraphStore,
    GraphStoreConfigurationError,
    GraphStoreLockedError,
    GraphStorePersistenceError,
    stop_lock_owner,
)


def test_fact_graph_deduplicates_across_chunks_and_papers_and_cleans_orphans(tmp_path):
    shared = Fact.create("method", "Monte Carlo", "The study uses Monte Carlo.")
    other = Fact.create("metric", "Accuracy", "Accuracy is reported.", "%")
    with CozoGraphStore(tmp_path / "graph.db") as store:
        store.replace_paper(
            "p1",
            "One",
            ["Ada"],
            ["Methods"],
            chunks=(ChunkNode("p1:c0", "p1", 0, 1), ChunkNode("p1:c1", "p1", 1, 2)),
            facts_by_chunk={"p1:c0": (shared,), "p1:c1": (shared, other)},
            fact_version=1,
        )
        store.replace_paper(
            "p2",
            "Two",
            ["Grace"],
            ["Methods"],
            chunks=(ChunkNode("p2:c0", "p2", 0, 3),),
            facts_by_chunk={"p2:c0": (shared,)},
            fact_version=1,
        )

        assert store.fact_version_for_paper("p1") == 1
        assert store.facts_for_chunks(("p1:c0", "p1:c1")) == {
            "p1:c0": (shared,),
            "p1:c1": (shared, other),
        }
        assert store.facts_for_chunks(("missing",)) == {}
        assert store.chunks_for_facts((shared.fact_id,))[shared.fact_id] == (
            "p1:c0",
            "p1:c1",
            "p2:c0",
        )

        store.remove_paper("p1")
        assert store.chunks_for_facts((shared.fact_id,))[shared.fact_id] == ("p2:c0",)
        assert store.chunks_for_facts((other.fact_id,))[other.fact_id] == ()
        assert store._client.run("?[fact_id] := *fact{fact_id}")["rows"] == [
            [shared.fact_id]
        ]


def test_fact_only_backfill_replaces_relations_without_changing_paper(tmp_path):
    fact = Fact.create("dataset", "HumanEval", "The evaluation uses HumanEval.")
    with CozoGraphStore(tmp_path / "graph.db") as store:
        original = store.replace_paper("p1", "One", ["Ada"], ["Evaluation"])

        store.replace_paper_facts(
            "p1",
            (ChunkNode("p1:c0", "p1", 0, 4),),
            {"p1:c0": (fact,)},
            1,
        )

        assert store.get_paper("p1") == original
        assert store.fact_version_for_paper("p1") == 1
        assert store.facts_for_chunks(("p1:c0",))["p1:c0"] == (fact,)


def test_paper_replacement_can_clear_facts_without_success_marker(tmp_path):
    shared = Fact.create("method", "Shared", "Both papers use this method.")
    orphan = Fact.create("claim", "Old", "Only the first paper has this claim.")
    with CozoGraphStore(tmp_path / "graph.db") as store:
        store.replace_paper(
            "p1",
            "One",
            ["Ada"],
            ["Methods"],
            chunks=(ChunkNode("p1:c0", "p1", 0, 1),),
            facts_by_chunk={"p1:c0": (shared, orphan)},
            fact_version=1,
        )
        store.replace_paper(
            "p2",
            "Two",
            ["Grace"],
            ["Methods"],
            chunks=(ChunkNode("p2:c0", "p2", 0, 1),),
            facts_by_chunk={"p2:c0": (shared,)},
            fact_version=1,
        )

        store.replace_paper("p1", "Updated", ["Ada"], ["Methods"], clear_facts=True)

        assert store.fact_version_for_paper("p1") is None
        assert store.fact_ids_for_paper("p1") == ()
        assert store.chunks_for_facts((shared.fact_id,))[shared.fact_id] == ("p2:c0",)
        assert store.chunks_for_facts((orphan.fact_id,))[orphan.fact_id] == ()


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


@pytest.mark.skipif(os.name == "nt", reason="owner PID is recorded on POSIX only")
def test_stop_lock_owner_takes_over_a_backend_lock_but_spares_other_processes(tmp_path):
    database = tmp_path / "graph.db"
    holder = (
        "import sys, time\n"
        "from src.storage.graph_store import CozoGraphStore\n"
        "store = CozoGraphStore(sys.argv[1])\n"
        "print('locked', flush=True)\n"
        "time.sleep(60)\n"
    )
    backend = subprocess.Popen(
        [sys.executable, "-c", holder, str(database), "src.cli"],
        stdout=subprocess.PIPE,
        text=True,
    )
    unrelated = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
    lock_file = tmp_path / "graph.db.lock"
    try:
        assert backend.stdout.readline().strip() == "locked"
        assert lock_file.read_text() == str(backend.pid)
        with pytest.raises(GraphStoreLockedError):
            CozoGraphStore(database, lock_timeout=0)

        assert stop_lock_owner(database) is True
        assert backend.wait(timeout=5) != 0
        with CozoGraphStore(database, lock_timeout=0):
            assert lock_file.read_text() == str(os.getpid())

        lock_file.write_text(str(unrelated.pid))
        assert stop_lock_owner(database) is False
        assert unrelated.poll() is None
    finally:
        for process in (backend, unrelated):
            process.kill()
            process.wait()
