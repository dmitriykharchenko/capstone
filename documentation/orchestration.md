# Orchestration and workflow boundaries

[Documentation index](README.md) · [Architecture](architecture.md) · [Retrieval and chat](retrieval-and-chat.md) · [Evaluation](evaluation.md)

**Source inspection date: 2026-10-14.** This guide describes the current working tree, including uncommitted implementation. It is not a claim of clean-checkout parity, runtime acceptance, or a completed feature rollout. No application, provider, live store, or application test is run for this documentation.

## 1. Two different meanings of orchestration

| Layer | Current implementation | Persistence and actual entry point |
| --- | --- | --- |
| Normal Python command workflows | Ephemeral LangGraph chat, indexing, and evaluation graphs; direct unary dispatch | Existing CLI/Tauri commands; canonical chat history and operation-specific persistence remain outside LangGraph |
| Durable specialist orchestration | Intake, question handlers, finalizer, bounded tools, SQLite journal, owned execution handles | Separate opt-in `orchestration_*` CLI/Tauri commands; not the default visible Chat flow |

Do not confuse a transient graph invocation with a durable run. The normal Chat UI displays the adaptive multihop pipeline described in [retrieval and chat](retrieval-and-chat.md). Its planner, coverage review, and hop progress are not the durable intake/question/finalizer trace.

### Actual UI wiring

- [`app/src/slices/chat/sagas/chat-saga.ts`](../app/src/slices/chat/sagas/chat-saga.ts) exports `chatSaga(chatStream = api.chatStream)` and `streamChatTurn`. It consumes the existing `plan`, `coverage`, hop, `delta`, `done`, and `error` stream, not `orchestrationApi`.
- [`app/src/lib/orchestration-api.ts`](../app/src/lib/orchestration-api.ts) exposes opt-in start/resume/pending/discard/trace methods. Its comment explicitly describes a later trace/recovery UI; the presence of these methods does not make recovery controls visible in Chat.
- `legacyOrchestrationEvent` maps only `final_delta`, `done`, and `error` into the old transcript contract. Specialist answers, tool receipts, questions, evidence, and snapshots never become assistant transcript text through this adapter. Snapshot-driven replacement of uncommitted text belongs to a future consumer.
- [`app/src-tauri/src/lib.rs`](../app/src-tauri/src/lib.rs) registers all five orchestration commands alongside the unchanged `chat` and `chat_stream` commands.
- The [desktop host](native-desktop.md) exposes both ordinary Chat and the opt-in orchestration commands; their existence does not connect a recovery UI.

## 2. Normal caller-pulled workflow graphs

### Command and unary routing

[`src/cli/__main__.py`](../src/cli/__main__.py) reads one JSON request. Ordinary unary requests reach `dispatch` in [`src/cli/commands.py`](../src/cli/commands.py), which calls one allowlisted handler directly; streaming requests call their stream handler directly. Durable `orchestration_*` requests are intercepted separately, before this normal routing path.

| Command family | Allowed mode |
| --- | --- |
| `chat`, `index_papers` | Unary and streaming |
| `evaluate` | Streaming only |
| `health`, `sessions`, `messages`, `search`, `search_papers`, `paper_metadata`, `paper_statuses`, `bm25_status`, `reset_all_indices` | Unary |

There is no command-level graph, checkpointer, retry policy, or result cache: an unknown command raises `ValueError` and handler errors surface unchanged as JSON errors.

### Chat, indexing, and evaluation

| Graph and symbols | How work advances | Persistence boundary |
| --- | --- | --- |
| [`src/chat/graph.py`](../src/chat/graph.py): `get_chat_graph`, `chat_events` | Prepare/plan; hop start; retrieval/subanswer; coverage; optional follow-up hops; compose; final deltas; persist | `AgentLoop._persist` writes the canonical exchange only after valid final composition; streamed generation is pulled by the caller |
| [`src/cli/stream_workflows.py`](../src/cli/stream_workflows.py): `get_index_graph`, `index_events` | One indexing progress item at a time; emits start/progress/final counts | Injected persistence handles outcomes; final refresh happens only when the iterator reaches finish |
| Same file: `get_evaluation_graph`, `evaluation_events` | Prepare, case start/result, retry-round selection, complete | In-memory stage checkpoints; terminal artifact publication is an outer CLI boundary, not a LangGraph durable checkpoint |

