# Data storage and consistency

[Documentation home](README.md) · [Architecture](architecture.md) · [Configuration](configuration.md) · [Paper indexing](paper-indexing.md) · [Retrieval and chat](retrieval-and-chat.md)

**Source inspection date: 2026-09-25.** Paths and schemas below come from current source, including uncommitted implementation, not from opening live stores or reading environment files. This is not a clean-checkout or runtime validation claim. No application, database operation, provider call, install, or application test is run for this guide.

## Store ownership at a glance

“History,” “index,” and “checkpoint” are different kinds of state. A derived index is not the authority for a conversation, and an indexing outcome indicator is not proof that all stores committed together.

| Path | Canonical / authoritative state | Derived or supporting state |
| --- | --- | --- |
| Python/Tauri | PDFs; SQLite completed exchanges/messages/source snapshots; separate orchestration store for orchestration requests | Chroma paper vectors, Chroma exchange vectors, BM25 JSON, Cozo graph, SQLite paper outcome indicators, plaintext logs |
See [getting started](getting-started.md), [desktop bridge](native-desktop.md), and [orchestration](orchestration.md) for the surrounding lifecycle.

## Python storage layout

Defaults originate in [CLIConfig](../src/cli/config.py), [model construction](../src/cli/models.py), and the adapter modules. These are code defaults, not observed machine settings.

| Default location / name | Owner and content |
| --- | --- |
| `ResearchPapers/` | Input PDFs; indexing reads top-level files |
| `data/chat_history.sqlite3` | `SQLiteHistoryStore`: messages, exchanges, sources, paper statuses |
| `data/chroma/`, collection `documents` | Paper chunks, explicit embeddings, provenance metadata |
| `data/chroma/`, collection `chat_exchanges` | Embedded completed exchange contexts with SQLite identity metadata |
| `data/chroma/bm25_index.json` | `BM25Index` snapshot of full paper corpus text/provenance and lexical statistics |
| `data/paper_graph.db` | `CozoGraphStore`, resolved as the Chroma directory's parent plus `paper_graph.db` |
| `data/chat_logs.txt` | Best-effort plaintext chat log |
| `data/retrieval_logs.txt` | Best-effort plaintext retrieval log, including full supplied context for chat |

Relative overrides and endpoint/model settings are detailed in [configuration](configuration.md). Changing one path does not relocate the others atomically. In particular, the graph path follows the Chroma parent, whereas the history path is separately configured.

### Canonical SQLite schema

[sqlite_history.py](../src/storage/sqlite_history.py) implements `SQLiteHistoryStore`. Construction creates missing parent directories, opens one `sqlite3` connection, enables foreign keys, and initializes/migrates tables.

| Table | Key and columns | Role |
| --- | --- | --- |
| `exchanges` | `id INTEGER PRIMARY KEY AUTOINCREMENT`, `session_id` | One completed turn identity |
| `messages` | `id`, `session_id`, `role`, `content`, nullable `exchange_id` | Ordered canonical transcript; foreign key to exchanges with cascade delete |
| `exchange_sources` | Primary key `(exchange_id, rank)`; `paper_id`, `chunk_index`, `filename`, `title`, `page`, `score`, `snippet` | Compact evidence snapshot attached to a completed exchange |
| `paper_index_status` | `paper_id` primary key; `filename`, `status`, `chunk_count`, nullable `error`, `updated_at` | Last recorded indexing outcome used for UI indicators and Python skip decisions |

Indexes `idx_messages_session_id(session_id, id)` and `idx_messages_exchange_id(exchange_id, id)` support ordered reads. The store adds a missing `exchange_id` column to legacy message-only databases. Existing loose messages remain loose; migration does not fabricate completed exchanges from adjacent user/assistant rows.

### Atomic exchange writes and reads

