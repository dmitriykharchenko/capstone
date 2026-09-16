import threading
import time
from pathlib import Path
from types import SimpleNamespace

import pytest

from src.paper_indexer import (
    ExtractedPage,
    PDFExtractionError,
    PaperIndexer,
    extract_pdf_pages,
    index_papers_directory,
)


class FakeCollection:
    def __init__(self):
        self.deletes = []
        self.upserts = []

    def delete(self, **kwargs):
        self.deletes.append(kwargs)

    def upsert(self, **kwargs):
        self.upserts.append(kwargs)


class ConcurrencyTrackingCollection:
    """Thread-safe fake that records the peak number of overlapping writers.

    A short sleep inside each write widens the window so that, without the
    indexer's write lock, concurrent workers would overlap and drive
    ``max_active`` above 1.
    """

    def __init__(self):
        self.deletes = []
        self.upserts = []
        self._lock = threading.Lock()
        self._active = 0
        self.max_active = 0

    def _enter(self):
        with self._lock:
            self._active += 1
            self.max_active = max(self.max_active, self._active)

    def _exit(self):
        with self._lock:
            self._active -= 1

    def delete(self, **kwargs):
        self._enter()
        time.sleep(0.01)
        with self._lock:
            self.deletes.append(kwargs)
        self._exit()

    def upsert(self, **kwargs):
        self._enter()
        time.sleep(0.01)
        with self._lock:
            self.upserts.append(kwargs)
        self._exit()


def touch_pdf(directory: Path, name: str) -> Path:
    path = directory / name
    path.write_bytes(b"%PDF-1.4\n")
    return path


def test_extract_pdf_pages_preserves_order_and_empty_pages(tmp_path):
    pdf = touch_pdf(tmp_path, "paper.pdf")
    pages = [SimpleNamespace(extract_text=lambda: "first"), SimpleNamespace(extract_text=lambda: None)]

    result = extract_pdf_pages(pdf, reader_factory=lambda path: SimpleNamespace(pages=pages))

    assert result == [ExtractedPage(1, "first"), ExtractedPage(2, "")]


def test_extract_pdf_pages_reports_open_and_page_failures(tmp_path):
    pdf = touch_pdf(tmp_path, "paper.pdf")

    def bad_reader(path):
        raise ValueError("broken xref")

    with pytest.raises(PDFExtractionError, match="unable to read PDF paper.pdf: broken xref"):
        extract_pdf_pages(pdf, reader_factory=bad_reader)

    page = SimpleNamespace(extract_text=lambda: (_ for _ in ()).throw(ValueError("encrypted")))
    with pytest.raises(PDFExtractionError, match="extract page 1.*encrypted"):
        extract_pdf_pages(pdf, reader_factory=lambda path: SimpleNamespace(pages=[page]))


def test_index_paper_chunks_embeds_and_writes_provenance(tmp_path):
    pdf = touch_pdf(tmp_path, "safe-paper.pdf")
    collection = FakeCollection()
    embedded = []

    def embed(text):
        embedded.append(text)
        return [float(len(text)), 1.0]

    indexer = PaperIndexer(
        tmp_path,
        collection=collection,
        extractor=lambda path: [
            ExtractedPage(1, "one\n\ntwo"),
            ExtractedPage(2, "three"),
        ],
        embedder=embed,
    )

    outcome = indexer.index_paper(pdf)

    assert outcome.status == "indexed" and outcome.chunk_count == 3
    assert embedded == ["one", "two", "three"]
    assert collection.deletes == [{"where": {"paper_id": "safe-paper"}}]
    write = collection.upserts[0]
    assert write["ids"] == [
        "safe-paper:chunk:000000",
        "safe-paper:chunk:000001",
        "safe-paper:chunk:000002",
    ]
    assert write["documents"] == embedded
    assert [item["chunk_index"] for item in write["metadatas"]] == [0, 1, 2]
    assert [item["page"] for item in write["metadatas"]] == [1, 1, 2]
    assert all(item["paper_id"] == "safe-paper" for item in write["metadatas"])
    assert all(item["filename"] == "safe-paper.pdf" for item in write["metadatas"])
    assert all(item["title"] == "safe paper" for item in write["metadatas"])


