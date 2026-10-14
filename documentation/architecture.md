# Application architecture

[Index](README.md) · [Setup](getting-started.md) · [Configuration](configuration.md) · [Storage](data-storage.md)

**Source inspection: September 25, 2026.** This chapter describes the working tree, including uncommitted implementation. It separates executable call paths from capabilities that are currently exposed only as lower-level APIs.

## Repository boundaries

| Location | Responsibility |
| --- | --- |
| [`app/src`](../app/src/) | Tauri application's Svelte components, Themis state, sagas, and API adapters |
| [`app/src-tauri/src`](../app/src-tauri/src/) | Native commands, local PDF access, Python process bridge, and transport validation |
| [`src/storage`](../src/storage/) | SQLite history, Chroma/BM25 indexes, embedded graph store |
| [`src/indexing`](../src/indexing/) | PDF conversion, chunking, fact extraction, paper indexer |
| [`src/retrieval`](../src/retrieval/) | Paper search and hybrid ranking |
| [`src/chat`](../src/chat/) | Adaptive multihop agent loop and chat graph |
| [`src/evaluation`](../src/evaluation/) | Evaluation runner, judge, metrics, diagnostics, result artifacts |
| [`src/orchestration`](../src/orchestration/) | Durable orchestration store, graph, workers, wire contract |
| [`src/cli`](../src/cli/) | Python command dispatch, dependency construction, and stream publication |
| [`tests`](../tests/) | Python regressions, evaluation questions, judge resource, and evaluation artifacts |
| [`ResearchPapers`](../ResearchPapers/) | Local source corpus; not generated application output |
| [`docs`](../docs/README.md) | Workspace history and decision record |
| [`documentation`](README.md) | Current behavior and operational reference |

The course checkpoint starter scripts are not the desktop application's production entrypoints. There is no current `src/server` REST service to launch; the Python interface is the CLI bridge.

## Tauri/Python execution path

The renderer calls [`api.ts`](../app/src/lib/api.ts). Unary operations use Tauri `invoke`; streaming operations create a Tauri `Channel` and deliver typed events to sagas.

[`lib.rs`](../app/src-tauri/src/lib.rs) registers desktop commands, which live in feature modules: [`papers.rs`](../app/src-tauri/src/papers.rs), [`chat.rs`](../app/src-tauri/src/chat.rs), [`indexing.rs`](../app/src-tauri/src/indexing.rs), [`evaluation/`](../app/src-tauri/src/evaluation/) and [`orchestration/`](../app/src-tauri/src/orchestration/). The backend handles paper filesystem reads natively and delegates model, search, indexing, and evaluation work to Python. [`bridge.rs`](../app/src-tauri/src/bridge.rs) runs a fresh interpreter with `-m src.cli`, sends one JSON request through stdin, and consumes either a JSON result or newline-delimited JSON events.

The process working directory is the resolved project root. `CAPSTONE_PYTHON` selects the interpreter; `CAPSTONE_PROJECT_ROOT` overrides the shell's repository-root resolution. The ordinary stream bridge drains stderr concurrently with stdout and owns child cleanup. The specialized [orchestration bridge](../app/src-tauri/src/orchestration/bridge.rs) has additional lifecycle/contract handling; do not assume all bridge generations have identical recovery behavior.

Python startup in [`__main__.py`](../src/cli/__main__.py) loads configuration, routes durable-orchestration requests separately, and chooses unary or streaming dispatch. Unary commands go straight to `dispatch` in [`commands.py`](../src/cli/commands.py); chat streams come from the chat graph, and indexing/evaluation streams use the pull-driven graphs in [`stream_workflows.py`](../src/cli/stream_workflows.py). Domain logic remains in its own modules.

### Command families