`chat_events` owns the model delta iterator and cleanup. Closing a stream is not permission to keep generating, persist an incomplete reply, or prefetch the next evaluation case. See [paper indexing](paper-indexing.md) and [evaluation](evaluation.md) for their distinct failure and completion rules.

## 3. Durable state and storage ownership

### State model

[`src/orchestration/state.py`](../src/orchestration/state.py) defines closed, typed public records and versioned JSON serialization. It does not store executable objects, provider responses, hidden prompts, private reasoning, or arbitrary metadata.

| Record or enum | Meaning |
| --- | --- |
| `RunState` | Run/session/graph identity, run status, retry counter, optional canonical exchange, fixed error code |
| `QuestionState` | Public question, parent/depth, work status, bounded answer and cited evidence IDs |
| `AgentState` | Intake/question/finalizer kind, assignment, work status, retry count, fenced attempt number |
| `EvidenceState` | Paper/chunk identity, compact snippet, score, optional page, originating question |
| `ToolResult` | Receipt for an already-committed effect; contains identifiers, not raw tool input/output |
| `Checkpoint`, `OrchestrationEvent` | Validated snapshots and append-only journal events |
| `RunStatus` | `running`, `interrupted`, `completed`, `failed`, `discarded` |
| `WorkStatus` | `queued`, `running`, `waiting`, `completed`, `failed`, `cancelled` |

`TERMINAL_RUNS` contains completed/failed/discarded. Completed question answers, evidence, and committed tool receipts cannot be rewritten. Identifiers are bounded to 128 ASCII-pattern characters; text bounds also check UTF-8 byte length.

### SQLite store

[`src/orchestration/store.py`](../src/orchestration/store.py) exports `SQLiteOrchestrationStore`. Its standalone default is `data/chat_orchestration.sqlite3`. The CLI's `store_path` resolves `chat_orchestration.sqlite3` next to the configured canonical history database, rather than reusing the history file. `:memory:` and the canonical `chat_history.sqlite3` filename are rejected as orchestration storage targets.

The database schema version is **6** (`DATABASE_SCHEMA_VERSION`); checkpoint/event serialization remains **1** (`SCHEMA_VERSION`). `APPLICATION_ID` is `0x4F524348`. These versions are distinct from the transport version and `GRAPH_VERSION = "orchestration-v1"`.

| Table | Responsibility |
| --- | --- |
| `runs` | Current revision, session/graph ownership, status, optional unique exchange binding |
| `events` | Ordered operation ID, request hash, serialized event and integrity hash |
| `checkpoints` | Snapshot paired with each event revision |
| `noop_receipts` | Stable replay receipts for successful lifecycle no-ops without a new event revision |
| `adapter_requests` | Immutable turn/query/history-cutoff binding |
| `source_display` | Bounded immutable filename/title display metadata per paper/chunk |

`commit(snapshot, operation_id=...)` accepts a candidate at its current sequence and atomically checks that revision, validates the transition, increments the sequence, and writes the event/checkpoint pair. The stable operation ID and exact request hash make replay idempotent: the same input returns the original committed snapshot; a conflicting input fails. Lifecycle no-ops reserve their operation IDs too.

Important APIs are `create_run`, `load(..., sequence=None)`, `events(..., after=0, limit=256)`, `list_pending`, `trace_for_exchange`, `request`, `remember_sources`, `resume`, `interrupt`, `attach_exchange`, `complete`, `fail`, and `discard`. Reads validate ownership, graph version, schema, record relationships, and integrity. Opening a database may initialize or upgrade exact supported older layouts; these methods are not a read-only inspection tool for arbitrary SQLite files.

`RunRequest` in [`src/orchestration/request.py`](../src/orchestration/request.py) holds `turn_id`, original query, canonical history cutoff, and hash. Canonical history is referenced, not copied into the journal. Resume re-reads the bounded history through the cutoff and verifies its fingerprint.