def test_index_paper_bounds_chunks_before_embedding_and_preserves_provenance(tmp_path):
    pdf = touch_pdf(tmp_path, "safe-paper.pdf")
    collection = FakeCollection()
    embedded = []

    def embed(text):
        embedded.append(text)
        return [float(len(text))]

    indexer = PaperIndexer(
        tmp_path,
        collection=collection,
        extractor=lambda path: [
            ExtractedPage(1, "Alpha one. Beta two. Gamma three."),
            ExtractedPage(2, "Final."),
        ],
        embedder=embed,
        max_chunk_bytes=22,
    )

    outcome = indexer.index_paper(pdf)

    expected = ["Alpha one. Beta two.", "Gamma three.", "Final."]
    assert outcome.chunk_count == 3
    assert embedded == expected
    write = collection.upserts[0]
    assert write["documents"] == expected
    assert write["ids"] == [
        "safe-paper:chunk:000000",
        "safe-paper:chunk:000001",
        "safe-paper:chunk:000002",
    ]
    assert [item["page"] for item in write["metadatas"]] == [1, 1, 2]
    assert [item["chunk_index"] for item in write["metadatas"]] == [0, 1, 2]


def test_index_paper_batches_all_chunks_into_one_embed_call(tmp_path):
    pdf = touch_pdf(tmp_path, "safe-paper.pdf")
    collection = FakeCollection()
    batch_calls = []

    def embed_batch(texts):
        chunk_texts = list(texts)
        batch_calls.append(chunk_texts)
        return [[float(len(text)), 1.0] for text in chunk_texts]

    indexer = PaperIndexer(
        tmp_path,
        collection=collection,
        extractor=lambda path: [
            ExtractedPage(1, "one\n\ntwo"),
            ExtractedPage(2, "three"),
        ],
        batch_embedder=embed_batch,
    )

    outcome = indexer.index_paper(pdf)

    assert outcome.status == "indexed" and outcome.chunk_count == 3
    assert batch_calls == [["one", "two", "three"]]
    write = collection.upserts[0]
    assert write["documents"] == ["one", "two", "three"]
    assert write["embeddings"] == [[3.0, 1.0], [3.0, 1.0], [5.0, 1.0]]


def test_empty_paper_skips_batch_embedder(tmp_path):
    pdf = touch_pdf(tmp_path, "empty.pdf")
    collection = FakeCollection()
    indexer = PaperIndexer(
        tmp_path,
        collection=collection,
        extractor=lambda path: [ExtractedPage(1, " \n\n ")],
        batch_embedder=lambda texts: pytest.fail("empty paper must not be embedded"),
    )

    outcome = indexer.index_paper(pdf)

    assert outcome.status == "empty" and outcome.chunk_count == 0
    assert collection.deletes == [{"where": {"paper_id": "empty"}}]
    assert collection.upserts == []


def test_reindex_deletes_before_each_replacement_and_ids_are_stable(tmp_path):
    pdf = touch_pdf(tmp_path, "paper.pdf")
    collection = FakeCollection()
    pages = [ExtractedPage(1, "old one\n\nold two")]
    indexer = PaperIndexer(
        tmp_path,
        collection=collection,
        extractor=lambda path: pages,
        embedder=lambda text: [1.0],
    )

    indexer.index_paper(pdf)
    pages[:] = [ExtractedPage(1, "new")]
    indexer.index_paper(pdf)

    assert len(collection.deletes) == 2
    assert collection.upserts[0]["ids"][0] == collection.upserts[1]["ids"][0]
    assert collection.upserts[1]["documents"] == ["new"]


def test_empty_paper_removes_stale_records_without_embedding_or_write(tmp_path):
    pdf = touch_pdf(tmp_path, "empty.pdf")
    collection = FakeCollection()
    indexer = PaperIndexer(
        tmp_path,
        collection=collection,
        extractor=lambda path: [ExtractedPage(1, " \n\n ")],
        embedder=lambda text: pytest.fail("empty paper must not be embedded"),
    )

    outcome = indexer.index_paper(pdf)

    assert outcome.status == "empty" and outcome.chunk_count == 0
    assert collection.deletes == [{"where": {"paper_id": "empty"}}]
    assert collection.upserts == []


