# Retrieval and chat

[Documentation home](README.md) · [Paper indexing](paper-indexing.md) · [Data storage](data-storage.md) · [Frontend](frontend.md) · [Orchestration](orchestration.md)

**Source inspection date: 2026-09-25.** This guide describes current source, including uncommitted working-tree changes, rather than asserting clean-checkout parity or runtime success. No application, provider, store, or application test is run during this inspection.

## What the current Chat UI calls

[Tauri's chat saga](../app/src/slices/chat/sagas/chat-saga.ts) defaults to `api.chatStream` in `chatSaga`. It does **not** route ordinary sends through the separate durable orchestration API merely because orchestration modules exist.

| Path | Current backend flow |
| --- | --- |
| `app/` | Tauri `chat_stream` → Python `handle_chat_stream` → `AgentLoop.stream_events` → transient `chat_graph.chat_events` |
| Durable orchestration | Separate Python orchestration request/store/runtime/transport contracts; see [orchestration](orchestration.md), not the default Chat saga |

The saga admits one send at a time with `takeLeading`, appends an optimistic user message, renders intermediate evidence/final deltas, and reconciles the assistant text and sources to the terminal result. Stream consumption races against `selectSession`, `createSession`, `setSessionId`, and `loadSession`; these actions stop consuming the current channel. It does not race against every `routeChanged` action, so merely leaving the Chat route does not cancel the stream. Closing the event channel does not itself cancel the Tauri invocation. UI state is not canonical persisted history.

## Distinct retrieval purposes

| Purpose | Python symbol | Result |
| --- | --- | --- |
| Papers list search | `PaperSearchIndex.search` | One `PaperMatch` per paper, using its best candidate chunk |
| Chat evidence | `PaperSearchIndex.retrieve_chunks` | Ranked `PaperChunkMatch` objects, retaining individual chunks and full provenance |
| Explicit strategy retrieval | `PaperSearchIndex.retrieve_evaluation_chunks` | Up to five chunks with `vector`, `bm25`, or `hybrid` full-corpus semantics |
| History search | `ChatHistoryIndex.search` | Ranked canonical SQLite exchanges, not paper evidence |

Source: [paper_search.py](../src/retrieval/paper_search.py), [chat_history_index.py](../src/retrieval/chat_history_index.py), and [CLI commands](../src/cli/commands.py). History search is a separate feature; ordinary chat does not retrieve old exchanges from Chroma as its paper context. It loads the selected session's canonical messages in insertion order.

## Python ranking

### Candidate-limited chat and Papers search

`search(query, n_results=5)` and default `retrieve_chunks(query, n_results=5)` embed the query and request `min(max(n_results * 5, n_results), 50)` Chroma candidates with documents, metadata, and distances. For the five-chunk Chat limit this is at most 25 vector candidates.

Ranking operates on that candidate set:

1. `_similarity` maps a valid nonnegative distance to `round(1 / (1 + distance), 4)`.
2. [bm25.py](../src/retrieval/bm25.py) computes lexical scores from lowercased ASCII alphanumeric tokens. Its defaults are `k1=1.5`, `b=0.75`.
3. `_min_max_normalize` scales vector similarity into `[0, 1]`; all-equal vectors map to `1`.
4. `_max_normalize` divides lexical scores by their maximum; a nonpositive maximum gives zeros.
5. `_hybrid_scores` blends the two equally (`HYBRID_VECTOR_WEIGHT=0.5`) and rounds to four decimals.

The lexical portion cannot recover a chunk omitted from the vector candidate set. The result is a relative ranking score, not calibrated confidence or a relevance probability. There is no positive-score threshold; ties and weak matches can still appear.

`_raw_chunks` validates required provenance and finite nonnegative distances, then sorts descending by blended score. Equal scores retain candidate arrival order. It returns complete chunk text for model context, not the short UI snippet. `_group_by_paper` is more permissive: it skips unusable paper identities, tolerates missing display fields with fallbacks, chooses the strongest candidate per paper, and returns a normalized snippet of at most 240 characters plus an ellipsis when truncated. Multiple requested chunks from one paper collapse to a single Papers result; Chat chunks do not collapse by paper.

### Typed fact-vector candidates

When the search index is constructed with both the Cozo fact graph and the `facts` Chroma collection, retrieval additionally embeds the query against fact statements. Matched fact IDs map through the graph to chunk IDs. Chunks absent from the normal Chroma candidate set are fetched by ID, so fact-only matches can join the result set. Every returned `PaperChunkMatch` includes its stable `chunk_id` and graph-connected `facts`.

The deterministic combined score is `max(normal chunk score, best connected fact similarity)`. A normal chunk score follows the selected retrieval strategy; fact similarity is `round(1 / (1 + distance), 4)`. Equal combined scores use the existing provenance key `(paper_id, chunk_index, filename, page, id)`. Without both optional stores, retrieval follows the legacy path unchanged.

### Explicit retrieval strategies

`retrieve_chunks(..., strategy=...)` delegates to `retrieve_evaluation_chunks`. `EVALUATION_CHUNK_LIMIT=5` caps its return count even when a larger count is requested.

| Strategy | Candidate universe | Embedding call | Ordering |
| --- | --- | --- | --- |
| `vector` | Chroma top results | Yes | Provider collection order, mapped similarity scores |
| `bm25` | Entire persisted corpus or accepted BM25 snapshot | No | Descending BM25, then stable provenance key |
| `hybrid` | Entire corpus; Chroma asked for all corpus vectors | Yes | Equal-weight normalized vector/BM25 score, then stable provenance key |

The deterministic key is `(paper_id, chunk_index, filename, page, id)`. Full-corpus hybrid rejects incomplete vector result counts. It is more expensive than candidate-limited Chat ranking and does not use the persisted BM25 snapshot in this implementation. These low-level APIs exist independently of the current evaluation UI's selected workflow; see [evaluation](evaluation.md) rather than assuming a strategy selector is wired to ordinary Chat.

### Persisted BM25 and freshness caveat

`PaperSearchIndex.refresh_bm25_index` reads the full Chroma corpus, builds `BM25Index`, writes `bm25_index.json` beside the Chroma store, and caches it in the instance. `_resolve_bm25_index` loads lazily; absent/unreadable files fall back to on-the-fly scoring. A count mismatch rejects a loaded index.

The freshness check compares **chunk count only**. It cannot detect a same-count text/provenance replacement, and a previously loaded object is reused rather than rereading every query. Rebuild after corpus changes; do not describe the count guard as content-integrity verification. [BM25Index](../src/retrieval/bm25_index.py) contains corpus text, provenance, term frequencies, document lengths, and document frequencies, not merely an opaque score cache.

## Python ordinary multihop chat

The meaningful interfaces are `AgentLoop.run`, `AgentLoop.stream`, and `AgentLoop.stream_events` in [agent_loop.py](../src/chat/agent_loop.py). `build_agent_loop` in [models.py](../src/cli/models.py) supplies a history store, optional history index, required paper retriever, and configured or mock model.

### Turn stages and bounds

| Stage | Current behavior |
| --- | --- |
| Prepare | `_start_turn` validates the request, resets last-result fields, requires a paper retriever, loads session history, and initializes the model if needed |
| Plan | `_plan_multihop` expects a JSON object containing only `subqueries`; `_parse_plan` requires 1–5 distinct normalized nonempty strings. A single fact from one paper uses exactly one search and is never split into reworded searches. |
| Initial hops | Each subquery retrieves up to `PAPER_CHUNK_LIMIT=5` chunks, builds untrusted context, and generates a subanswer |
| Coverage | One `_review_multihop_coverage` call evaluates the initial subanswers and can add at most two distinct follow-up subqueries |
| Follow-up hops | Retrieve and answer the follow-ups; there is no unbounded review loop |
| Compose | The model composes a final answer from the original question, history, subqueries, and subanswers |
| Persist | Atomically append the completed user/assistant pair and aggregate sources, then optionally index the exchange for history search |

`MAX_TOTAL_HOP_COUNT=7` and `MAX_AGGREGATE_SOURCE_COUNT=35` are hard bounds. `_aggregate_records` deduplicates by `(paper_id, chunk_index)` in first-seen hop order and reranks sources consecutively. It does not globally rerank all ordinary-chat sources against the original question; the separately documented orchestration evidence-fusion path must not be substituted into this description.

`_retrieve_multihop` calls the default candidate-limited retriever and fails on zero chunks. It also fails on missing retrieval infrastructure or malformed provenance instead of answering without evidence. Mock mode is deterministic but still requires retrievable chunks; it is not a bypass around indexing.

The ordinary `AgentLoop._plan` passes no `PlannerRetryPolicy`, so `_plan_multihop` follows its direct planner path. Evaluation-specific retry/resume behavior is not automatically enabled for ordinary Chat. Provider SDK retry policy is a separate layer; see [configuration](configuration.md).

### Prompt and source boundaries

`_paper_context` serializes chunk text, typed facts, and provenance into an explicitly untrusted context block, delimited by `UNTRUSTED_CONTEXT_BEGIN` and `UNTRUSTED_CONTEXT_END`. Subanswer prompts separate this evidence from system instructions. This is a prompt boundary, not a guarantee that a model cannot be influenced by document text. Durable orchestration includes a compact fact rendering with its evidence snippet and still applies the existing 1,024-byte snippet limit.

| Source field | Meaning |
| --- | --- |
| `rank` | Consecutive one-based rank in the returned source list |
| `paper_id`, `chunk_index` | Stable chunk identity for deduplication |
| `filename`, `title`, `page` | Human-readable provenance and one-based page |
| `score` | Retrieval ranking signal |
| `facts` | Typed graph facts connected to the retrieved chunk: type, canonical value, source statement, and optional unit |
| `snippet` | Ordinary Python Chat generates a compact whitespace-normalized excerpt of at most 240 characters; the storage validator separately allows up to 280 |

The model receives full selected chunk text; history stores compact source snapshots. Sources are attached to the completed assistant message on hydration. The source list records retrieved evidence; it is not a machine-verified mapping from every answer sentence to an exact supporting quote. Links from source records to graph or PDF UI are described in [frontend](frontend.md), [knowledge graph](knowledge-graph.md), and [PDF viewer](pdf-viewer.md).

### Streaming is provisional until completion

[chat_graph.py](../src/chat/graph.py) implements a pull-driven transient LangGraph. `get_chat_graph` compiles topology without checkpoint stores, tracing, graph retries, or a second conversation database. Each yielded event returns control to the consumer before further model work or persistence. `ChatInvocation` keeps live model/history resources and provider iterators outside graph state.

| Python public event | UI wire event | Meaning |
| --- | --- | --- |
| `MultihopPlanEvent` | `plan` | Initial plan |
| `MultihopHopStartedEvent` | `subquery` | Current hop identity/query |
| `MultihopHopCompletedEvent` | `subanswer` | Hop answer and sources |
| Coverage start/decision | `coverage` | Reviewing, complete, or added follow-ups |
| `MultihopFinalDeltaEvent` | `delta` | Provisional final-answer text |
| `MultihopCompletedEvent` | `done` | Authoritative answer/sources after persistence |

`handle_chat_stream` stops at the core's completion record and emits the authoritative reply. Deltas, subanswers, or a closed stream are not substitutes for `done`. `_raise_if_output_limited` detects recognized output-limit metadata before accepting responses; an incomplete final stream can already have displayed text but must not persist as a successful answer. Closing the generator before completion prevents the later persistence step.

`AgentLoop._persist` first calls `SQLiteHistoryStore.append_exchange`, then `history_index.index_exchange`. CLI construction wraps the latter in `_SafeIndex`, so derived indexing failure does not roll back the canonical reply. The returned Python `indexed` flag is `index is not None`: it reports index availability, **not verified success of this exchange's vector write**. Direct users injecting another history-index implementation do not receive `_SafeIndex` automatically.

Stream errors are converted to a specific incomplete-response message or a generic chat-failed message. `last_sources`/`last_context` are cleared at turn start and populated after canonical persistence; a failed new turn does not retain the prior turn's evidence in these fields. Python logs are best-effort but include question, answer, provenance, and full retrieved context; see [data storage](data-storage.md).

## Errors and practical diagnosis

| Symptom | Source-grounded interpretation |
| --- | --- |
| Paper search asks to index first | Empty/unavailable Python paper collection; the handler does not auto-index |
| Chat fails before final answer | Missing/empty paper retrieval, invalid plan/coverage, provider failure, output limit, or persistence failure can terminate a turn |
| Python sources exist but History search misses the turn | Canonical append can succeed while best-effort exchange vector indexing fails |
| Similarity seems high for weak results | Scores are normalized rankings; no calibrated threshold is enforced |

## Source and test map

Existing tests cover offline seams; they are not executed for this documentation task. For approved verification commands, see [operations and testing](operations-and-testing.md).

| Concern | Existing tests |
| --- | --- |
| Candidate/full-corpus ranking and provenance | [test_paper_search.py](../tests/test_paper_search.py) |
| Required evidence and complete-response persistence | [test_agent_loop.py](../tests/test_agent_loop.py), [test_agent_loop_multihop.py](../tests/test_agent_loop_multihop.py) |
| Pull boundaries, close, and final persistence | [test_chat_graph.py](../tests/test_chat_graph.py) |
| Canonical exchange hydration | [test_chat_history_index.py](../tests/test_chat_history_index.py), [test_sqlite_history.py](../tests/test_sqlite_history.py) |
| UI terminal/progress contract | [chat-progress-saga.test.mjs](../app/tests/chat-progress-saga.test.mjs), [chat-progress-state.test.mjs](../app/tests/chat-progress-state.test.mjs) |

A useful exact regression target is `test_hybrid_fusion_has_deterministic_provenance_tie_breaking` in `test_paper_search.py`.

For an approved behavior change, extend the smallest relevant offline test seam before running its focused suite. Changing ranking, source limits, or terminal-event semantics requires checking storage and frontend contracts as well as the retriever itself.
