from pathlib import Path
from types import SimpleNamespace

import pytest

from src.indexing.paper_indexer import (
    PDFExtractionError,
    extract_pdf_pages,
)


def touch_pdf(directory: Path, name: str) -> Path:
    path = directory / name
    path.write_bytes(b"%PDF-1.4\n")
    return path


def test_extract_pdf_pages_reports_open_and_page_failures(tmp_path):
    pdf = touch_pdf(tmp_path, "paper.pdf")

    def bad_reader(path):
        raise ValueError("broken xref")

    with pytest.raises(PDFExtractionError, match="unable to read PDF paper.pdf: broken xref"):
        extract_pdf_pages(pdf, reader_factory=bad_reader)

    page = SimpleNamespace(extract_text=lambda: (_ for _ in ()).throw(ValueError("encrypted")))
    with pytest.raises(PDFExtractionError, match="extract page 1.*encrypted"):
        extract_pdf_pages(pdf, reader_factory=lambda path: SimpleNamespace(pages=[page]))