### Limits

[`Limits`](../src/orchestration/tools.py) values are defaults and upper bounds unless the table gives a range. Callers may lower them only within validation bounds.

| Limit | Default / accepted range |
| --- | --- |
| Questions | 8 / 1–8 |
| Question depth | 2 / 0–2; roots are depth 0 |
| Active admitted workers | 4 / 1–4 |
| Chunks per retrieval | 5 / 1–5 |
| Automatic retries per question handler | 1 / 0–1 |
| Optional retrieval calls per agent attempt | 2 / 1–2; original-query intake retrieval is additional |
| Follow-up children per question | 2 / 0–2 |
| State hard bounds | 32 agents, 128 evidence records, 256 tool receipts, 4096 events; no-op receipts are separately bounded |
| Serialized checkpoint / event | 1,048,576 / 16,384 bytes |
| Tool query / answer / retrieved snippet | 2048 / 4096 / 1024 UTF-8 bytes |
| Structured specialist response / general prompt budget | 16,384 / 65,536 bytes; prompt instructions count toward the input budget |
| Runtime attempt deadline | 60 seconds by default, checked before and after polling |

State/wire text limits may be broader than capability limits: a tool answer is restricted to 4096 bytes even though stored/public answer fields permit 8192. Use the narrowest relevant boundary when extending the system.

## 4. Tools and specialist responsibilities

### Scoped capabilities

[`src/orchestration/tools.py`](../src/orchestration/tools.py) binds `OrchestrationTools` to a scheduler-created `ToolScope(run_id, session_id, graph_version, agent_id, attempt)`. Model output cannot choose another run, worker, or attempt.

| Tool | Authorized work and replay behavior |
| --- | --- |
| `retrieve_context(query, call_id=..., best_effort=False)` | Intake or question handler; bounded `PaperChunkMatch` consumption; deterministic evidence/receipt commit |
| `add_question(question, call_id=..., parent_id=None)` | Intake may add roots; handlers may add only children of their assigned question; normalized duplicates and work budgets are rejected |
| `answer_question(question_id, answer, evidence_ids, call_id=...)` | Assigned question handler only; cites at most ten unique permitted records; answer and receipt commit together |

`authorize` reloads the latest state and verifies kind, assignment, attempt, active status, and absence of an already attached exchange. `_prepare` combines call ID with canonical arguments; changing arguments under the same call ID fails rather than silently performing new work. `public_text` normalizes Unicode/whitespace and rejects controls and recognizable private-content markers; it is defense in depth, not a semantic secret detector.

Best-effort retrieval records a durable empty outcome and `retrieval_failed` warning on failure. A successful retrieval with no evidence is distinguishable as `empty_evidence`. Replay returns that recorded outcome rather than querying again. Retrieved snippet text is bounded; unsafe recognizable content is replaced with an omission marker.

### Intake and the current partial aspect logic

[`src/orchestration/agents.py`](../src/orchestration/agents.py) defines `StructuredModel.invoke(tuple[PromptMessage, ...]) -> str`, `run_intake`, `run_handler`, and `run_finalizer`. The model returns strict JSON text, not a provider object. Role-specific trusted instructions are separated from untrusted JSON containing queries, history, and source text.

`run_intake` first performs the exact original-query retrieval with `ORIGINAL_QUERY_CALL_ID = "intake-original-query"`. This mandatory receipt is run-scoped across intake attempts. It then makes one structured planning call: `parse_intake` expects exactly `parts`, containing 1–8 `{part, question}` pairs. A part label is at most 256 UTF-8 bytes; a question is at most 2048. The parser rejects duplicate keys, extra fields, malformed/non-public text, and normalized overlapping labels/questions. A multi-part plan cannot contain a question equal to the normalized whole original request.

The following is a **shape example**, not a runtime call:

<augment_code_snippet mode="EXCERPT">
````json
{"parts":[{"part":"Accuracy","question":"How do A and B compare on accuracy?"},{"part":"Latency","question":"How do A and B compare on latency?"}]}
````
</augment_code_snippet>

