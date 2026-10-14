# Paper indexing

[Documentation home](README.md) · [Architecture](architecture.md) · [Retrieval and chat](retrieval-and-chat.md) · [Data storage](data-storage.md)

**Source inspection date: 2026-09-25.** This reference describes the current working tree, including uncommitted implementation. It is not a claim about a clean checkout, a running installation, or a verified provider run. Source and test files are inspected only; no indexing, database access, application tests, or provider calls are performed for this guide.

**Fact-index update: 2026-10-02.** Python indexing now extracts typed facts into separate graph/vector records and backfills facts from stored chunks. No live corpus conversion or provider run is claimed.

## Indexing path

Tauri frontend `api.indexPapersStream` → desktop `index_papers_stream` → Python `handle_index_papers_stream`. Persistence comprises Chroma `documents` chunks, Chroma `facts`, a separate Cozo graph, SQLite outcome indicators, and BM25 JSON; these do not share one transaction.

## Python: entrypoints and UI wiring

The [frontend API](../app/src/lib/api.ts) exposes `indexPapers`, `indexPapersStream`, `paperStatuses`, `bm25Status`, and `resetAllIndices`. The [Papers saga](../app/src/slices/papers/sagas/papers-saga.ts) uses the streaming API for progress and hydrates persisted indicators. [Tauri's command registration](../app/src-tauri/src/lib.rs) provides the desktop bridge; [Python commands](../src/cli/commands.py) implement the work.

| Python interface | Meaning |
| --- | --- |
| `PaperIndexer.index_paper(pdf_path)` | Direct replacement of one top-level PDF; raises errors to its caller |
| `PaperIndexer.iter_directory(skip_paper_ids=None)` | Directory scan with progress, isolated per-paper failures, optional skip set |
| `PaperIndexer.index_directory(skip_paper_ids=None)` | Consumes progress and returns `PaperIndexReport` sorted by filename |
| `index_papers_directory(...)` | Convenience construction plus directory indexing |
| `handle_index_papers` | Non-streaming command; reads existing skip statuses and refreshes BM25, but does **not** persist each new outcome |
| `handle_index_papers_stream` | Streaming command; best-effort persists non-skipped outcomes and refreshes BM25 at completion |

The implementation is [paper_indexer.py](../src/indexing/paper_indexer.py). CLI construction is `build_paper_indexer` in [models.py](../src/cli/models.py). It opens `CozoGraphStore`, configures `PaperMetadataExtractor` and `FactExtractor` with the real configured model or deterministic offline mock, opens both Chroma collections, and selects the provider or local embedder.

### Source selection and identity

- The default input directory is `ResearchPapers`, resolved by `CLIConfig` relative to the project root. Overrides are described in [configuration](configuration.md).
- `iter_directory` considers top-level files with a case-insensitive `.pdf` suffix. It sorts by `(filename.casefold(), filename)` before scheduling; it does not recurse.
- `paper_id` is the filename stem, not the title or a content hash. `_paper_id` requires `^[A-Za-z0-9][A-Za-z0-9._-]*$`. A stem containing spaces is rejected when processed.
- `_resolve_pdf` requires the file's parent to resolve to the configured papers directory. It does not pin file identity against later external edits.
- The scan does not remove index records merely because a PDF disappears from the directory. The current file set, the Chroma collection, and saved statuses can therefore diverge.

### Extraction, chunking, and provenance

1. The default `extract_artifact_pages` calls `ensure_artifacts` from [pdf_to_markdown.py](../src/indexing/pdf_to_markdown.py) before any metadata, embedding, graph, or vector writes. It creates missing Markdown, CSV, and image files beside the PDF. Under the Hybrid policy, legacy Markdown without page metadata is regenerated; valid page-aware Markdown and all existing CSV/images remain untouched. Stale or malformed modern metadata requires an explicit refresh rather than fabricated page citations. See [conversion format and safety](pdf-to-markdown.md).
2. Versioned Markdown supplies one-based PDF page numbers, including blank pages. Each marked Markdown table block (including its CSV link) is replaced by evidence read from its declared page-numbered CSV, avoiding duplicate Markdown/CSV table evidence. CSV uses strict UTF-8 parsing with quoted commas, quotes, and multiline cells; empty, ragged, NUL-containing, or malformed tables fail before provider/store calls. Each data row includes table number, logical row number (header is row 1), page, repeated column labels, and JSON-escaped cell values. Duplicate/empty headers and numeric-looking strings are preserved; formulas are text, not executed. Header-only tables emit row 1. Undeclared CSV files are not ingested.
3. Each reconstructed page is independently passed to `split_into_paragraphs` from [chunking.py](../src/indexing/chunking.py). Blank lines delimit nonempty, trimmed paragraphs; escaped cell newlines do not accidentally split CSV rows. There is no model-driven segmentation, overlap window, or cross-page merge in this path.
4. `split_oversized_chunk` bounds each paragraph to `DEFAULT_MAX_CHUNK_BYTES = 8_000` UTF-8 bytes. It prefers fitting sentence ends, then uses character boundaries; it does not split inside a UTF-8 character. Oversized CSV rows use this same splitter, so continuation chunks retain page metadata but need not repeat the row prefix.
5. Optional metadata extraction receives the reconstructed prose and CSV evidence. `PaperMetadataExtractor` applies its own prompt bound and strict title/author/topic parsing; see [knowledge graph](knowledge-graph.md) and [paper_metadata.py](../src/indexing/paper_metadata.py).
6. `FactExtractor` receives bounded batches of chunks as untrusted evidence and strictly validates a closed fact-type schema. Exact normalized type/value identity deduplicates a fact across chunks and papers.
7. Chunk and unique fact-statement embeddings are prepared before normal nonempty-paper store mutation. Chunk IDs are `paper_id:chunk:000000`, `paper_id:chunk:000001`, and so on. `chunk_index` is zero-based across the whole paper, not reset on each page.

Injected `extractor` callables retain their existing behavior and bypass automatic artifact creation/backfill. The legacy `extract_pdf_pages` helper remains available for explicit `pypdf` extraction; it is no longer the default. Artifact parsing/conversion errors become `PDFExtractionError`, without silently falling back to raw PDF text.

| Per-chunk metadata | Meaning |
| --- | --- |
| `paper_id`, `chunk_index` | Stable paper/chunk identity for one indexing layout |
| `filename`, `page` | Source PDF filename and one-based page |
| `title` | Extracted title when metadata extraction is configured; otherwise a stem-derived title |
| `authors`, `topics` | JSON-encoded lists, when enrichment is enabled |
| `content_fingerprint` | SHA-256 of serialized extracted page numbers/text, not raw PDF bytes |
| `metadata_version` | `PAPER_METADATA_VERSION = 1`, when enrichment is enabled |

The enriched fingerprint is stored as provenance; Python's CLI skip decision does **not** compare it with the current PDF. Reusing a filename after replacing its content can leave an old publication marked indexed until explicitly reprocessed.

### Embedding batches

`PaperIndexer._embedding_batches` greedily partitions already-bounded chunks without changing their text or order. Defaults are:

| Constant / option | Default | Purpose |
| --- | --- | --- |
| `DEFAULT_MAX_WORKERS` / `max_workers` | `4` | Concurrent paper preparation workers |
| `DEFAULT_MAX_EMBEDDING_BATCH_INPUTS` | `64` | Maximum input strings per embedding request |
| `DEFAULT_MAX_EMBEDDING_BATCH_BYTES` | `240_000` | Maximum aggregate UTF-8 input bytes per request |
| `max_chunk_bytes` | `8_000` | Maximum bytes per individual chunk |

The byte budget is a conservative request-size guard, not an exact token counter. The constructor rejects nonpositive limits and a batch byte budget smaller than the chunk limit. A caller-injected single-text `embedder` retains a per-chunk path; `batch_embedder` and the default backend use bounded batches.

`_embed_chunks` validates batch vector count, finite numeric values, nonempty vectors, and consistent dimensions across batches. [embeddings.py](../src/retrieval/embeddings.py) provides `create_embedding`, `create_embeddings`, and `OpenAIEmbeddingBackend.embed_batch`; provider results are sorted by their response index. A late batch failure happens before the normal paper replacement starts, so previously stored chunks are not deliberately deleted by that preparation failure.

CLI offline mode uses `make_local_embedder`, a deterministic 256-dimensional signed hashing embedder. It is not a semantic model. Changing between local and provider embeddings, or changing the embedding model/dimension, requires managing the existing index accordingly; the adapter has no explicit embedding-identity fence.

### Publication and failure boundaries

For a nonempty paper, the instance's `_write_lock` encloses:

1. `graph_store.replace_paper(...)`, including chunk/fact relations when configured;
2. `delete_records(collection, where={"paper_id": paper_id})`;
3. `upsert_records(...)` with the prepared chunk vectors and metadata;
4. fact-vector upsert into `facts` and deletion of vectors whose graph facts became orphaned.

The lock prevents workers using this **same indexer instance** from interleaving these writes. It is not a transaction across Cozo and Chroma, a process-wide lock, or crash recovery. A graph write can succeed before a later vector write fails; a delete can succeed before an upsert fails. There is no rollback journal for the Python combination.

An empty extraction removes the paper's graph entry and vector records under the same lock, then returns `status="empty"`. `_index_or_failure` converts per-paper exceptions into `PaperIndexOutcome(status="failed", error=...)`, allowing other papers to finish. The error text can include exception details; do not publish it without review.

### Progress, statuses, and skip behavior

[stream_workflows.py](../src/cli/stream_workflows.py) implements `index_events` using a small transient LangGraph. It validates terminal counts but is not a durable indexing checkpoint.

| Stream event | Fields / significance |
| --- | --- |
| `start` | `completed=0`, `total` scanned PDFs |
| `started` | Paper ID and filename before processing; not evidence of a write |
| `progress` | Monotonic completed count plus `indexed`, `empty`, `failed`, or `skipped` outcome |
| `done` | Summary counts after the iterator finishes and the refresh callback returns |

Concurrent `started`/outcome events can interleave in completion order; use `paper_id`, not array position, to update UI state. A skipped paper still receives artifact inspection. When its graph fact version is stale, the indexer reads its existing Chroma chunks by `paper_id`, orders them by `chunk_index`, extracts/embeds facts, and atomically replaces only fact graph relations before updating `facts`. Chunk records and paper/author/topic graph data are unchanged. A current version skips fact work. A backfill failure retains `status="skipped"` with actionable text so the CLI does not overwrite its saved indexed status.

`_already_indexed_ids` selects only SQLite rows whose status is `indexed`. `empty` and `failed` are retried. Status-store open/read/write failures are intentionally best-effort: indexing can continue without durable indicators. The streaming command leaves skipped rows unchanged. `_refresh_bm25_index` rebuilds from the full Chroma corpus, including skipped papers, and swallows refresh failures. Consequently, `done` is not proof that indicators and BM25 were successfully persisted.

`handle_search_papers` does **not** lazily index. Its implementation returns an empty match list with a “run indexing first” reason when the collection count is zero, despite the handler's stale lazy-indexing docstring.

## Reset boundaries

**Reset is destructive and is not run by this guide.** Inspect [data storage](data-storage.md) before using it.

| Reset implementation | Clears | Preserves / limitations |
| --- | --- | --- |
| Python `handle_reset_all_indices` | Chroma `documents`, `bm25_index.json`, SQLite paper status rows | PDFs, canonical chat history, `chat_exchanges`, Cozo graph, and orchestration state remain; steps are independent/best-effort |

Python returns an `ok` envelope plus per-step flags. The vector step currently treats any `delete_collection` exception as an already-reset collection after successful client construction, so its true flag is not a strong deletion proof. The current Tauri Papers saga awaits the reset call but does not inspect those flags before dispatching `resetIndicesSucceeded`; a successful UI notification is therefore weaker than verified reset of every component.

## Source and test map

These are existing verification seams, not tests executed for this documentation task. For approved commands and runtime safeguards, see [operations and testing](operations-and-testing.md).

| Concern | Source / existing tests |
| --- | --- |
| Paragraph and byte-bounded splits | [chunking.py](../src/indexing/chunking.py) |
| Deterministic identity and failure isolation | [test_paper_indexer.py](../tests/test_paper_indexer.py) |
| Adjacent Markdown/CSV ingestion and skipped-paper backfill | [test_paper_indexer_artifacts.py](../tests/test_paper_indexer_artifacts.py), [test_pdf_to_markdown.py](../tests/test_pdf_to_markdown.py) |
| Multi-batch behavior and preparation failures | [test_paper_indexer_batching.py](../tests/test_paper_indexer_batching.py), [test_embeddings.py](../tests/test_embeddings.py) |
| Graph/vector sequencing | [test_paper_indexer_graph_integration.py](../tests/test_paper_indexer_graph_integration.py), [test_cli_graph_index_integration.py](../tests/test_cli_graph_index_integration.py) |
| Typed facts and skipped-paper backfill | [test_fact_extraction.py](../tests/test_fact_extraction.py), [test_paper_indexer_facts.py](../tests/test_paper_indexer_facts.py), [test_graph_store.py](../tests/test_graph_store.py) |
| Streaming statuses and BM25 refresh | [test_cli_index_stream.py](../tests/test_cli_index_stream.py), [test_cli_stream_workflows.py](../tests/test_cli_stream_workflows.py) |

A useful exact regression target is `test_batch_failures_leave_old_graph_and_vectors_untouched` in `test_paper_indexer_batching.py`.

For a new extractor/embedder, use `PaperIndexer`'s injection seams and preserve provenance, ordering, and preparation-before-publication. Do not add a UI-only “indexed” flag as a substitute for store publication. Changes to graph semantics belong with [knowledge graph](knowledge-graph.md); changes to durable conversational workflows belong with [orchestration](orchestration.md).
