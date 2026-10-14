"""Deterministic PDF paper ingestion into the paper Chroma collection."""

from __future__ import annotations

import csv
import hashlib
import json
import math
import queue
import re
import threading
from collections.abc import Callable, Sequence
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from numbers import Real
from pathlib import Path
from typing import Any, Iterator, Literal

from ..storage.chroma_persistence import (
    DEFAULT_COLLECTION_NAME,
    delete_records,
    get_records,
    open_collection,
    open_facts_collection,
    upsert_fact_records,
    upsert_records,
)
from .chunking import (
    DEFAULT_MAX_CHUNK_BYTES,
    split_into_paragraphs,
    split_oversized_chunk,
)
from ..retrieval.embeddings import (
    EmbeddingBackend,
    EmbeddingConfig,
    EmbeddingError,
    create_embeddings,
)
from .fact_extraction import FACT_EXTRACTION_VERSION, Fact, FactOutputError
from ..storage.graph_store import ChunkNode
from .paper_metadata import PaperMetadata, PaperMetadataExtractor
from .pdf_to_markdown import (
    ConversionArtifacts,
    ConversionError,
    MarkdownTable,
    ensure_artifacts,
)

DEFAULT_PAPERS_DIRECTORY = Path("ResearchPapers")
DEFAULT_CHROMA_DIRECTORY = Path("data/chroma")
DEFAULT_MAX_WORKERS = 4
DEFAULT_MAX_EMBEDDING_BATCH_INPUTS = 64
# Conservative margin below the provider's 300,000-token aggregate limit,
# without assuming a fixed characters-per-token ratio or adding a tokenizer.
DEFAULT_MAX_EMBEDDING_BATCH_BYTES = 240_000
PAPER_METADATA_VERSION = 1
FACT_ENRICHMENT_WARNING = (
    "Fact enrichment unavailable: invalid model output after retry."
)
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
    status: Literal["indexed", "empty", "failed", "skipped"]
    chunk_count: int = 0
    error: str | None = None


@dataclass(frozen=True)
class PaperIndexReport:
    """Deterministically ordered outcomes for one directory scan."""

    papers_directory: Path
    outcomes: tuple[PaperIndexOutcome, ...]


@dataclass(frozen=True)
class PaperStart:
    """Marks the paper the indexer is about to process next."""

    paper_id: str
    filename: str


@dataclass(frozen=True)
class PaperIndexProgress:
    """One determinate directory-indexing progress update.

    The first update has ``completed == 0`` with no ``started`` and no
    ``outcome``. Each paper then produces a ``started`` update (before it is
    processed) followed by an ``outcome`` update (once it finishes).
    """

    completed: int
    total: int
    started: PaperStart | None = None
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


def _ensure_pdf_artifacts(path: Path) -> ConversionArtifacts:
    try:
        return ensure_artifacts(path)
    except ConversionError as exc:
        raise PDFExtractionError(str(exc)) from None


def _csv_table_text(table: MarkdownTable, page_number: int) -> str:
    try:
        with table.csv_path.open(encoding="utf-8", newline="") as stream:
            rows = list(csv.reader(stream, strict=True))
    except (OSError, UnicodeError, csv.Error):
        raise PDFExtractionError(
            "Cannot read CSV table; check UTF-8 encoding, CSV quoting, and file permissions."
        ) from None
    if (
        not rows
        or not rows[0]
        or any(len(row) != len(rows[0]) for row in rows)
        or any("\x00" in cell for row in rows for cell in row)
    ):
        raise PDFExtractionError(
            "Invalid CSV table; provide a nonempty rectangular grid without NUL characters."
        )
    # Arrays retain duplicate/empty headers and numeric-looking strings. JSON
    # escaping keeps embedded cell newlines inside a single paragraph/row.
    records = enumerate(rows[1:], 2) if len(rows) > 1 else [(1, rows[0])]
    return "\n\n".join(
        f"Table {table.table_number}, row {number} (page {page_number}): "
        + json.dumps(
            {"columns": rows[0], "values": row},
            ensure_ascii=False,
            separators=(",", ":"),
        )
        for number, row in records
    )