Only question strings are sent to `add_question`. The `part` labels are not persisted as durable aspect records, passed through the wire snapshot, or displayed by a trace UI. Coverage of every semantic aspect is requested by the prompt; exact string validation does not prove semantic exhaustiveness.

**Current implementation caveat:** [`OfflineStructuredModel.invoke`](../src/cli/orchestration_workers.py) still returns the older intake `queries`/`questions` shape. That does not satisfy the current `parse_intake` schema. The current source therefore does not justify claiming end-to-end offline compatibility or final aspect-decomposition acceptance. This guide records the mismatch without modifying application code.

### Handlers, follow-ups, and finalization

`run_handler` retrieves for its assigned question, then asks for exactly `answer`, `evidence_ids`, and `followups`. It may use the intake evidence plus evidence retrieved by that handler. It cannot cite arbitrary sibling evidence. Up to two gap-driven follow-ups are added before the answer receipt: this ordering keeps follow-ups recoverable if the worker stops after committing the answer but before acknowledging completion.

`run_finalizer` requires every question to be settled, consumes completed answers and eligible evidence, and alone returns `FinalizerResult.assistant_text`. It explicitly supplies warnings for failed/cancelled questions, missing evidence, retrieval failure, duplicate questions, and budget omissions. Final output is bounded to 8192 bytes; late output is fenced if the input checkpoint changes.

Finalizer history accepts only user/assistant turns, at most 16 turns, each at most 2048 bytes, with a 7500-byte encoded history budget. `history_context` in [`src/orchestration/publication.py`](../src/orchestration/publication.py) reads only the bounded prefix through the stored cutoff.

## 5. Evidence fusion and compression

`evidence_groups`, `rank_evidence`, `handler_evidence_ids`, and `final_evidence_ids` are in [`src/orchestration/tools.py`](../src/orchestration/tools.py). Duplicate groups connect either identical paper/chunk IDs or normalized snippet text; a later item can bridge groups. Representatives are ordered by descending score, then paper ID, chunk index, snippet, and evidence ID. Durable receipts retain bounded variants; consumers receive ranked winners.

Final evidence includes original-query intake evidence and retrieved evidence belonging to completed questions. Citation coverage is tracked across duplicate groups, so an intake representative may support several answered questions without cloning the source.

[`src/orchestration/evidence.py`](../src/orchestration/evidence.py) provides `prepare_evidence`, `EvidencePacket`, `RankedEvidence`, and `EvidenceBudget`:

| Budget | Default |
| --- | --- |
| Final evidence packet | 12,000 bytes |
| Compression input, including reserved prompt space | 16,000 bytes |
| Compression summary | 4096 bytes |
| Selected records | 16 |

If all evidence fits, no compression model call is needed. Otherwise, the reducer reserves the highest-ranked supporting record for every covered question before filling spare capacity by rank. Selected snippets are compacted to 256 bytes for compression; inability to preserve required coverage fails rather than silently dropping a question. Compression must return a bounded summary and every selected evidence ID in the exact supplied order. The finalizer receives either snippets or the summary plus attributed metadata.

## 6. Durable scheduler, execution ownership, and failure handling

[`src/orchestration/graph.py`](../src/orchestration/graph.py) is a pure reducer layer. `transition_graph` routes `QuestionAdded`, `QuestionAnswered`, `AgentCompleted`, or `Reconcile` to one candidate checkpoint; SQLite, not LangGraph, owns recovery.

1. `reconcile` creates intake if no agent exists.
2. Committed pending questions get one deterministic handler each via `question_added`.
3. Failed question handlers with remaining retry budget are requeued with incremented attempt/retry counters.
4. Queued work is admitted within `Limits.concurrency`.
5. `question_answered` creates the finalizer only after intake completes, all questions and worker acknowledgements settle, and no retry remains eligible.

An answer receipt is stronger evidence than a failed completion notification: a committed complete question is retained, not regenerated. Duplicate/stale notifications are harmless because reducers consult committed state and fenced attempt IDs. Intake/finalizer failures do not get the question handler's automatic retry allowance.

