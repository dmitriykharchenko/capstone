# Knowledge-graph reference

[Documentation home](README.md) · [Architecture](architecture.md) · [Frontend](frontend.md) · [Paper indexing](paper-indexing.md) · [Data storage](data-storage.md)

**Source inspection date: 2026-09-25.** This guide describes the current working-tree source, including uncommitted changes. It is not a clean-checkout, runtime, or visual-validation report. No provider calls, graph-store opens, index mutations, or application tests are performed for this documentation pass.

## Purpose and boundaries

The paper knowledge graph stores bibliographic relationships among **papers**, **authors**, and **topics**, plus typed facts connected through their source chunks. Chunk/fact relations support retrieval; the desktop app has no graph view. It is not a citation graph, a chunk-similarity graph, or the durable question/agent/tool execution graph described in [orchestration](orchestration.md).

| Layer | Tauri/Python path |
| --- | --- |
| Store | [graph_store.py](../src/storage/graph_store.py), `CozoGraphStore` |
| Metadata | [paper_metadata.py](../src/indexing/paper_metadata.py), `PaperMetadataExtractor` |
| Facts | [fact_extraction.py](../src/indexing/fact_extraction.py), `FactExtractor` |
| Database | `config.chroma_directory.parent / "paper_graph.db"` |
| Consistency | Serialized graph transactions; graph/vector writes are separate operations |

The store validates all ten relation schemas and adds missing fact relations to an existing compatible database without replacing legacy data.

## Graph model and stable identity

The conceptual relations are:

| Relation | Key | Value / meaning |
| --- | --- | --- |
| `paper` | `paper_id` | `title` |
| `author` | `author_id` | Display `name`, `normalized_name` |
| `topic` | `topic_id` | Display `name`, `normalized_name` |
| `authored_by` | `(paper_id, author_id)` | Paper-to-author edge |
| `has_topic` | `(paper_id, topic_id)` | Paper-to-topic edge |
| `chunk` | `chunk_id` | `paper_id`, zero-based `chunk_index`, one-based `page` |
| `fact` | `fact_id` | Closed `fact_type`, canonical `value`, evidence `statement`, optional `unit` |
| `paper_chunk` | `(paper_id, chunk_id)` | Paper-to-chunk edge |
| `chunk_fact` | `(chunk_id, fact_id)` | Chunk-to-fact edge |
| `paper_fact_version` | `paper_id` | Completed fact-extraction version used for backfill |

Wire node kinds are `paper`, `author`, and `topic`. Wire edge kinds are exactly `authored_by` and `has_topic`. Edges retain paper-as-source orientation.

`normalize_node_name` compacts whitespace after Unicode NFKC normalization and applies case folding. `stable_node_id` computes SHA-256 over that normalized UTF-8 name and prefixes the digest with `author:` or `topic:`. Namespace separation prevents an author and a topic with the same text from being the same node. Paper IDs come from the paper/indexing contract rather than this author/topic hash function.

Identity normalization is not author disambiguation. Different people with the same normalized display name share an author ID; different spellings are not reconciled by affiliation, ORCID, or a separate entity-resolution model. Topic labels are extracted text, not members of an enforced ontology. Replacing metadata can change a shared node's display spelling without changing its stable identity.

Fact identity is exact normalized matching: `fact:` plus SHA-256 of `fact_type`, a NUL separator, and the NFKC-normalized, whitespace-compacted, case-folded canonical value. A fact repeated in multiple chunks or papers therefore has one `fact` row and multiple `chunk_fact` edges. There is no embedding-based entity merge.

## Metadata extraction and publication

### Extractor contract

The extractor asks for one strict JSON object with exactly `title`, `authors`, and `topics`. Paper text is untrusted reference material, not model instructions. `MAX_METADATA_TEXT_CHARS` is **12,000**; it uses a normalized, bounded view rather than unrestricted document text.

| Field / rule | Current contract |
| --- | --- |
| Title | Nonempty string with compacted whitespace |
| Authors | Array of strings; drop blank entries, deduplicate case-insensitively, retain first-seen display casing/order; at least one remains |
| Topics | Array of strings; drop blanks, casefold and deduplicate; 3–10 distinct entries |
| Object validation | Reject missing/extra fields, duplicate JSON keys, malformed JSON, and nonstring entries |
| Completion | Provider output-limit/incomplete responses are failures, not partial metadata |

Python exposes `parse_metadata_output`, immutable `PaperMetadata`, `PaperMetadataExtractor.extract`, and `extract_paper_metadata`. Errors distinguish configuration, validation/output, model and incomplete-response failures.

