import io
import json
from types import SimpleNamespace

import pytest

from src.retrieval.embeddings import EmbeddingConfig
from src.indexing.paper_indexer import ExtractedPage, PaperIndexer
from src.indexing.paper_metadata import PaperMetadata


class MemoryCollection:
    def __init__(self):
        self.records = {"paper:old": "old evidence"}
        self.writes = []

    def delete(self, *, where):
        self.writes.append(("delete", where))
        self.records = {
            key: value
            for key, value in self.records.items()
            if not key.startswith(where["paper_id"] + ":")
        }

    def upsert(self, **kwargs):
        self.writes.append(("upsert", kwargs))
        self.records.update(zip(kwargs["ids"], kwargs["documents"], strict=True))


class MemoryGraph:
    def __init__(self):
        self.papers = {"paper": ("Old", ("Old author",), ("Old topic",))}
        self.writes = []

    def replace_paper(self, paper_id, title, authors, topics):
        self.writes.append(("replace", paper_id))
        self.papers[paper_id] = (title, authors, topics)

    def remove_paper(self, paper_id):
        self.writes.append(("remove", paper_id))
        self.papers.pop(paper_id, None)


@pytest.fixture(params=["default", "injected"])
def embedding_options(request):
    def options(embed):
        if request.param == "injected":
            return {"batch_embedder": embed}
        config = EmbeddingConfig(model="offline-batching-test")

        def embed_batch(texts, received_config):
            assert received_config is config
            return embed(texts)

        return {
            "embedding_config": config,
            "embedding_backend": SimpleNamespace(embed_batch=embed_batch),
        }

    return options


def synthetic_pdf(tmp_path, name="paper.pdf"):
    path = tmp_path / name
    path.write_bytes(b"%PDF-1.4\n")
    return path


def metadata(_text):
    return PaperMetadata("Title", ("Ada",), ("graphs", "retrieval", "testing"))


@pytest.mark.parametrize("failure_batch", [1, 2, 3])
@pytest.mark.parametrize(
    "failure", ["provider", "short", "extra", "dimension", "invalid"]
)
def test_batch_failures_leave_old_graph_and_vectors_untouched(
    tmp_path, embedding_options, failure_batch, failure
):
    synthetic_pdf(tmp_path)
    chunks = [f"{index:04d}:" + "x" * 7995 for index in range(90)]
    collection, graph = MemoryCollection(), MemoryGraph()
    old_records, old_papers = dict(collection.records), dict(graph.papers)
    calls = []

    def embed(texts):
        calls.append(list(texts))
        assert collection.writes == [] and graph.writes == []
        if len(calls) == failure_batch:
            if failure == "provider":
                raise RuntimeError("offline provider failure")
            if failure == "short":
                return [[1.0]] * (len(texts) - 1)
            if failure == "extra":
                return [[1.0]] * (len(texts) + 1)
            if failure == "dimension":
                return [[1.0]] * (len(texts) - 1) + [[1.0, 2.0]]
            return [[1.0]] * (len(texts) - 1) + [[float("nan")]]
        return [[1.0] for _ in texts]

    worker = PaperIndexer(
        tmp_path,
        collection=collection,
        graph_store=graph,
        extractor=lambda _: [ExtractedPage(1, "\n\n".join(chunks))],
        metadata_extractor=metadata,
        **embedding_options(embed),
    )
    report = worker.index_directory()
    assert report.outcomes[0].status == "failed"
    assert len(calls) == failure_batch
    assert all(len(call) == 30 for call in calls)
    expected_error = {
        "provider": "offline provider failure",
        "short": "different number of vectors",
        "extra": "different number of vectors",
        "dimension": "same vector dimension",
        "invalid": "vector",
    }
    assert expected_error[failure] in report.outcomes[0].error
    assert collection.records == old_records and graph.papers == old_papers
    assert collection.writes == [] and graph.writes == []


