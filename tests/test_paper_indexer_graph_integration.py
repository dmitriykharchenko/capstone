import json
import threading
from pathlib import Path

import pytest

from src.paper_indexer import ExtractedPage, PaperIndexer
from src.paper_metadata import PaperMetadata
from src.paper_search import PaperSearchIndex


class RecordingCollection:
    def __init__(self):
        self.deletes = []
        self.upserts = []

    def delete(self, **kwargs):
        self.deletes.append(kwargs)

    def upsert(self, **kwargs):
        self.upserts.append(kwargs)


class MemoryGraph:
    def __init__(self):
        self.papers = {}
        self.replacements = []
        self.removals = []
        self.fail_replace = False
        self.fail_remove = False
        self._lock = threading.Lock()

    def replace_paper(self, paper_id, title, authors, topics):
        if self.fail_replace:
            raise RuntimeError("graph unavailable")
        value = (title, tuple(authors), tuple(topics))
        with self._lock:
            self.papers[paper_id] = value
            self.replacements.append((paper_id, *value))

    def remove_paper(self, paper_id):
        if self.fail_remove:
            raise RuntimeError("graph unavailable")
        with self._lock:
            self.removals.append(paper_id)
            return self.papers.pop(paper_id, None) is not None


def touch_pdf(directory: Path, name: str = "paper.pdf") -> Path:
    path = directory / name
    path.write_bytes(b"%PDF-1.4\n")
    return path


def metadata(title="Graph Paper", authors=("Ada", "Grace"), topics=None):
    return PaperMetadata(
        title,
        authors,
        topics or ("graphs", "retrieval", "databases"),
    )


def indexer(tmp_path, collection, graph, extractor, extractor_metadata=metadata):
    return PaperIndexer(
        tmp_path,
        collection=collection,
        graph_store=graph,
        extractor=extractor,
        metadata_extractor=extractor_metadata,
        embedder=lambda text: [float(len(text))],
    )


def test_enriched_index_extracts_once_calls_metadata_once_and_writes_provenance(
    tmp_path,
):
    pdf = touch_pdf(tmp_path)
    collection = RecordingCollection()
    graph = MemoryGraph()
    extraction_calls = []
    metadata_calls = []

    def extract(path):
        extraction_calls.append(path)
        return [ExtractedPage(1, "first\n\nsecond"), ExtractedPage(2, "third")]

    def extract_metadata(text):
        metadata_calls.append(text)
        return metadata()

    outcome = indexer(
        tmp_path, collection, graph, extract, extract_metadata
    ).index_paper(pdf)

    assert outcome.status == "indexed" and outcome.chunk_count == 3
    assert extraction_calls == [pdf]
    assert metadata_calls == ["first\n\nsecond\n\nthird"]
    assert graph.papers["paper"] == (
        "Graph Paper",
        ("Ada", "Grace"),
        ("graphs", "retrieval", "databases"),
    )
    stored = collection.upserts[0]["metadatas"]
    assert [item["page"] for item in stored] == [1, 1, 2]
    assert all(item["title"] == "Graph Paper" for item in stored)
    assert all(json.loads(item["authors"]) == ["Ada", "Grace"] for item in stored)
    assert all(
        json.loads(item["topics"]) == ["graphs", "retrieval", "databases"]
        for item in stored
    )
    assert len({item["content_fingerprint"] for item in stored}) == 1
    assert all(item["metadata_version"] == 1 for item in stored)


def test_reindex_replaces_stale_graph_relations_and_changes_fingerprint(tmp_path):
    pdf = touch_pdf(tmp_path)
    collection = RecordingCollection()
    graph = MemoryGraph()
    pages = [ExtractedPage(1, "old")]
    values = [metadata(), metadata("Revised", ("Alan",), ("logic", "ai", "proofs"))]

    def extract_metadata(_text):
        return values.pop(0)

    worker = indexer(
        tmp_path, collection, graph, lambda _path: list(pages), extract_metadata
    )
    worker.index_paper(pdf)
    pages[:] = [ExtractedPage(1, "new")]
    worker.index_paper(pdf)

    assert graph.papers["paper"] == (
        "Revised",
        ("Alan",),
        ("logic", "ai", "proofs"),
    )
    first, second = collection.upserts
    assert (
        first["metadatas"][0]["content_fingerprint"]
        != second["metadatas"][0]["content_fingerprint"]
    )


