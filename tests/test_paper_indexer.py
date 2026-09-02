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