- `append_message` validates a conventional role and nonblank content, then commits one loose message.
- `append_exchange(session_id, user_content, assistant_content, sources=...)` validates inputs before one transaction inserts the exchange, both ordered message rows, and all sources. Any insert failure rolls back the complete exchange.
- `get_exchange(exchange_id, session_id)` verifies ownership and requires exactly the completed user/assistant pair. It raises for a missing or malformed canonical exchange.
- `get_messages(session_id)` returns insertion order. An unknown valid session returns an empty list; source snapshots attach to the assistant record, with the exchange ID carried for navigation.
- `record_paper_status` upserts by paper ID with a UTC timestamp; `clear_paper_statuses` deletes indicators only. Neither is a commit marker for Chroma or Cozo.
- `close` and context-manager exit release the owned connection. The class does not add a cross-thread connection-sharing policy, cross-process lock, or distributed transaction.

`SourceRecord` has `rank`, `paper_id`, `chunk_index`, `filename`, `title`, `page`, `score`, and `snippet`. `_validate_sources` requires consecutive ranks, nonnegative chunk indices, positive pages, finite scores, nonblank text fields, at most `MAX_EXCHANGE_SOURCE_COUNT=35` sources, and snippets of at most `MAX_SOURCE_SNIPPET_LENGTH=280` characters. Ordinary Chat generates shorter 240-character snippets. `MessageRecord` includes optional `exchange_id` and a tuple of sources; `ExchangeRecord` includes owning session and the two canonical message objects.

Ordinary chat persists only after successful composition. Subqueries, provisional deltas, and failed partial answers are not separate history turns. The selected session's full persisted transcript is loaded for a new ordinary Python turn; this store does not implement history summarization or an automatic retention window.

### Chroma paper collection

[chroma_persistence.py](../src/storage/chroma_persistence.py) is an explicit-vector adapter. `create_client` lazily imports Chroma and creates a persistent client; `open_collection` opens or creates the named collection. The adapter does not generate embeddings or call a provider.

`upsert_records` validates equal record lengths, unique nonempty IDs, text/vector input, and required paper provenance (`paper_id`, nonnegative integer `chunk_index`). Indexing supplies the stronger retrieval fields `filename`, `title`, and `page` too. A valid low-level record is not necessarily sufficient for strict Chat retrieval, which additionally validates full provenance and distances.

`get_records` supports IDs, a metadata filter, or an unfiltered full read. `query_records` accepts a caller-supplied query vector and optional metadata filter. `delete_records` requires either IDs or a metadata filter and refuses an unconditional collection deletion. The reset command separately uses the client's collection-deletion API.

Paper replacement is delete-then-upsert under an indexer-instance lock, not a single Chroma/Cozo/SQLite transaction. See [paper indexing](paper-indexing.md) for batch preparation and publication failure boundaries. There is no durable Python paper-publication journal that reconciles a half-completed graph/vector replacement on startup.

### Chroma exchange collection

[ChatHistoryIndex](../src/retrieval/chat_history_index.py) opens `chat_exchanges` rather than `documents`. It deliberately writes its own exchange metadata rather than using the paper-only provenance adapter.

| Record part | Value |
| --- | --- |
| Vector ID | `exchange:<positive SQLite exchange_id>` from `exchange_vector_id` |
| Embedded document | `build_exchange_context`: labeled user text followed by labeled assistant text |
| Metadata | Canonical `exchange_id` and `session_id` |
| Search result authority | SQLite `get_exchange`, not the vector document text |

`index_exchange` hydrates a complete canonical exchange before embedding and upserting, making repeated indexing use the same vector ID. `search` embeds the query, applies an optional session filter, validates result identity metadata, and hydrates the corresponding canonical exchanges in vector ranking order. Missing or mismatched canonical data is an error.

CLI `_SafeIndex` suppresses exchange-indexing failures **after** canonical persistence. This favors keeping the conversation over guaranteed immediate History search coverage. There is no automatic retry journal in this Python adapter. Never treat the Python chat response's `indexed` boolean as an exact per-exchange confirmation; it indicates that an index adapter exists.

### BM25 snapshot format and atomicity

