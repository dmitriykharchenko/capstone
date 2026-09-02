"""Deterministic PDF paper ingestion into the paper Chroma collection."""

from __future__ import annotations

import re
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterator, Literal

from .chroma_persistence import (
    DEFAULT_COLLECTION_NAME,
    delete_records,
    open_collection,
    upsert_records,
)
from .chunking import split_into_paragraphs
from .embeddings import EmbeddingBackend, EmbeddingConfig, create_embedding

DEFAULT_PAPERS_DIRECTORY = Path("ResearchPapers")
DEFAULT_CHROMA_DIRECTORY = Path("data/chroma")
_SAFE_PAPER_ID = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")


class PDFExtractionError(RuntimeError):
    """Raised when a PDF cannot be opened or a page cannot be extracted."""


@dataclass(frozen=True)
class ExtractedPage:
    """Text extracted from one one-based PDF page."""

    page_number: int
    text: str


@dataclass(frozen=True)
class PaperIndexOutcome:
    """Result of attempting to index one paper."""

    paper_id: str
    filename: str
    status: Literal["indexed", "empty", "failed"]
    chunk_count: int = 0
    error: str | None = None


@dataclass(frozen=True)
class PaperIndexReport:
    """Deterministically ordered outcomes for one directory scan."""

    papers_directory: Path
    outcomes: tuple[PaperIndexOutcome, ...]


@dataclass(frozen=True)
class PaperIndexProgress:
    """One determinate directory-indexing progress update.

    The first update has ``completed == 0`` and no outcome. Each subsequent
    update contains the outcome for exactly one paper.
    """

    completed: int
    total: int
    outcome: PaperIndexOutcome | None = None


def _pdf_reader_factory(path: Path) -> Any:
    try:
        from pypdf import PdfReader
    except ImportError as exc:
        raise PDFExtractionError(
            "pypdf is required for PDF extraction; install the declared dependency"
        ) from exc
    return PdfReader(path)


def extract_pdf_pages(
    pdf_path: str | Path,
    *,
    reader_factory: Callable[[Path], Any] | None = None,
) -> list[ExtractedPage]:
    """Extract readable text in deterministic page order using pypdf.

    ``reader_factory`` is an offline-test seam compatible with ``pypdf.PdfReader``.
    Pages with no extractable text are represented by an empty string.
    """

    if not isinstance(pdf_path, (str, Path)):
        raise TypeError("pdf_path must be a string or Path")
    path = Path(pdf_path)
    if not path.is_file():
        raise PDFExtractionError(f"PDF file does not exist: {path}")
    if path.suffix.lower() != ".pdf":
        raise PDFExtractionError(f"file is not a PDF: {path.name}")
    factory = reader_factory or _pdf_reader_factory
    try:
        reader = factory(path)
        pages = list(reader.pages)
    except PDFExtractionError:
        raise
    except Exception as exc:
        raise PDFExtractionError(f"unable to read PDF {path.name}: {exc}") from exc

    extracted: list[ExtractedPage] = []
    for page_number, page in enumerate(pages, start=1):
        try:
            text = page.extract_text()
        except Exception as exc:
            raise PDFExtractionError(
                f"unable to extract page {page_number} from {path.name}: {exc}"
            ) from exc
        if text is not None and not isinstance(text, str):
            raise PDFExtractionError(
                f"page {page_number} from {path.name} returned non-text content"
            )
        extracted.append(ExtractedPage(page_number, text or ""))
    return extracted


def _paper_id(path: Path) -> str:
    paper_id = path.stem
    if not _SAFE_PAPER_ID.fullmatch(paper_id):
        raise ValueError(
            f"unsafe PDF filename stem {paper_id!r}; use letters, numbers, '.', '_', or '-'"
        )
    return paper_id