[`OrchestrationRuntime`](../src/orchestration/runtime.py) exposes `step`, `notify`, `resume`, `discard`, and `close`. `step` advances one transition or polls one owned handle, rotating active workers so a slow worker cannot starve peers. It commits admission before dispatch and retires fenced physical handles before reusing logical capacity. A thrown commit may already be durable; recovery reloads and reconciles rather than assuming rollback.

The injected ports are `Executor.submit(operation_id, Dispatch) -> Handle`, nonblocking `Handle.poll() -> Delta | Completed | None`, `Handle.close()`, durable `Publisher`, and `Sink.emit`. The core runtime itself does not create worker threads, processes, providers, or history stores. The production CLI adapter does:

- [`ProcessExecutor`, `ProcessHandle`, `_worker`](../src/cli/orchestration_workers.py) use multiprocessing `spawn`. Each worker opens its own orchestration store, wraps the paper retriever with `DisplayRetriever`, and constructs its model inside the worker.
- `ProcessHandle.close` terminates, joins for two seconds, then kills/joins a remaining worker. Provider exception details and provider objects are not sent back as public IPC results.
- `specialist_steps` calls the synchronous specialist and slices validated finalizer output into 256-character deltas. These are **not live provider token streaming**; only the finalizer can emit assistant deltas.
- [`session_owner`](../src/cli/orchestration.py) takes a nonblocking OS file lock under `.orchestration-locks` beside the orchestration database. A crash releases the lock; there is no expiring lease that permits two owners.

`resume` is explicit. The old owner must be stopped first. Resume fences active/waiting attempts, preserves committed answers/evidence/receipts, and requeues unfinished work. Repeating a nonterminal `orchestration_start` is a lookup plus error, not implicit recovery. Closing an unfinished runtime interrupts and closes owned resources; it does not publish a partial reply.

## 7. Canonical publication and discard arbitration

[`CanonicalPublisher`](../src/orchestration/publication.py) implements durable `lookup`, `publish`, and `discard` against canonical history. Its `orchestration_publications` table records a fingerprinted published exchange or discarded tombstone for the run key.

Publication uses one SQLite transaction for an exchange, exactly the user/assistant message pair, ordered exchange sources, and publication receipt. Only the finalizer publishes. Optional history-search indexing happens afterward; failure produces `derived_index` warning without erasing the saved reply.

This is not one transaction spanning the history database and orchestration database. Instead, `OrchestrationRuntime._finish_publication` attaches and completes the journal after canonical publication. If acknowledgement is lost, `lookup` recovers the saved reply and avoids another exchange. If publication fails ambiguously, the run remains interrupted/resumable rather than falsely reporting rollback.

`discard` writes a tombstone only when the publication key is unused. A publication that already won is retained even if discard arrives later. The orchestration trace is not deleted, canonical history is not cleared, and attached exchanges cannot be failed/discarded away. See [data storage](data-storage.md) for the surrounding history authority.

## 8. Commands and versioned event contract

[`src/cli/orchestration.py`](../src/cli/orchestration.py) fixes `GRAPH_VERSION` to `orchestration-v1`:

| Command | Required request fields besides `command` | Result |
| --- | --- | --- |
| `orchestration_start` | `session_id`, `run_id`, `turn_id`, `message` | Snapshot-first event stream |
| `orchestration_resume` | `session_id`, `run_id`, `turn_id` | Snapshot-first recovery stream |
| `orchestration_pending` | `session_id` | `{pending: snapshot-event-or-null}`; more than one pending run fails closed |
| `orchestration_discard` | `session_id`, `run_id`, `turn_id` | `{snapshot: snapshot-event}` after publication arbitration |
| `orchestration_trace` | `session_id`, positive `exchange_id` | `{snapshot: snapshot-event}` after canonical exchange ownership validation |

The TypeScript API converts identities to Tauri camelCase arguments. [`app/src-tauri/src/orchestration/mod.rs`](../app/src-tauri/src/orchestration/mod.rs) translates them into these Python requests and validates the response stream.

[`src/orchestration/wire.json`](../src/orchestration/wire.json) is the closed version-1 schema consumed by the Python and Rust validators. Envelopes carry `version`, `session_id`, `run_id`, `turn_id`, and strictly increasing `sequence`.