[BM25Index](../src/retrieval/bm25_index.py) serializes `INDEX_VERSION=1`, `k1`, `b`, `document_frequency`, and a `documents` array. Each document contains provenance, full text, term frequencies, and token length. `save` writes a sibling `.tmp` file and uses `os.replace` to avoid torn reads of the final file. That is a single-file replacement, not a multi-store commit or writer-serialization mechanism.

`load` returns `None` for absent/unreadable data so queries can fall back. `PaperSearchIndex` uses a live chunk-count comparison, not a corpus hash, to reject obviously stale snapshots. Same-count changes can evade that guard. `handle_bm25_status` reports what the file covers; it does not establish that the file matches the live collection. See [retrieval and chat](retrieval-and-chat.md) for which search paths actually consult this snapshot.

### Graph, orchestration, evaluation, and logs

The Cozo graph contains normalized paper/author/topic relationships, not the canonical conversation. Its schema and graph-query boundaries are described in [knowledge graph](knowledge-graph.md); inspect [graph_store.py](../src/storage/graph_store.py) for `CozoGraphStore`.

The separate [orchestration store](../src/orchestration/store.py) is not another name for `SQLiteHistoryStore` and is not the transient `chat_graph` state. It tracks durable orchestration lifecycle and publication contracts; see [orchestration](orchestration.md). Evaluation artifacts have their own retention and terminal-publication behavior; see [evaluation](evaluation.md) and [evaluation_results.py](../src/evaluation/results.py).

[log_chat](../src/chat/chat_logger.py) writes session, model/mode, input, reply, and source metadata. [log_retrieval](../src/chat/retrieval_logger.py) writes query, results, and optional full context verbatim. Both create parent directories as needed and suppress `OSError`. They are supporting plaintext records, not canonical stores, and can contain sensitive user text and document excerpts. Do not share them as harmless diagnostics or assume index reset deletes them. No automatic rotation/retention policy is implemented in these logging functions.

## Reset and retention matrix

**These are destructive backend capabilities, not commands executed by this guide.** Back up canonical data appropriately and identify the intended runtime before an approved reset. Do not manually delete SQLite sidecars or files from a running application.

| Data | `reset_all_indices` |
| --- | --- |
| Source PDFs | Preserved |
| Canonical history and saved source snapshots | Preserved |
| Paper vectors/chunks | Best-effort delete `documents` collection |
| Exchange vectors | Preserved in `chat_exchanges` |
| Lexical index | Removes BM25 JSON independently |
| SQLite outcome indicators | Cleared |
| Graph | Preserved |
| Logs / evaluation artifacts / separate orchestration state | Outside the reset handler |

Reset flags are best-effort step reports, not a cross-store commit certificate. Retained canonical history means old messages and citations remain readable even if paper retrieval has been cleared; it does not mean their source PDF or current chunk layout still exists unchanged.

## Consistency limits to keep visible

1. Python indexing statuses can outlive a changed or removed PDF; skip decisions are status-based, not source-hash-based.
2. Python graph/vector/status/BM25 writes have no shared transaction. A successful terminal response can coexist with best-effort status or BM25 failure.
3. Python BM25 count checks cannot detect same-count content changes.
4. Canonical exchange persistence and derived exchange search are separate. The response often only reports adapter availability.
5. File reads do not prevent an external editor from subsequently changing a PDF.
6. A history source record is a stored excerpt/provenance snapshot, not a permanent copy of the PDF or an immutable proof that the present file matches it.
7. Changing embedding identities in place or deleting one store to “repair” another can violate these assumptions. Follow [operations and testing](operations-and-testing.md), not a blanket filesystem cleanup.

## Existing verification map

The paths below identify tests to extend and run for an approved code change; no application tests are run for this documentation-only task.

| Contract | Tests |
| --- | --- |
| Canonical Python exchange/source transactions and migration | [test_sqlite_history.py](../tests/test_sqlite_history.py) |
| Paper vector provenance adapter | [test_chroma_persistence.py](../tests/test_chroma_persistence.py) |
| Exchange vector identity and SQLite hydration | [test_chat_history_index.py](../tests/test_chat_history_index.py) |