class PaperIndexer:
    """Extract, chunk, embed, and replace papers in a Chroma collection."""

    def __init__(
        self,
        papers_directory: str | Path = DEFAULT_PAPERS_DIRECTORY,
        *,
        persist_directory: str | Path = DEFAULT_CHROMA_DIRECTORY,
        collection_name: str = DEFAULT_COLLECTION_NAME,
        client: Any | None = None,
        collection: Any | None = None,
        extractor: Callable[[Path], Sequence[ExtractedPage]] = extract_pdf_pages,
        chunker: Callable[[str], Sequence[str]] = split_into_paragraphs,
        embedder: Callable[[str], Sequence[float]] | None = None,
        embedding_config: EmbeddingConfig | None = None,
        embedding_backend: EmbeddingBackend | Any | None = None,
    ) -> None:
        if not isinstance(papers_directory, (str, Path)):
            raise TypeError("papers_directory must be a string or Path")
        if not callable(extractor) or not callable(chunker):
            raise TypeError("extractor and chunker must be callable")
        if embedder is not None and not callable(embedder):
            raise TypeError("embedder must be callable")
        self.papers_directory = Path(papers_directory)
        self.collection = (
            collection
            if collection is not None
            else open_collection(persist_directory, collection_name, client=client)
        )
        self._extractor = extractor
        self._chunker = chunker
        self._embedding_config = embedding_config
        self._embedding_backend = embedding_backend
        self._embedder = embedder or self._create_embedding

    def _create_embedding(self, text: str) -> Sequence[float]:
        return create_embedding(
            text,
            config=self._embedding_config,
            backend=self._embedding_backend,
        )

    def _resolve_pdf(self, pdf_path: str | Path) -> Path:
        if not isinstance(pdf_path, (str, Path)):
            raise TypeError("pdf_path must be a string or Path")
        path = Path(pdf_path)
        if not path.is_absolute() and path.parent == Path("."):
            path = self.papers_directory / path
        if path.parent.resolve() != self.papers_directory.resolve():
            raise ValueError("PDF must be a top-level file in papers_directory")
        if not path.is_file() or path.suffix.lower() != ".pdf":
            raise ValueError(f"not a readable PDF file: {path}")
        return path

    def index_paper(self, pdf_path: str | Path) -> PaperIndexOutcome:
        """Replace all stored chunks for one top-level PDF."""

        path = self._resolve_pdf(pdf_path)
        paper_id = _paper_id(path)
        chunks: list[str] = []
        page_numbers: list[int] = []
        for page in self._extractor(path):
            if not isinstance(page, ExtractedPage):
                raise TypeError("extractor must return ExtractedPage values")
            if (
                isinstance(page.page_number, bool)
                or not isinstance(page.page_number, int)
                or page.page_number < 1
                or not isinstance(page.text, str)
            ):
                raise ValueError("extracted pages require a positive page number and text")
            page_chunks = list(self._chunker(page.text))
            if any(not isinstance(chunk, str) or not chunk.strip() for chunk in page_chunks):
                raise ValueError("chunker must return non-empty strings")
            chunks.extend(page_chunks)
            page_numbers.extend([page.page_number] * len(page_chunks))

        embeddings = [list(self._embedder(chunk)) for chunk in chunks]
        delete_records(self.collection, where={"paper_id": paper_id})
        if not chunks:
            return PaperIndexOutcome(paper_id, path.name, "empty")

        ids = [f"{paper_id}:chunk:{index:06d}" for index in range(len(chunks))]
        title = re.sub(r"[_-]+", " ", paper_id).strip()
        metadatas = [
            {
                "paper_id": paper_id,
                "chunk_index": index,
                "filename": path.name,
                "title": title,
                "page": page_numbers[index],
            }
            for index in range(len(chunks))
        ]
        upsert_records(self.collection, ids, chunks, embeddings, metadatas)
        return PaperIndexOutcome(paper_id, path.name, "indexed", len(chunks))

    def iter_directory(self) -> Iterator[PaperIndexProgress]:
        """Yield a start update then one outcome per top-level PDF."""

        directory = self.papers_directory
        if not directory.is_dir():
            raise NotADirectoryError(f"papers directory does not exist: {directory}")
        pdfs = sorted(
            (
                path
                for path in directory.iterdir()
                if path.is_file() and path.suffix.lower() == ".pdf"
            ),
            key=lambda path: (path.name.casefold(), path.name),
        )
        total = len(pdfs)
        yield PaperIndexProgress(0, total)
        for completed, path in enumerate(pdfs, start=1):
            try:
                outcome = self.index_paper(path)
            except Exception as exc:
                outcome = PaperIndexOutcome(
                    path.stem,
                    path.name,
                    "failed",
                    error=f"{type(exc).__name__}: {exc}",
                )
            yield PaperIndexProgress(completed, total, outcome)

    def index_directory(self) -> PaperIndexReport:
        """Index top-level PDFs in filename order, isolating paper failures."""

        outcomes: list[PaperIndexOutcome] = []
        for progress in self.iter_directory():
            if progress.outcome is not None:
                outcomes.append(progress.outcome)
        return PaperIndexReport(self.papers_directory, tuple(outcomes))


def index_papers_directory(
    papers_directory: str | Path = DEFAULT_PAPERS_DIRECTORY,
    **indexer_options: Any,
) -> PaperIndexReport:
    """Create a :class:`PaperIndexer` and index its configured directory."""

    return PaperIndexer(papers_directory, **indexer_options).index_directory()


__all__ = [
    "DEFAULT_CHROMA_DIRECTORY",
    "DEFAULT_PAPERS_DIRECTORY",
    "ExtractedPage",
    "PDFExtractionError",
    "PaperIndexOutcome",
    "PaperIndexProgress",
    "PaperIndexReport",
    "PaperIndexer",
    "extract_pdf_pages",
    "index_papers_directory",
]