def test_directory_scan_is_top_level_sorted_and_isolates_failures(tmp_path):
    touch_pdf(tmp_path, "b.pdf")
    touch_pdf(tmp_path, "A.PDF")
    touch_pdf(tmp_path, "unsafe name.pdf")
    (tmp_path / "ignored.txt").write_text("text")
    nested = tmp_path / "nested"
    nested.mkdir()
    touch_pdf(nested, "nested.pdf")
    collection = FakeCollection()

    report = index_papers_directory(
        tmp_path,
        collection=collection,
        extractor=lambda path: [ExtractedPage(1, path.stem)],
        embedder=lambda text: [1.0],
    )

    assert [outcome.filename for outcome in report.outcomes] == [
        "A.PDF",
        "b.pdf",
        "unsafe name.pdf",
    ]
    assert [outcome.status for outcome in report.outcomes] == ["indexed", "indexed", "failed"]
    assert "unsafe PDF filename stem" in report.outcomes[-1].error
    assert len(collection.upserts) == 2


def test_directory_iterator_reports_determinate_progress_and_failures(tmp_path):
    touch_pdf(tmp_path, "b.pdf")
    touch_pdf(tmp_path, "A.pdf")
    # max_workers=1 exercises the ordered, single-threaded streaming protocol.
    indexer = PaperIndexer(
        tmp_path,
        collection=FakeCollection(),
        extractor=lambda path: (
            (_ for _ in ()).throw(RuntimeError("unreadable"))
            if path.name == "b.pdf"
            else [ExtractedPage(1, "content")]
        ),
        embedder=lambda text: [1.0],
        max_workers=1,
    )

    progress = list(indexer.iter_directory())

    # A start update, then a started/outcome pair per paper in filename order.
    assert [(item.completed, item.total) for item in progress] == [
        (0, 2),
        (0, 2),
        (1, 2),
        (1, 2),
        (2, 2),
    ]
    assert progress[0].outcome is None and progress[0].started is None
    assert progress[1].outcome is None
    assert progress[1].started.paper_id == "A"
    assert progress[1].started.filename == "A.pdf"
    assert progress[2].outcome.filename == "A.pdf"
    assert progress[2].outcome.status == "indexed"
    assert progress[3].started.filename == "b.pdf"
    assert progress[4].outcome.filename == "b.pdf"
    assert progress[4].outcome.status == "failed"
    assert progress[4].outcome.error == "RuntimeError: unreadable"


def test_directory_iterator_skips_already_indexed_papers_without_work(tmp_path):
    touch_pdf(tmp_path, "keep.pdf")
    touch_pdf(tmp_path, "fresh.pdf")
    collection = FakeCollection()
    # max_workers=1 keeps the skip/started/outcome stream in filename order.
    indexer = PaperIndexer(
        tmp_path,
        collection=collection,
        extractor=lambda path: (
            pytest.fail(f"skipped paper must not be extracted: {path.name}")
            if path.name == "keep.pdf"
            else [ExtractedPage(1, "content")]
        ),
        embedder=lambda text: [1.0],
        max_workers=1,
    )

    progress = list(indexer.iter_directory({"keep"}))
    outcomes = [item.outcome for item in progress if item.outcome is not None]

    # "fresh" is indexed normally; "keep" is reported skipped with no started
    # event and no collection writes for it.
    assert [(o.filename, o.status) for o in outcomes] == [
        ("fresh.pdf", "indexed"),
        ("keep.pdf", "skipped"),
    ]
    assert not any(item.started and item.started.paper_id == "keep" for item in progress)
    assert collection.deletes == [{"where": {"paper_id": "fresh"}}]
    assert all(write["metadatas"][0]["paper_id"] == "fresh" for write in collection.upserts)


def test_concurrent_indexing_runs_papers_in_parallel(tmp_path):
    for name in ("a.pdf", "b.pdf", "c.pdf"):
        touch_pdf(tmp_path, name)
    # Each paper's embed must reach the barrier simultaneously; a sequential
    # run could never gather all three, so the barrier would time out.
    barrier = threading.Barrier(3, timeout=5)

    def embed_batch(texts):
        barrier.wait()
        return [[1.0, 2.0] for _ in texts]

    indexer = PaperIndexer(
        tmp_path,
        collection=ConcurrencyTrackingCollection(),
        extractor=lambda path: [ExtractedPage(1, "content")],
        batch_embedder=embed_batch,
        max_workers=3,
    )

    report = indexer.index_directory()

    assert [outcome.status for outcome in report.outcomes] == ["indexed"] * 3