| Events | Meaning |
| --- | --- |
| `snapshot` | Initial complete committed public baseline; replaces prior connection state |
| `agent_started`, `agent_completed`, `agent_retried` | Fenced worker state transitions |
| `question_added`, `question_status`, `question_answered`, `question_failed` | Durable public question state |
| `tool_started`, `tool_completed` | Matched projections of a committed logical tool call, not real-time provider start/end telemetry |
| `evidence`, `warning` | Bounded public evidence and allowlisted warning code |
| `final_delta` | Provisional text from a running finalizer only |
| `done` | Canonical reply, exchange ID, sources, chat mode, and index flag |
| `error` | Fixed safe message, error code, and resumability flag |

[`Projection`](../src/orchestration/stream.py) derives wire sequences from checkpoint sequence × 1024 plus an ordinal: ordinary changes below 900, snapshot 900, final delta 1000, terminal 1001. Sequence gaps are expected; sequence reversal/replay is invalid. `StreamValidator` rejects wrong identities, malformed exact fields, duplicate JSON keys, invalid relationships, unmatched tools, non-finalizer deltas, and events after terminal completion. The per-frame cap is 1,048,576 bytes and per-connection count cap is 16,384; the maximum wire sequence is 4,195,328.

[`app/src-tauri/src/orchestration/bridge.rs`](../app/src-tauri/src/orchestration/bridge.rs) launches `python3 -m src.cli` unless the configured executable override is present. Requests are capped at 16,384 bytes. NDJSON reads are bounded; stderr is drained without entering public errors. The default receive/post-EOF wait timeout is 120 seconds, not a guaranteed total-run deadline. On Unix the owned process group includes the CLI and its specialist children and is killed/reaped on transport cleanup; the bounded reader queue has capacity two.

## 9. Source-based checks and extension points

These are existing tests to consult, not results claimed by this documentation pass:

| Boundary | Source/test reference |
| --- | --- |
| CLI dispatch and pull/close semantics | [`tests/test_cli_commands.py`](../tests/test_cli_commands.py), [`tests/test_chat_graph.py`](../tests/test_chat_graph.py), [`tests/test_cli_stream_workflows.py`](../tests/test_cli_stream_workflows.py) |
| Durable revision/replay/migration rules | [`tests/test_orchestration_store.py`](../tests/test_orchestration_store.py) |
| Scoped tools and current intake shape | [`tests/test_orchestration_specialists.py`](../tests/test_orchestration_specialists.py): `test_strict_intake_parsing_rejects_entire_plan_before_questions` |
| Original-query fusion and coverage preservation | [`tests/test_orchestration_fusion.py`](../tests/test_orchestration_fusion.py) and specialist evidence-budget tests |
| Resource ownership, fencing, publication ambiguity | [`tests/test_orchestration_graph.py`](../tests/test_orchestration_graph.py) |
| Wire and recovery projections | [`tests/test_orchestration_wire.py`](../tests/test_orchestration_wire.py), [`tests/test_orchestration_transport.py`](../tests/test_orchestration_transport.py), [`tests/fixtures/orchestration-wire-cases.json`](../tests/fixtures/orchestration-wire-cases.json) |
| Rust bridge and opt-in TypeScript adapter | Inline tests in [`orchestration/bridge.rs`](../app/src-tauri/src/orchestration/bridge.rs), [`app/tests/orchestration-api.test.mjs`](../app/tests/orchestration-api.test.mjs) |

Extensions should preserve the split between pure transitions, bounded scoped tools, owned execution, canonical publication, and public projection. Changes to specialist schemas require corresponding model adapters and fixtures; changes to public state require schema/validator/consumer review. Do not wire durable recovery into Chat merely by mapping every event to transcript text.

For setup and deliberate verification commands, see [getting started](getting-started.md), [configuration](configuration.md), and [operations and testing](operations-and-testing.md). Related context: [frontend](frontend.md), [knowledge graph](knowledge-graph.md), and [PDF viewer](pdf-viewer.md).
