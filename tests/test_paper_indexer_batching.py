import io
import json
from types import SimpleNamespace

import pytest

from src.embeddings import EmbeddingConfig
from src.paper_indexer import ExtractedPage, PaperIndexer
from src.paper_metadata import PaperMetadata


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


@pytest.mark.parametrize(
    ("count", "chunk_bytes", "expected_sizes"),
    [
        (3, 8, [3]),
        (64, 8, [64]),
        (65, 8, [64, 1]),
        (129, 8, [64, 64, 1]),
        (30, 8000, [30]),
        (31, 8000, [30, 1]),
        (91, 8000, [30, 30, 30, 1]),
    ],
)
def test_default_batch_boundaries_preserve_all_provenance_before_writes(
    tmp_path, embedding_options, count, chunk_bytes, expected_sizes
):
    pdf = synthetic_pdf(tmp_path)
    chunks = [f"{index:04d}:" + "x" * (chunk_bytes - 5) for index in range(count)]
    pages = [
        ExtractedPage(page + 1, "\n\n".join(chunks[offset : offset + 3]))
        for page, offset in enumerate(range(0, count, 3))
    ]
    collection, graph = MemoryCollection(), MemoryGraph()
    calls = []

    def embed(texts):
        assert collection.writes == [] and graph.writes == []
        assert len(texts) <= 64
        assert sum(len(text.encode("utf-8")) for text in texts) <= 240_000
        calls.append(list(texts))
        return [[float(text[:4]), 1.0] for text in texts]

    worker = PaperIndexer(
        tmp_path,
        collection=collection,
        graph_store=graph,
        metadata_extractor=metadata,
        extractor=lambda _: pages,
        **embedding_options(embed),
    )
    outcome = worker.index_paper(pdf)

    assert outcome.status == "indexed" and outcome.chunk_count == count
    assert [len(call) for call in calls] == expected_sizes
    assert [text for call in calls for text in call] == chunks
    assert [kind for kind, _ in collection.writes] == ["delete", "upsert"]
    write = collection.writes[1][1]
    assert write["documents"] == chunks
    assert write["embeddings"] == [[float(index), 1.0] for index in range(count)]
    assert write["ids"] == [f"paper:chunk:{index:06d}" for index in range(count)]
    assert [meta["chunk_index"] for meta in write["metadatas"]] == list(range(count))
    assert [meta["page"] for meta in write["metadatas"]] == [
        index // 3 + 1 for index in range(count)
    ]
    for meta in write["metadatas"]:
        assert meta["paper_id"] == "paper" and meta["filename"] == "paper.pdf"
        assert meta["title"] == "Title" and json.loads(meta["authors"]) == ["Ada"]
        assert json.loads(meta["topics"]) == ["graphs", "retrieval", "testing"]
        assert meta["metadata_version"] == 1
        assert meta["content_fingerprint"] == worker._content_fingerprint(pages)
    assert graph.writes == [("replace", "paper")]
    assert "paper:old" not in collection.records


def test_custom_budgets_count_utf8_bytes_and_reproduce_exact_batches(
    tmp_path, embedding_options
):
    pdf = synthetic_pdf(tmp_path)
    chunks = ["éé", "éé", "x", "🙂", "abc", "z"]
    calls = []

    def embed(texts):
        calls.append(list(texts))
        return [[float(len(text.encode("utf-8")))] for text in texts]

    worker = PaperIndexer(
        tmp_path,
        collection=MemoryCollection(),
        extractor=lambda _: [ExtractedPage(1, "\n\n".join(chunks))],
        max_chunk_bytes=8,
        max_embedding_batch_inputs=3,
        max_embedding_batch_bytes=8,
        **embedding_options(embed),
    )
    worker.index_paper(pdf)
    worker.index_paper(pdf)
    expected = [["éé", "éé"], ["x", "🙂", "abc"], ["z"]]
    assert calls == expected * 2