| User/API operation | Desktop entrypoint | Domain path |
| --- | --- | --- |
| List/open paper | `list_papers`, `read_pdf` | Rust `PaperRepository`; not Python PDF serving |
| Chat | `chat_stream` | Python `chat`, `AgentLoop`, ordinary `chat_graph` |
| Sessions/messages | `list_sessions`, `get_messages` | Python `sessions` / `messages`, canonical SQLite |
| History search | `search` | Derived exchange index plus SQLite hydration |
| Paper search | `search_papers` | `PaperSearchIndex`, grouped paper matches |
| Indexing | `index_papers_stream` | `PaperIndexer`, statuses, BM25 refresh |
| Status/reset | `paper_statuses`, `bm25_status`, `reset_all_indices` | Explicit derived-state operations |
| Evaluation | `evaluate_stream` | Python `evaluate`, stage executor, judge, artifact publication |
| Durable run lifecycle | `orchestration_start`, `orchestration_resume`, `orchestration_pending`, `orchestration_discard`, `orchestration_trace` | Separate orchestration CLI, state store, workers, and publication arbitration |

Names differ across layers: for example, desktop `list_sessions` sends CLI `sessions`. Use the adapter rather than guessing names from the UI.

## Data flow: paper to answer

1. **Browse:** the native shell lists safe local PDF identities; this does not itself index documents.
2. **Index explicitly:** extract pages, split text, compute embeddings, extract graph metadata, and publish derived records. Python writes have no cross-store publication transaction.
3. **Submit Chat:** the route/session saga sends the original question to the ordinary adaptive pipeline.
4. **Plan and retrieve:** generate bounded subquestions, retrieve hybrid-ranked chunks, and generate evidence-grounded subanswers.
5. **Review coverage:** optionally retrieve additional evidence within the pipeline's follow-up budget.
6. **Compose:** stream only the final answer as assistant text; structured progress is separate.
7. **Save:** commit the original user question and final assistant answer with source metadata to canonical history; update derived history search afterward according to that path's failure policy.
8. **Reload/search:** hydrate complete saved exchanges from canonical SQLite, not from vector-index snippets.

This describes ordinary Chat. Durable orchestration has separate Intake/question-handler/finalizer scheduling, original-query evidence fusion, receipts, and restart control. It is **not** selected by the current Tauri Chat saga; see [Orchestration](orchestration.md).

## Ownership and consistency

| Concern | Authority | Boundary |
| --- | --- | --- |
| Saved conversation | Canonical SQLite exchange/messages/source records | Derived indexing failure does not prove the conversation was lost |
| Python paper retrieval | Chroma plus lexical cache | Python's per-indexer lock is not a transaction spanning Cozo and Chroma |
| Graph relationships | Cozo data derived from indexed PDFs | Stored for indexing; no desktop graph view |
| Evaluation results | Separate timestamped JSON artifacts | Evaluation does not append answers to normal Chat history |
| Durable orchestration | Separate trace/checkpoint database plus canonical publication arbitration | Checkpoint durability does not mean ordinary Chat has Resume/Discard UI |
| UI search cache | In-memory query-keyed collections | Not durable search-result storage |
| PDF render resources | Component/reader lifecycle | Passwords, document handles, and canvas objects are not application-state entities |

## Failure boundaries

- A paper can fail extraction, embedding, or metadata validation without stopping all other directory papers.
- Empty/unavailable retrieval is not a grounded success on the strict ordinary Chat path.
- A provider truncation is different from a completed answer; provider-imposed output limits can still exist without an application cap.
- Evaluation case failures can be retried by stage; artifact publication/transport failures are different terminal concerns.
- Closing a saga channel does not cancel an ordinary Tauri invocation; missing completion is not proof that no write occurred.
- Legacy Python command errors and logs are not uniformly sanitized like the newer evaluation/orchestration boundaries. Do not infer a repository-wide secret-safe logging guarantee.

See [Operations](operations-and-testing.md) for failure triage without deleting data or repeating paid work.

## Security boundaries

The Tauri [configuration](../app/src-tauri/tauri.conf.json) currently has `csp: null`. Native paper access validates identities and containment; this is not permission for arbitrary file access.

The app depends on correct provider/data configuration and trustworthy launcher access. Local documents are untrusted model evidence; prompt delimiting and typed outputs reduce risk but are not a proof of immunity to prompt injection. Documents and conversations can contain sensitive content.

## Extension strategy

Change the smallest domain module, update its tests, then follow the entire consumer boundary when its public contract changes. For stream changes this usually includes Python producer, Tauri validator, TypeScript type/parser, reducer/saga, and component. For schema changes include reopen/migration/recovery behavior.