@pytest.mark.parametrize("max_workers", [1, 3])
def test_ndjson_batch_failure_is_persisted_then_retried_while_success_is_skipped(
    tmp_path, monkeypatch, embedding_options, max_workers
):
    from src.cli import __main__ as cli_main, commands
    from src.cli.config import CLIConfig

    for name in ("paper.pdf", "done.pdf"):
        synthetic_pdf(tmp_path, name)
    collection, graph = MemoryCollection(), MemoryGraph()
    settings = CLIConfig(
        database_path=tmp_path / "status.sqlite3",
        papers_directory=tmp_path,
        chroma_directory=tmp_path / "chroma",
        chat_model="offline",
        chat_base_url="https://example.invalid",
        embedding_model="offline",
        embedding_base_url="https://example.invalid",
        api_key=None,
        system_prompt=None,
        log_path=tmp_path / "cli.log",
        retrieval_log_path=tmp_path / "retrieval.log",
    )
    failing = True
    calls = []

    def embed(texts):
        calls.append(list(texts))
        if texts == ["c", "d"] and failing:
            raise RuntimeError("offline batch failure")
        return [[1.0] for _ in texts]

    def build(_):
        return PaperIndexer(
            tmp_path,
            collection=collection,
            graph_store=graph,
            extractor=lambda path: [
                ExtractedPage(1, "done" if path.stem == "done" else "a\n\nb\n\nc\n\nd")
            ],
            metadata_extractor=metadata,
            max_embedding_batch_inputs=2,
            max_workers=max_workers,
            **embedding_options(embed),
        )

    monkeypatch.setattr(commands, "build_paper_indexer", build)
    monkeypatch.setattr(commands, "chromadb_available", lambda: False)
    monkeypatch.setattr(cli_main, "load_dotenv_if_present", lambda: None)
    monkeypatch.setattr(cli_main.CLIConfig, "from_env", lambda: settings)

    def run():
        output = io.StringIO()
        monkeypatch.setattr(
            cli_main.sys,
            "stdin",
            io.StringIO('{"command":"index_papers","stream":true}'),
        )
        monkeypatch.setattr(cli_main.sys, "stdout", output)
        assert cli_main.main() == 0
        events = [json.loads(line) for line in output.getvalue().splitlines()]
        assert events[0] == {"event": "start", "completed": 0, "total": 2}
        assert len([event for event in events if event["event"] == "done"]) == 1
        assert events[-1]["completed"] == events[-1]["total"] == 2
        assert [
            event["completed"] for event in events if event["event"] == "progress"
        ] == [1, 2]
        return events

    first = run()
    assert first[-1] == {
        "event": "done",
        "ok": True,
        "completed": 2,
        "total": 2,
        "indexed": 1,
        "empty": 0,
        "failed": 1,
        "skipped": 0,
    }
    statuses = {
        entry["paper_id"]: entry
        for entry in commands.handle_paper_statuses(settings)["statuses"]
    }
    assert statuses["paper"]["status"] == "failed"
    assert collection.records["paper:old"] == "old evidence"
    assert graph.papers["paper"][0] == "Old"
    assert all(
        value["paper_id"] == "done"
        for kind, value in collection.writes
        if kind == "delete"
    )

    failing = False
    calls.clear()
    second = run()
    assert second[-1]["indexed"] == 1 and second[-1]["skipped"] == 1
    assert second[-1]["failed"] == 0
    assert calls == [["a", "b"], ["c", "d"]]
    statuses = {
        entry["paper_id"]: entry
        for entry in commands.handle_paper_statuses(settings)["statuses"]
    }
    assert statuses["paper"]["status"] == "indexed"
    assert statuses["paper"]["chunk_count"] == 4 and statuses["paper"]["error"] is None
    assert "paper:old" not in collection.records and graph.papers["paper"][0] == "Title"
    calls.clear()
    third = run()
    assert third[-1]["skipped"] == 2 and not calls
