import csv
import hashlib

import pytest

from src.indexing import pdf_to_markdown as converter
from src.indexing.paper_indexer import (
    PDFExtractionError,
    PaperIndexOutcome,
    PaperIndexer,
)


ROWS = [
    ["Label", "Label", ""],
    ["CSV_ONLY", "001", 'comma, quote " and\n\nmultiline é'],
    ["second row", "", "=not executed"],
]


def fake_convert(source, staging):
    digest = hashlib.sha256(source.read_bytes()).hexdigest()
    (staging / "document.md").write_text(
        f"<!-- pdf-artifacts: v1 pages=2 sha256={digest} -->\n"
        "\n<!-- pdf-page: 1 -->\n\n<!-- pdf-page: 2 -->\n"
        "Prose before\n\n<!-- pdf-table: 1 -->\n"
        "|Label|Score|\n|---|---|\n|MARKDOWN_ONLY|999|\n\n"
        "[Table 1 on page 2 (CSV)](document-page-0002-table-0001.csv)\n"
        "<!-- /pdf-table -->\n\nProse after\n\n![](figure.png)\n",
        encoding="utf-8",
    )
    with (staging / "document-page-0002-table-0001.csv").open(
        "w", encoding="utf-8", newline=""
    ) as stream:
        csv.writer(stream).writerows(ROWS)
    (staging / "figure.png").write_bytes(b"fake image")


class Stores:
    def __init__(self):
        self.writes = []
        self.records = {"paper:old": "previous evidence"}
        self.papers = {"paper": ("Old title", ("Ada",), ("topic",))}

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

    def replace_paper(self, paper_id, title, authors, topics):
        self.writes.append(("replace_paper", paper_id))
        self.papers[paper_id] = (title, authors, topics)

    def remove_paper(self, paper_id):
        self.writes.append(("remove_paper", paper_id))
        self.papers.pop(paper_id, None)


@pytest.fixture
def fixture(tmp_path, monkeypatch):
    pdf = tmp_path / "paper.pdf"
    pdf.write_bytes(b"%PDF synthetic offline seam")
    monkeypatch.setattr(converter, "_extract_pdf", fake_convert)
    return pdf, Stores()


def no_call(*_args):
    pytest.fail("backfill/failure must not call a provider")


def protected_indexer(pdf, stores, **options):
    return PaperIndexer(
        pdf.parent,
        collection=stores,
        graph_store=stores,
        metadata_extractor=no_call,
        batch_embedder=no_call,
        **options,
    )


def snapshot(directory):
    return {
        path.name: path.read_bytes() for path in directory.iterdir() if path.is_file()
    }


@pytest.mark.parametrize("workers", [1, 3])
@pytest.mark.parametrize(
    "state", ["missing", "legacy", "missing_csv", "missing_image", "complete"]
)
def test_skipped_backfill_has_unchanged_events_and_zero_store_writes(
    fixture, workers, state
):
    pdf, stores = fixture
    if state != "missing":
        artifacts = converter.ensure_artifacts(pdf)
        if state == "legacy":
            artifacts.markdown_path.write_text("# Legacy")
            artifacts.pages[1].tables[0].csv_path.write_bytes(b"preserved CSV edits")
        elif state == "missing_csv":
            artifacts.pages[1].tables[0].csv_path.rename(pdf.parent / "saved.csv")
        elif state == "missing_image":
            artifacts.images[0].rename(pdf.parent / "saved.png")
    before = snapshot(pdf.parent)
    records, papers = dict(stores.records), dict(stores.papers)
    events = list(
        protected_indexer(pdf, stores, max_workers=workers).iter_directory({"paper"})
    )
    assert [(event.completed, event.total) for event in events] == [(0, 1), (1, 1)]
    assert all(event.started is None for event in events)
    assert events[-1].outcome == PaperIndexOutcome("paper", "paper.pdf", "skipped")
    assert converter.inspect_artifacts(pdf).missing == ()
    assert stores.writes == [] and stores.records == records and stores.papers == papers
    for name, content in before.items():
        if not (state == "legacy" and name == "paper.md"):
            assert (pdf.parent / name).read_bytes() == content


@pytest.mark.parametrize(
    "data", [b"", b'Name,Value\n"unterminated', b"a,b\n1\n", b"a\n\xff", b"a\n\x00\n"]
)
def test_invalid_csv_fails_before_metadata_embeddings_or_store_writes(fixture, data):
    pdf, stores = fixture
    artifacts = converter.ensure_artifacts(pdf)
    artifacts.pages[1].tables[0].csv_path.write_bytes(data)
    before = snapshot(pdf.parent)
    with pytest.raises(PDFExtractionError, match="CSV table"):
        protected_indexer(pdf, stores).index_paper(pdf)
    assert stores.writes == [] and stores.records == {"paper:old": "previous evidence"}
    assert snapshot(pdf.parent) == before