The metadata schema's 3–10 topic rule belongs to extraction validation. The lower-level graph replacement APIs primarily require valid identities and nonempty name lists; they are not a substitute for the extractor schema.

### Python indexing integration

[build_paper_indexer](../src/cli/models.py) constructs the graph store beside Chroma and injects `PaperMetadataExtractor` into `PaperIndexer`. Its configured-model branch and its explicit offline mock branch both supply a graph store. The lower-level `PaperIndexer` can be used without one, so library construction and the CLI's normal construction are different seams. A failed builder returns `None` rather than silently publishing a graphless normal CLI indexer.

In [PaperIndexer.index_paper](../src/indexing/paper_indexer.py), the pipeline resolves the PDF, builds bounded chunks, extracts metadata and strict typed facts, and embeds both chunks and unique fact statements before successful-paper writes. Chunks remain in Chroma `documents`; fact statements are stored once by stable ID in Chroma `facts` with type/value metadata. The Cozo replacement writes chunk/fact relations in the same transaction as paper metadata. Empty and replacement paths delete fact vectors only after the graph shows that no chunk in any paper still references them.

Skipped already-indexed papers use their stored Chroma chunks, ordered by `chunk_index`, for fact-only backfill. `replace_paper_facts` changes only chunk/fact relations and the version marker; it does not mutate stored chunk text, embeddings, metadata, or paper/author/topic rows. Papers already marked with the current `FACT_EXTRACTION_VERSION` are not re-extracted.

**Atomicity boundary:** the graph replacement is transactional within CozoDB; the subsequent Chroma delete/upsert is not in the same database transaction. The Python write lock prevents interleaved workers in that indexer, but is not a distributed commit or proof that a later vector-write failure rolls back a successful graph write. See [paper indexing](paper-indexing.md) for outcome/status behavior.

## Persistence and failure contracts

### Python `CozoGraphStore`

- Uses embedded CozoDB with the SQLite engine and `dataframe=False`; creates/validates ten relation schemas.
- Owns a sibling `.lock` file for process exclusion and a `threading.RLock` for in-process serialization. Default `lock_timeout` is 5 seconds. Context-manager exit/`close` releases ownership; operations after closure fail.
- On POSIX the owner writes its PID into the `.lock` file. `stop_lock_owner` stops that owner (SIGTERM, then SIGKILL) only when it is a live `src.cli` process; `index_papers` and `reset_all_indices` call it first, so a new run or reset replaces a previous backend instead of failing as locked. `python -m src.cli` also exits when its parent process is gone, so a closed app leaves no lock owner behind.
- `replace_paper` can replace paper metadata and its chunk/fact graph in one transaction; `replace_paper_facts` provides the fact-only backfill transaction.
- `remove_paper` removes the paper, all incident edges/chunks, its version marker, and newly orphaned author/topic/fact nodes transactionally. Shared facts survive while another chunk references them.
- `facts_for_chunks` and `chunks_for_facts` are deterministic read APIs. `_all_nodes` and `_all_edges` still expose only the legacy explorer nodes/edges.
- Uses parameter-bound values rather than interpolating user identifiers into query scripts.
- Exposes `GraphStoreConfigurationError`, `GraphStoreLockedError`, `GraphStoreClosedError`, and `GraphStorePersistenceError`; failed transactional replacement aborts rather than presenting a partial graph.

Store construction can initialize a missing database/schema. No stores are opened while preparing this guide.

## Source and test map

The following existing tests document intended contracts. They are inspected, not run here, and source-contract checks do not substitute for actual desktop rendering verification.

| Area | Tests / exact meaningful cases |
| --- | --- |
| Python identity/store semantics | [test_graph_store.py](../tests/test_graph_store.py): legacy replacement plus fact deduplication, backfill, and orphan cleanup; [test_fact_extraction.py](../tests/test_fact_extraction.py): strict typed extraction and identity |
| Metadata and indexing | [test_paper_metadata.py](../tests/test_paper_metadata.py), [test_paper_indexer_graph_integration.py](../tests/test_paper_indexer_graph_integration.py), [test_cli_graph_index_integration.py](../tests/test_cli_graph_index_integration.py) |

Keep edge kinds aligned across storage and indexing; update focused contract tests using [operations](operations-and-testing.md). For paper-open failures see [PDF viewer](pdf-viewer.md); for missing graph content see [indexing](paper-indexing.md).