def test_minimum_budgets_and_oversized_unicode_chunks(tmp_path, embedding_options):
    pdf = synthetic_pdf(tmp_path)
    calls = []

    def embed(texts):
        calls.append(list(texts))
        return [[1.0] for _ in texts]

    for text, byte_limit, expected in [
        ("abc", 1, ["a", "b", "c"]),
        ("🙂🙂", 4, ["🙂", "🙂"]),
    ]:
        calls.clear()
        collection = MemoryCollection()
        worker = PaperIndexer(
            tmp_path,
            collection=collection,
            extractor=lambda _, text=text: [ExtractedPage(7, text)],
            max_chunk_bytes=byte_limit,
            max_embedding_batch_inputs=1,
            max_embedding_batch_bytes=byte_limit,
            **embedding_options(embed),
        )
        worker.index_paper(pdf)
        assert calls == [[chunk] for chunk in expected]
        assert [meta["page"] for meta in collection.writes[1][1]["metadatas"]] == [
            7
        ] * len(expected)


@pytest.mark.parametrize(
    "name", ["max_embedding_batch_inputs", "max_embedding_batch_bytes"]
)
@pytest.mark.parametrize("value", [0, -1, True, False, 1.5, "64", None])
def test_invalid_limits_fail_before_collection_or_provider_work(
    tmp_path, monkeypatch, name, value
):
    def forbidden(*_args, **_kwargs):
        pytest.fail("invalid limits must fail before collection or provider work")

    monkeypatch.setattr("src.paper_indexer.open_collection", forbidden)
    with pytest.raises((TypeError, ValueError), match=name):
        PaperIndexer(
            tmp_path,
            batch_embedder=forbidden,
            metadata_extractor=forbidden,
            **{name: value},
        )


def test_batch_byte_limit_must_accommodate_chunk_ceiling(tmp_path, monkeypatch):
    monkeypatch.setattr(
        "src.paper_indexer.open_collection",
        lambda *_args, **_kwargs: pytest.fail("must not open"),
    )
    with pytest.raises(ValueError, match="must be at least max_chunk_bytes"):
        PaperIndexer(tmp_path, max_embedding_batch_bytes=7999)


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


def test_cross_batch_dimension_change_fails_before_replacement(
    tmp_path, embedding_options
):
    pdf = synthetic_pdf(tmp_path)
    collection = MemoryCollection()
    worker = PaperIndexer(
        tmp_path,
        collection=collection,
        extractor=lambda _: [ExtractedPage(1, "a\n\nb")],
        max_embedding_batch_inputs=1,
        **embedding_options(lambda texts: [[1.0] if texts == ["a"] else [1.0, 2.0]]),
    )
    with pytest.raises(RuntimeError, match="same vector dimension"):
        worker.index_paper(pdf)
    assert collection.records == {"paper:old": "old evidence"}
    assert collection.writes == []


def test_empty_paper_removes_stale_stores_without_provider_calls(
    tmp_path, embedding_options
):
    pdf = synthetic_pdf(tmp_path)
    collection, graph = MemoryCollection(), MemoryGraph()
    worker = PaperIndexer(
        tmp_path,
        collection=collection,
        graph_store=graph,
        extractor=lambda _: [ExtractedPage(1, " \n\n ")],
        metadata_extractor=lambda _: pytest.fail("empty metadata call"),
        **embedding_options(lambda _: pytest.fail("empty embedding call")),
    )
    assert worker.index_paper(pdf).status == "empty"
    assert not collection.records and not graph.papers
    assert collection.writes == [("delete", {"paper_id": "paper"})]
    assert graph.writes == [("remove", "paper")]


def test_single_text_embedder_stays_per_chunk_and_batch_override_keeps_precedence(
    tmp_path,
):
    pdf = synthetic_pdf(tmp_path)
    calls = []

    def embed(text):
        calls.append(text)
        return [1.0]

    options = {
        "collection": MemoryCollection(),
        "extractor": lambda _: [ExtractedPage(1, "a\n\nb\n\nc")],
        "embedder": embed,
        "max_embedding_batch_inputs": 2,
    }
    PaperIndexer(tmp_path, **options).index_paper(pdf)
    assert calls == ["a", "b", "c"]
    calls.clear()

    def embed_batch(texts):
        calls.append(list(texts))
        return [[1.0] for _ in texts]

    PaperIndexer(tmp_path, batch_embedder=embed_batch, **options).index_paper(pdf)
    assert calls == [["a", "b"], ["c"]]


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