@pytest.mark.parametrize("state", ["conversion", "stale", "markers", "csv_symlink"])
def test_conversion_and_provenance_fail_before_store_mutation(
    fixture, monkeypatch, state
):
    pdf, stores = fixture
    if state == "conversion":

        def fail(*_args):
            raise RuntimeError("private diagnostic must not escape")

        monkeypatch.setattr(converter, "_extract_pdf", fail)
    else:
        artifacts = converter.ensure_artifacts(pdf)
        if state == "stale":
            pdf.write_bytes(b"changed PDF")
        elif state == "markers":
            artifacts.markdown_path.write_text(
                artifacts.markdown_path.read_text().replace(
                    "pdf-page: 2", "pdf-page: 0"
                )
            )
        else:
            table = artifacts.pages[1].tables[0].csv_path
            table.rename(pdf.parent / "saved.csv")
            table.symlink_to(pdf)
    before = snapshot(pdf.parent)
    with pytest.raises(PDFExtractionError) as error:
        protected_indexer(pdf, stores).index_paper(pdf)
    assert "private diagnostic" not in str(error.value)
    assert stores.writes == []
    assert snapshot(pdf.parent) == before


def test_real_generated_pdf_creates_and_indexes_adjacent_artifacts(tmp_path):
    pymupdf = pytest.importorskip("pymupdf")
    pytest.importorskip("pymupdf4llm")
    pdf = tmp_path / "generated.pdf"
    with pymupdf.open() as document:
        document.new_page()
        page = document.new_page(width=600, height=800)
        page.insert_text((50, 50), "Actual page two prose", fontsize=16)
        for y in (150, 185, 220):
            page.draw_line((50, y), (450, y))
        for x in (50, 250, 450):
            page.draw_line((x, 150), (x, 220))
        for row, cells in enumerate((("Method", "Score"), ("GeneratedCell", "001"))):
            for column, cell in enumerate(cells):
                page.insert_text((60 + column * 200, 173 + row * 35), cell)
        document.save(pdf)
    stores = Stores()
    worker = PaperIndexer(tmp_path, collection=stores, embedder=lambda _: [1.0])
    assert worker.index_paper(pdf).status == "indexed"
    artifacts = converter.inspect_artifacts(pdf)
    assert len(artifacts.pages) == 2 and len(artifacts.pages[1].tables) == 1
    write = stores.writes[-1][1]
    assert all(meta["page"] == 2 for meta in write["metadatas"])
    assert sum("GeneratedCell" in text for text in write["documents"]) == 1
    assert any("Table 1, row 2 (page 2)" in text for text in write["documents"])


@pytest.mark.parametrize("header_only", [False, True])
def test_csv_rows_keep_existing_byte_bounds_batching_and_stable_ids(
    fixture, header_only
):
    pdf, stores = fixture
    artifacts = converter.ensure_artifacts(pdf)
    table = artifacts.pages[1].tables[0].csv_path
    with table.open("w", newline="", encoding="utf-8") as stream:
        csv.writer(stream).writerows(
            [["Header"]] if header_only else [["Header"], ["é" * 500]]
        )
    batches = []

    def embed(texts):
        assert len(texts) <= 2
        assert sum(len(text.encode("utf-8")) for text in texts) <= 256
        batches.append(list(texts))
        return [[1.0] for _ in texts]

    worker = PaperIndexer(
        pdf.parent,
        collection=stores,
        batch_embedder=embed,
        max_chunk_bytes=128,
        max_embedding_batch_inputs=2,
        max_embedding_batch_bytes=256,
    )
    worker.index_paper(pdf)
    first = stores.writes[-1][1]
    worker.index_paper(pdf)
    second = stores.writes[-1][1]
    assert first == second
    assert all(len(text.encode("utf-8")) <= 128 for text in first["documents"])
    assert all(meta["page"] == 2 for meta in first["metadatas"])
    if header_only:
        assert any("Table 1, row 1 (page 2)" in text for text in first["documents"])
    else:
        assert len(batches) > 2


@pytest.mark.parametrize("empty", [False, True])
def test_real_text_only_or_empty_pdf_needs_no_csv(tmp_path, empty):
    pymupdf = pytest.importorskip("pymupdf")
    pytest.importorskip("pymupdf4llm")
    pdf = tmp_path / "paper.pdf"
    with pymupdf.open() as document:
        page = document.new_page()
        if not empty:
            page.insert_text((50, 50), "Text without tables", fontsize=16)
        document.save(pdf)
    stores = Stores()
    worker = (
        protected_indexer(pdf, stores)
        if empty
        else PaperIndexer(tmp_path, collection=stores, embedder=lambda _: [1.0])
    )
    outcome = worker.index_paper(pdf)
    assert outcome.status == ("empty" if empty else "indexed")
    assert converter.inspect_artifacts(pdf).missing == ()
    assert set(snapshot(tmp_path)) == {"paper.pdf", "paper.md"}
    if empty:
        assert stores.writes == [
            ("remove_paper", "paper"),
            ("delete", {"paper_id": "paper"}),
        ]
        assert stores.records == {} and stores.papers == {}
    else:
        write = stores.writes[-1][1]
        assert all(meta["page"] == 1 for meta in write["metadatas"])
        before = snapshot(tmp_path)
        assert protected_indexer(pdf, Stores()).index_directory({"paper"}).outcomes == (
            PaperIndexOutcome("paper", "paper.pdf", "skipped"),
        )
        assert snapshot(tmp_path) == before