def extract_artifact_pages(pdf_path: str | Path) -> list[ExtractedPage]:
    """Ensure adjacent artifacts, then read prose and CSV rows in PDF page order.

    Marked Markdown tables are replaced with CSV evidence, never indexed twice.
    All tables are parsed before returning, so malformed CSV cannot cause partial
    embedding or store mutation. No raw-PDF extraction fallback is used.
    """
    artifacts = _ensure_pdf_artifacts(Path(pdf_path))
    pages = []
    for page in artifacts.pages:
        text = page.text
        for table in page.tables:
            text = text.replace(
                table.markdown,
                "\n\n" + _csv_table_text(table, page.page_number) + "\n\n",
                1,
            )
        pages.append(ExtractedPage(page.page_number, text))
    return pages


def _paper_id(path: Path) -> str:
    paper_id = path.stem
    if not _SAFE_PAPER_ID.fullmatch(paper_id):
        raise ValueError(
            f"unsafe PDF filename stem {paper_id!r}; use letters, numbers, '.', '_', or '-'"
        )
    return paper_id


class PaperIndexer:
    """Extract, chunk, embed, and replace papers in a Chroma collection.

    Batch limits must be positive integers, and the byte budget must accommodate
    ``max_chunk_bytes`` so every bounded chunk fits without changing chunking.
    """

    def __init__(
        self,
        papers_directory: str | Path = DEFAULT_PAPERS_DIRECTORY,
        *,
        persist_directory: str | Path = DEFAULT_CHROMA_DIRECTORY,
        collection_name: str = DEFAULT_COLLECTION_NAME,
        client: Any | None = None,
        collection: Any | None = None,
        extractor: Callable[[Path], Sequence[ExtractedPage]] = extract_artifact_pages,
        chunker: Callable[[str], Sequence[str]] = split_into_paragraphs,
        embedder: Callable[[str], Sequence[float]] | None = None,
        batch_embedder: Callable[[Sequence[str]], Sequence[Sequence[float]]]
        | None = None,
        embedding_config: EmbeddingConfig | None = None,
        embedding_backend: EmbeddingBackend | Any | None = None,
        metadata_extractor: Callable[[str], PaperMetadata] | None = None,
        fact_extractor: Callable[[Sequence[str]], Sequence[Sequence[Fact]]]
        | None = None,
        graph_store: Any | None = None,
        facts_collection: Any | None = None,
        max_chunk_bytes: int = DEFAULT_MAX_CHUNK_BYTES,
        max_workers: int = DEFAULT_MAX_WORKERS,
        max_embedding_batch_inputs: int = DEFAULT_MAX_EMBEDDING_BATCH_INPUTS,
        max_embedding_batch_bytes: int = DEFAULT_MAX_EMBEDDING_BATCH_BYTES,
    ) -> None:
        if not isinstance(papers_directory, (str, Path)):
            raise TypeError("papers_directory must be a string or Path")
        if not callable(extractor) or not callable(chunker):
            raise TypeError("extractor and chunker must be callable")
        if embedder is not None and not callable(embedder):
            raise TypeError("embedder must be callable")
        if batch_embedder is not None and not callable(batch_embedder):
            raise TypeError("batch_embedder must be callable")
        if metadata_extractor is not None and not callable(metadata_extractor):
            raise TypeError("metadata_extractor must be callable")
        if fact_extractor is not None and not callable(fact_extractor):
            raise TypeError("fact_extractor must be callable")
        if graph_store is not None and (
            not callable(getattr(graph_store, "replace_paper", None))
            or not callable(getattr(graph_store, "remove_paper", None))
        ):
            raise TypeError("graph_store must provide replace_paper and remove_paper")
        if fact_extractor is not None:
            if graph_store is None:
                raise ValueError("fact_extractor requires graph_store")
            required = (
                "replace_paper_facts",
                "fact_version_for_paper",
                "fact_ids_for_paper",
                "chunks_for_facts",
            )
            if any(not callable(getattr(graph_store, name, None)) for name in required):
                raise TypeError("graph_store does not provide the fact graph API")
        if isinstance(max_chunk_bytes, bool) or not isinstance(max_chunk_bytes, int):
            raise TypeError("max_chunk_bytes must be an integer")
        if max_chunk_bytes < 1:
            raise ValueError("max_chunk_bytes must be positive")
        if isinstance(max_workers, bool) or not isinstance(max_workers, int):
            raise TypeError("max_workers must be an integer")
        if max_workers < 1:
            raise ValueError("max_workers must be at least 1")
        for name, value in (
            ("max_embedding_batch_inputs", max_embedding_batch_inputs),
            ("max_embedding_batch_bytes", max_embedding_batch_bytes),
        ):
            if isinstance(value, bool) or not isinstance(value, int):
                raise TypeError(f"{name} must be an integer")
            if value < 1:
                raise ValueError(f"{name} must be positive")
        if max_embedding_batch_bytes < max_chunk_bytes:
            raise ValueError(
                "max_embedding_batch_bytes must be at least max_chunk_bytes"
            )
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
        self._embedder = embedder
        self._batch_embedder = batch_embedder
        self._metadata_extractor = (
            metadata_extractor
            if metadata_extractor is not None
            else PaperMetadataExtractor()
            if graph_store is not None
            else None
        )
        self._fact_extractor = fact_extractor
        self._graph_store = graph_store
        self.facts_collection = None
        if fact_extractor is not None:
            if facts_collection is not None:
                self.facts_collection = facts_collection
            elif collection is not None and client is None:
                raise ValueError(
                    "facts_collection is required with an injected documents collection"
                )
            else:
                self.facts_collection = open_facts_collection(
                    persist_directory, client=client
                )
        self._max_chunk_bytes = max_chunk_bytes
        self._max_workers = max_workers
        self._max_embedding_batch_inputs = max_embedding_batch_inputs
        self._max_embedding_batch_bytes = max_embedding_batch_bytes
        # Serializes coordinated graph/Chroma mutations while extraction,
        # metadata generation, and embedding remain parallel per paper.
        self._write_lock = threading.Lock()

    def _embed_chunks(self, chunks: Sequence[str]) -> list[list[float]]:
        """Embed bounded batches in order, finishing before any store mutation.

        An injected single-text ``embedder`` keeps the per-chunk path (used by
        existing callers and tests); an injected ``batch_embedder`` or the
        default backend is bounded by input count and aggregate UTF-8 bytes.
        """

        if self._batch_embedder is None and self._embedder is not None:
            return [list(self._embedder(chunk)) for chunk in chunks]

        embeddings: list[list[float]] = []
        for batch in self._embedding_batches(chunks):
            vectors = (
                self._batch_embedder(batch)
                if self._batch_embedder is not None
                else create_embeddings(
                    batch,
                    config=self._embedding_config,
                    backend=self._embedding_backend,
                )
            )
            materialized = [list(vector) for vector in vectors]
            if len(materialized) != len(batch):
                raise EmbeddingError(
                    "Embedding backend returned a different number of vectors than texts "
                    f"(got {len(materialized)}, expected {len(batch)})"
                )
            for vector in materialized:
                if not vector or any(
                    isinstance(value, bool)
                    or not isinstance(value, Real)
                    or not math.isfinite(value)
                    for value in vector
                ):
                    raise EmbeddingError("Embedding backend returned an invalid vector")
                if embeddings and len(vector) != len(embeddings[0]):
                    raise EmbeddingError(
                        "All embeddings must have the same vector dimension"
                    )
                embeddings.append(vector)
        return embeddings

    def _embedding_batches(self, chunks: Sequence[str]) -> Iterator[list[str]]:
        """Greedily partition already-bounded chunks without changing their text."""

        batch: list[str] = []
        batch_bytes = 0
        for chunk in chunks:
            chunk_bytes = len(chunk.encode("utf-8"))
            if batch and (
                len(batch) >= self._max_embedding_batch_inputs
                or batch_bytes + chunk_bytes > self._max_embedding_batch_bytes
            ):
                yield batch
                batch = []
                batch_bytes = 0
            batch.append(chunk)
            batch_bytes += chunk_bytes
        if batch:
            yield batch

    def _extract_facts(self, chunks: Sequence[str]) -> tuple[tuple[Fact, ...], ...]:
        if self._fact_extractor is None:
            return tuple(() for _ in chunks)
        extracted = self._fact_extractor(chunks)
        if isinstance(extracted, (str, bytes)) or not isinstance(extracted, Sequence):
            raise TypeError("fact_extractor must return one fact sequence per chunk")
        if len(extracted) != len(chunks):
            raise ValueError("fact_extractor returned a different number of chunk results")
        result: list[tuple[Fact, ...]] = []
        for facts in extracted:
            if isinstance(facts, (str, bytes)) or not isinstance(facts, Sequence):
                raise TypeError("fact_extractor chunk results must be sequences")
            if any(not isinstance(fact, Fact) for fact in facts):
                raise TypeError("fact_extractor must return only Fact values")
            result.append(tuple({fact.fact_id: fact for fact in facts}.values()))
        return tuple(result)

    @staticmethod
    def _unique_facts(facts_by_chunk: Sequence[Sequence[Fact]]) -> tuple[Fact, ...]:
        unique: dict[str, Fact] = {}
        for facts in facts_by_chunk:
            for fact in facts:
                unique.setdefault(fact.fact_id, fact)
        return tuple(unique.values())

    def _orphaned_fact_ids(self, previous_fact_ids: Sequence[str]) -> tuple[str, ...]:
        if not previous_fact_ids or self._graph_store is None:
            return ()
        connected = self._graph_store.chunks_for_facts(previous_fact_ids)
        return tuple(
            fact_id for fact_id in previous_fact_ids if not connected.get(fact_id, ())
        )

    def _write_fact_vectors(
        self,
        facts: Sequence[Fact],
        embeddings: Sequence[Sequence[float]],
        orphaned_fact_ids: Sequence[str],
    ) -> None:
        if self.facts_collection is None:
            return
        if facts:
            upsert_fact_records(
                self.facts_collection,
                [fact.fact_id for fact in facts],
                [fact.statement for fact in facts],
                embeddings,
                [
                    {
                        "fact_type": fact.fact_type,
                        "value": fact.value,
                        **({"unit": fact.unit} if fact.unit is not None else {}),
                    }
                    for fact in facts
                ],
            )
        if orphaned_fact_ids:
            delete_records(self.facts_collection, ids=orphaned_fact_ids)

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

    def _extract_pages(self, path: Path) -> list[ExtractedPage]:
        pages = list(self._extractor(path))
        for page in pages:
            if not isinstance(page, ExtractedPage):
                raise TypeError("extractor must return ExtractedPage values")
            if (
                isinstance(page.page_number, bool)
                or not isinstance(page.page_number, int)
                or page.page_number < 1
                or not isinstance(page.text, str)
            ):
                raise ValueError(
                    "extracted pages require a positive page number and text"
                )
        return pages

    @staticmethod
    def _content_fingerprint(pages: Sequence[ExtractedPage]) -> str:
        payload = json.dumps(
            [[page.page_number, page.text] for page in pages],
            ensure_ascii=False,
            separators=(",", ":"),
        )
        return hashlib.sha256(payload.encode("utf-8")).hexdigest()

    def index_paper(self, pdf_path: str | Path) -> PaperIndexOutcome:
        """Replace all stored chunks for one top-level PDF."""

        path = self._resolve_pdf(pdf_path)
        paper_id = _paper_id(path)
        pages = self._extract_pages(path)
        chunks: list[str] = []
        page_numbers: list[int] = []
        for page in pages:
            page_chunks = list(self._chunker(page.text))
            if any(
                not isinstance(chunk, str) or not chunk.strip() for chunk in page_chunks
            ):
                raise ValueError("chunker must return non-empty strings")
            bounded_chunks = [
                bounded
                for chunk in page_chunks
                for bounded in split_oversized_chunk(
                    chunk, max_bytes=self._max_chunk_bytes
                )
            ]
            chunks.extend(bounded_chunks)
            page_numbers.extend([page.page_number] * len(bounded_chunks))

        if not chunks:
            with self._write_lock:
                previous_fact_ids = (
                    self._graph_store.fact_ids_for_paper(paper_id)
                    if self._fact_extractor is not None
                    else ()
                )
                if self._graph_store is not None:
                    self._graph_store.remove_paper(paper_id)
                delete_records(self.collection, where={"paper_id": paper_id})
                self._write_fact_vectors(
                    (), (), self._orphaned_fact_ids(previous_fact_ids)
                )
            return PaperIndexOutcome(paper_id, path.name, "empty")

        metadata = None
        if self._metadata_extractor is not None:
            metadata = self._metadata_extractor(
                "\n\n".join(page.text for page in pages)
            )
            if not isinstance(metadata, PaperMetadata):
                raise TypeError("metadata_extractor must return PaperMetadata")
        ids = [f"{paper_id}:chunk:{index:06d}" for index in range(len(chunks))]
        fact_warning = None
        facts_by_position = None
        try:
            facts_by_position = self._extract_facts(chunks)
        except FactOutputError:
            fact_warning = FACT_ENRICHMENT_WARNING
        unique_facts = (
            self._unique_facts(facts_by_position)
            if facts_by_position is not None
            else ()
        )
        embeddings = self._embed_chunks(chunks)
        fact_embeddings = self._embed_chunks(
            [fact.statement for fact in unique_facts]
        )
        title = (
            metadata.title
            if metadata is not None
            else re.sub(r"[_-]+", " ", paper_id).strip()
        )
        enrichment: dict[str, str | int] = {}
        if metadata is not None:
            enrichment = {
                "authors": json.dumps(
                    metadata.authors, ensure_ascii=False, separators=(",", ":")
                ),
                "topics": json.dumps(
                    metadata.topics, ensure_ascii=False, separators=(",", ":")
                ),
                "content_fingerprint": self._content_fingerprint(pages),
                "metadata_version": PAPER_METADATA_VERSION,
            }
        metadatas = [
            {
                "paper_id": paper_id,
                "chunk_index": index,
                "filename": path.name,
                "title": title,
                "page": page_numbers[index],
                **enrichment,
            }
            for index in range(len(chunks))
        ]
        # Replace this paper's records atomically under the write lock so a
        # concurrent worker never interleaves its own delete/upsert here.
        with self._write_lock:
            previous_fact_ids = (
                self._graph_store.fact_ids_for_paper(paper_id)
                if self._fact_extractor is not None
                else ()
            )
            if self._graph_store is not None:
                if metadata is None:  # pragma: no cover - constructor establishes it
                    raise RuntimeError("graph metadata extraction is unavailable")
                graph_options: dict[str, Any] = {}
                if self._fact_extractor is not None and facts_by_position is not None:
                    nodes = tuple(
                        ChunkNode(ids[index], paper_id, index, page_numbers[index])
                        for index in range(len(ids))
                    )
                    graph_options = {
                        "chunks": nodes,
                        "facts_by_chunk": {
                            ids[index]: facts_by_position[index]
                            for index in range(len(ids))
                        },
                        "fact_version": FACT_EXTRACTION_VERSION,
                    }
                elif self._fact_extractor is not None:
                    graph_options = {"clear_facts": True}
                self._graph_store.replace_paper(
                    paper_id,
                    metadata.title,
                    metadata.authors,
                    metadata.topics,
                    **graph_options,
                )
            delete_records(self.collection, where={"paper_id": paper_id})
            upsert_records(self.collection, ids, chunks, embeddings, metadatas)
            self._write_fact_vectors(
                unique_facts,
                fact_embeddings,
                self._orphaned_fact_ids(previous_fact_ids),
            )
        return PaperIndexOutcome(
            paper_id,
            path.name,
            "indexed",
            len(chunks),
            error=fact_warning,
        )

    def iter_directory(
        self, skip_paper_ids: set[str] | frozenset[str] | None = None
    ) -> Iterator[PaperIndexProgress]:
        """Yield a start update then one ``started``/``outcome`` pair per PDF.

        The default extractor backfills adjacent artifacts for skipped papers
        without changing document vectors. Successful fact backfills and
        malformed-output warnings use an indexed outcome so persisted status can
        be refreshed; unrelated backfill errors remain failed outcomes.

        With ``max_workers > 1`` several papers are indexed concurrently, so
        ``started``/``outcome`` events for different papers may interleave and
        arrive out of filename order; the terminal ``completed`` count is still
        monotonic and reaches ``total``. Every event is keyed by ``paper_id`` so
        downstream consumers stay correct regardless of arrival order.
        """

        skip = skip_paper_ids or frozenset()
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
        if self._max_workers <= 1:
            yield from self._iter_sequential(pdfs, skip, total)
        else:
            yield from self._iter_concurrent(pdfs, skip, total)

    def _iter_sequential(
        self,
        pdfs: Sequence[Path],
        skip: frozenset[str] | set[str],
        total: int,
    ) -> Iterator[PaperIndexProgress]:
        """Index papers one at a time in filename order (``max_workers == 1``)."""

        for completed, path in enumerate(pdfs, start=1):
            if path.stem in skip:
                yield PaperIndexProgress(
                    completed,
                    total,
                    outcome=self._backfill_or_failure(path),
                )
                continue
            yield PaperIndexProgress(
                completed - 1,
                total,
                started=PaperStart(path.stem, path.name),
            )
            yield PaperIndexProgress(
                completed, total, outcome=self._index_or_failure(path)
            )

    def _iter_concurrent(
        self,
        pdfs: Sequence[Path],
        skip: frozenset[str] | set[str],
        total: int,
    ) -> Iterator[PaperIndexProgress]:
        """Index papers via a bounded thread pool, yielding events as they land.

        Skipped papers emit their outcomes up front in filename order; the rest
        are submitted to the pool. Each worker pushes a ``started`` event when it
        begins and an ``outcome`` event when it finishes onto a thread-safe
        queue, which this generator drains, advancing a monotonic ``completed``
        count on each outcome.
        """

        completed = 0
        to_process: list[Path] = []
        for path in pdfs:
            if path.stem in skip:
                completed += 1
                yield PaperIndexProgress(
                    completed,
                    total,
                    outcome=self._backfill_or_failure(path),
                )
            else:
                to_process.append(path)
        if not to_process:
            return

        events: queue.Queue[tuple[str, Any]] = queue.Queue()

        def worker(path: Path) -> None:
            events.put(("started", PaperStart(path.stem, path.name)))
            events.put(("outcome", self._index_or_failure(path)))

        workers = min(self._max_workers, len(to_process))
        with ThreadPoolExecutor(max_workers=workers) as executor:
            for path in to_process:
                executor.submit(worker, path)
            remaining = len(to_process)
            while remaining:
                kind, payload = events.get()
                if kind == "started":
                    yield PaperIndexProgress(completed, total, started=payload)
                else:
                    completed += 1
                    remaining -= 1
                    yield PaperIndexProgress(completed, total, outcome=payload)

    def _backfill_skipped(self, path: Path) -> PaperIndexOutcome:
        if self._extractor is extract_artifact_pages:
            _ensure_pdf_artifacts(path)
        if self._fact_extractor is not None:
            if (
                self._graph_store.fact_version_for_paper(path.stem)
                != FACT_EXTRACTION_VERSION
            ):
                chunk_count, facts_available = self._backfill_facts(path.stem)
                return PaperIndexOutcome(
                    path.stem,
                    path.name,
                    "indexed",
                    chunk_count,
                    error=None if facts_available else FACT_ENRICHMENT_WARNING,
                )
        return PaperIndexOutcome(path.stem, path.name, "skipped")

    def _backfill_facts(self, paper_id: str) -> tuple[int, bool]:
        records = get_records(self.collection, where={"paper_id": paper_id})
        ids = records.get("ids")
        documents = records.get("documents")
        metadatas = records.get("metadatas")
        if not isinstance(ids, list) or not isinstance(documents, list) or not isinstance(
            metadatas, list
        ):
            raise ValueError("stored chunks are missing ids, documents, or metadatas")
        if not ids or not len(ids) == len(documents) == len(metadatas):
            raise ValueError("stored chunks are empty or inconsistent")
        stored = []
        for chunk_id, document, metadata in zip(ids, documents, metadatas, strict=True):
            if (
                not isinstance(chunk_id, str)
                or not isinstance(document, str)
                or not isinstance(metadata, dict)
                or metadata.get("paper_id") != paper_id
            ):
                raise ValueError("stored chunk provenance is invalid")
            stored.append(
                (
                    metadata.get("chunk_index"),
                    chunk_id,
                    document,
                    metadata.get("page"),
                )
            )
        if any(
            isinstance(index, bool)
            or not isinstance(index, int)
            or index < 0
            or isinstance(page, bool)
            or not isinstance(page, int)
            or page < 1
            for index, _, _, page in stored
        ):
            raise ValueError("stored chunk indexes or pages are invalid")
        stored.sort(key=lambda item: (item[0], item[1]))
        if len({item[0] for item in stored}) != len(stored):
            raise ValueError("stored chunk indexes must be unique")
        chunk_texts = [item[2] for item in stored]
        try:
            facts_by_position = self._extract_facts(chunk_texts)
        except FactOutputError:
            return len(stored), False
        unique_facts = self._unique_facts(facts_by_position)
        fact_embeddings = self._embed_chunks(
            [fact.statement for fact in unique_facts]
        )
        nodes = tuple(
            ChunkNode(chunk_id, paper_id, chunk_index, page)
            for chunk_index, chunk_id, _, page in stored
        )
        facts_by_chunk = {
            stored[index][1]: facts_by_position[index] for index in range(len(stored))
        }
        with self._write_lock:
            if (
                self._graph_store.fact_version_for_paper(paper_id)
                == FACT_EXTRACTION_VERSION
            ):
                return len(stored), True
            previous_fact_ids = self._graph_store.fact_ids_for_paper(paper_id)
            self._graph_store.replace_paper_facts(
                paper_id, nodes, facts_by_chunk, FACT_EXTRACTION_VERSION
            )
            self._write_fact_vectors(
                unique_facts,
                fact_embeddings,
                self._orphaned_fact_ids(previous_fact_ids),
            )
        return len(stored), True

    def _backfill_or_failure(self, path: Path) -> PaperIndexOutcome:
        try:
            return self._backfill_skipped(path)
        except Exception as exc:  # noqa: BLE001 - isolate one paper's failure
            return PaperIndexOutcome(
                path.stem,
                path.name,
                "failed",
                error=f"{type(exc).__name__}: {exc}",
            )

    def _index_or_failure(self, path: Path) -> PaperIndexOutcome:
        """Index one paper, converting any exception into a ``failed`` outcome."""

        try:
            return self.index_paper(path)
        except Exception as exc:  # noqa: BLE001 - isolate one paper's failure
            return PaperIndexOutcome(
                path.stem,
                path.name,
                "failed",
                error=f"{type(exc).__name__}: {exc}",
            )

    def index_directory(
        self, skip_paper_ids: set[str] | frozenset[str] | None = None
    ) -> PaperIndexReport:
        """Index top-level PDFs, isolating paper failures.

        ``skip_paper_ids`` is forwarded to :meth:`iter_directory` so callers can
        avoid re-indexing papers that are already indexed. Outcomes are sorted
        by filename so the report is deterministically ordered even when papers
        are indexed concurrently and finish out of order.
        """

        outcomes: list[PaperIndexOutcome] = []
        for progress in self.iter_directory(skip_paper_ids):
            if progress.outcome is not None:
                outcomes.append(progress.outcome)
        outcomes.sort(
            key=lambda outcome: (outcome.filename.casefold(), outcome.filename)
        )
        return PaperIndexReport(self.papers_directory, tuple(outcomes))


def index_papers_directory(
    papers_directory: str | Path = DEFAULT_PAPERS_DIRECTORY,
    **indexer_options: Any,
) -> PaperIndexReport:
    """Create a :class:`PaperIndexer` and index its configured directory."""

    return PaperIndexer(papers_directory, **indexer_options).index_directory()


__all__ = [
    "DEFAULT_CHROMA_DIRECTORY",
    "DEFAULT_MAX_EMBEDDING_BATCH_BYTES",
    "DEFAULT_MAX_EMBEDDING_BATCH_INPUTS",
    "DEFAULT_MAX_WORKERS",
    "DEFAULT_PAPERS_DIRECTORY",
    "ExtractedPage",
    "PDFExtractionError",
    "PAPER_METADATA_VERSION",
    "PaperIndexOutcome",
    "PaperIndexProgress",
    "PaperIndexReport",
    "PaperIndexer",
    "PaperStart",
    "extract_artifact_pages",
    "extract_pdf_pages",
    "index_papers_directory",
]