def test_concurrent_indexing_serializes_collection_writes(tmp_path):
    for name in ("a.pdf", "b.pdf", "c.pdf", "d.pdf"):
        touch_pdf(tmp_path, name)
    collection = ConcurrencyTrackingCollection()
    indexer = PaperIndexer(
        tmp_path,
        collection=collection,
        extractor=lambda path: [ExtractedPage(1, "content")],
        batch_embedder=lambda texts: [[1.0, 2.0] for _ in texts],
        max_workers=4,
    )

    report = indexer.index_directory()

    assert [outcome.status for outcome in report.outcomes] == ["indexed"] * 4
    # The write lock must prevent any two workers from writing at once.
    assert collection.max_active == 1
    assert len(collection.upserts) == 4


def test_concurrent_iter_directory_emits_all_events_with_monotonic_completed(tmp_path):
    ids = [f"p{index}" for index in range(5)]
    for paper_id in ids:
        touch_pdf(tmp_path, f"{paper_id}.pdf")
    indexer = PaperIndexer(
        tmp_path,
        collection=ConcurrencyTrackingCollection(),
        extractor=lambda path: [ExtractedPage(1, "content")],
        batch_embedder=lambda texts: [[1.0] for _ in texts],
        max_workers=5,
    )

    progress = list(indexer.iter_directory())

    assert progress[0].completed == 0 and progress[0].total == 5
    started = [item.started.paper_id for item in progress if item.started is not None]
    outcomes = [item.outcome for item in progress if item.outcome is not None]
    # Every paper announces a start and a terminal outcome, in any order.
    assert sorted(started) == ids
    assert sorted(outcome.paper_id for outcome in outcomes) == ids
    assert all(outcome.status == "indexed" for outcome in outcomes)
    # The completed counter advances by one per outcome and ends at total.
    completed_on_outcomes = [item.completed for item in progress if item.outcome is not None]
    assert completed_on_outcomes == [1, 2, 3, 4, 5]
    assert progress[-1].completed == 5


def test_concurrent_indexing_isolates_failures_and_skips(tmp_path):
    for name in ("good.pdf", "bad.pdf", "done.pdf"):
        touch_pdf(tmp_path, name)
    indexer = PaperIndexer(
        tmp_path,
        collection=ConcurrencyTrackingCollection(),
        extractor=lambda path: (
            (_ for _ in ()).throw(RuntimeError("boom"))
            if path.name == "bad.pdf"
            else [ExtractedPage(1, "content")]
        ),
        batch_embedder=lambda texts: [[1.0] for _ in texts],
        max_workers=3,
    )

    report = indexer.index_directory({"done"})

    by_id = {outcome.paper_id: outcome for outcome in report.outcomes}
    assert by_id["good"].status == "indexed"
    assert by_id["bad"].status == "failed"
    assert "RuntimeError: boom" in by_id["bad"].error
    assert by_id["done"].status == "skipped"


@pytest.mark.parametrize("value", [0, -1, True, 1.5, "many"])
def test_paper_indexer_rejects_invalid_max_workers(tmp_path, value):
    with pytest.raises((TypeError, ValueError)):
        PaperIndexer(tmp_path, collection=FakeCollection(), max_workers=value)


def test_injected_client_opens_existing_paper_collection(tmp_path):
    collection = FakeCollection()
    client = SimpleNamespace(
        get_or_create_collection=lambda **kwargs: collection,
    )

    indexer = PaperIndexer(tmp_path, client=client, persist_directory=tmp_path / "db")

    assert indexer.collection is collection


def test_embedding_failure_preserves_existing_records(tmp_path):
    pdf = touch_pdf(tmp_path, "paper.pdf")
    collection = FakeCollection()
    indexer = PaperIndexer(
        tmp_path,
        collection=collection,
        extractor=lambda path: [ExtractedPage(1, "content")],
        embedder=lambda text: (_ for _ in ()).throw(RuntimeError("offline")),
    )

    with pytest.raises(RuntimeError, match="offline"):
        indexer.index_paper(pdf)

    assert collection.deletes == [] and collection.upserts == []