def test_empty_paper_removes_stale_graph_and_chroma_without_llm_or_embedding(tmp_path):
    pdf = touch_pdf(tmp_path)
    collection = RecordingCollection()
    graph = MemoryGraph()
    graph.papers["paper"] = ("Old", ("Ada",), ("graphs",))
    worker = PaperIndexer(
        tmp_path,
        collection=collection,
        graph_store=graph,
        extractor=lambda _path: [ExtractedPage(1, " \n\n ")],
        metadata_extractor=lambda _text: pytest.fail(
            "empty paper must not call metadata"
        ),
        embedder=lambda _text: pytest.fail("empty paper must not embed"),
    )

    outcome = worker.index_paper(pdf)

    assert outcome.status == "empty"
    assert graph.papers == {} and graph.removals == ["paper"]
    assert collection.deletes == [{"where": {"paper_id": "paper"}}]
    assert collection.upserts == []


def test_extraction_failure_isolated_and_preserves_existing_stores(tmp_path):
    touch_pdf(tmp_path)
    collection = RecordingCollection()
    graph = MemoryGraph()
    graph.papers["paper"] = ("Old", ("Ada",), ("graphs",))

    def fail(_path):
        raise RuntimeError("unreadable")

    report = indexer(tmp_path, collection, graph, fail).index_directory()

    assert report.outcomes[0].status == "failed"
    assert report.outcomes[0].error == "RuntimeError: unreadable"
    assert graph.papers["paper"][0] == "Old"
    assert collection.deletes == [] and collection.upserts == []


def test_graph_failure_isolated_and_preserves_existing_stores_transactionally(tmp_path):
    touch_pdf(tmp_path)
    collection = RecordingCollection()
    graph = MemoryGraph()
    graph.papers["paper"] = ("Old", ("Ada",), ("graphs",))
    graph.fail_replace = True

    report = indexer(
        tmp_path,
        collection,
        graph,
        lambda _path: [ExtractedPage(1, "new content")],
    ).index_directory()

    assert report.outcomes[0].status == "failed"
    assert report.outcomes[0].error == "RuntimeError: graph unavailable"
    assert graph.papers["paper"][0] == "Old"
    assert collection.deletes == [] and collection.upserts == []


def test_invalid_metadata_is_rejected_before_any_write(tmp_path):
    pdf = touch_pdf(tmp_path)
    collection = RecordingCollection()
    graph = MemoryGraph()
    worker = indexer(
        tmp_path,
        collection,
        graph,
        lambda _path: [ExtractedPage(1, "content")],
        lambda _text: {"title": "not validated"},
    )

    with pytest.raises(TypeError, match="must return PaperMetadata"):
        worker.index_paper(pdf)

    assert graph.papers == {}
    assert collection.deletes == [] and collection.upserts == []


def test_fingerprint_is_stable_across_identical_reindexing(tmp_path):
    pdf = touch_pdf(tmp_path)
    collection = RecordingCollection()
    graph = MemoryGraph()
    pages = [ExtractedPage(1, "same"), ExtractedPage(2, "content")]
    worker = indexer(tmp_path, collection, graph, lambda _path: list(pages))

    worker.index_paper(pdf)
    worker.index_paper(pdf)

    fingerprints = [
        write["metadatas"][0]["content_fingerprint"] for write in collection.upserts
    ]
    assert fingerprints[0] == fingerprints[1]


def test_directory_indexing_continues_with_deterministic_progress(tmp_path):
    for name in ("c.pdf", "A.pdf", "b.pdf"):
        touch_pdf(tmp_path, name)
    collection = RecordingCollection()
    graph = MemoryGraph()
    worker = PaperIndexer(
        tmp_path,
        collection=collection,
        graph_store=graph,
        extractor=lambda path: [ExtractedPage(1, path.stem)],
        metadata_extractor=lambda text: metadata(text),
        embedder=lambda _text: [1.0],
    )

    progress = list(worker.iter_directory())
    outcomes = [item.outcome for item in progress if item.outcome is not None]

    assert [outcome.paper_id for outcome in outcomes] == ["A", "b", "c"]
    assert [item.completed for item in progress if item.outcome is not None] == [
        1,
        2,
        3,
    ]
    assert progress[0].completed == 0 and progress[-1].completed == 3


def test_enriched_metadata_remains_backward_compatible_with_paper_search(tmp_path):
    pdf = touch_pdf(tmp_path)
    collection = RecordingCollection()
    graph = MemoryGraph()
    indexer(
        tmp_path,
        collection,
        graph,
        lambda _path: [ExtractedPage(1, "graph evidence")],
        lambda _text: metadata(),
    ).index_paper(pdf)
    write = collection.upserts[0]

    class QueryCollection:
        def query(self, **_kwargs):
            return {
                "metadatas": [write["metadatas"]],
                "documents": [write["documents"]],
                "distances": [[0.1]],
            }

    matches = PaperSearchIndex(
        collection=QueryCollection(), embedder=lambda _text: [1.0]
    ).search("graph")

    assert matches[0].paper_id == "paper"
    assert matches[0].title == "Graph Paper"
    assert matches[0].filename == "paper.pdf" and matches[0].page == 1
