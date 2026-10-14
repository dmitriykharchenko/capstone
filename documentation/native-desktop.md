# Tauri desktop host reference

[Index](README.md) · [Architecture](architecture.md) · [Configuration](configuration.md) · [PDF viewer](pdf-viewer.md) · [Operations](operations-and-testing.md)

**Source inspection: September 25, 2026.** The Rust code under `app/src-tauri/` is a desktop host and Python process bridge, not an independent Rust RAG service. Source inspection does not establish runtime or package acceptance.

## Process boundaries

[App.svelte](../app/src/App.svelte) mounts the Svelte/Themis frontend and starts app-level sagas. [api.ts](../app/src/lib/api.ts) uses Tauri `invoke` and `Channel`. [lib.rs](../app/src-tauri/src/lib.rs) registers native paper operations and Python-backed domain commands; `run_blocking` moves blocking work to Tauri's pool while the invocation remains pending.

`project_root` honors nonblank `CAPSTONE_PROJECT_ROOT`, otherwise resolves two levels above the Cargo manifest directory. Paper access uses shell `CAPSTONE_PAPERS_DIR` or project `ResearchPapers/`. Python CLI dotenv loading happens separately, so a Python-only paper override may disagree with the desktop listing. See [configuration](configuration.md).

## Command map

| Desktop command | Owner / purpose |
| --- | --- |
| `list_papers`, `read_pdf` | [PaperRepository](../app/src-tauri/src/papers.rs): native filesystem browsing and whole-file bytes |
| `chat_stream` | [chat.rs](../app/src-tauri/src/chat.rs): Python Chat request; ordinary adaptive answer generation |
| `list_sessions`, `get_messages`, `search` | [chat.rs](../app/src-tauri/src/chat.rs): Python `sessions` / `messages` history reads and history search |
| `search_papers` | [papers.rs](../app/src-tauri/src/papers.rs): Python paper search; no implicit indexing |
| `index_papers_stream` | [indexing.rs](../app/src-tauri/src/indexing.rs): Python indexing with streamed progress |
| `paper_statuses`, `bm25_status`, `reset_all_indices` | [indexing.rs](../app/src-tauri/src/indexing.rs): Python outcome/lexical diagnostics and explicit best-effort reset |
| `evaluate_stream` | Python evaluation; typed requests and sanitized [evaluation DTOs](../app/src-tauri/src/evaluation/mod.rs) |
| `orchestration_start`, `orchestration_resume`, `orchestration_pending`, `orchestration_discard`, `orchestration_trace` | Separate [orchestration bridge](../app/src-tauri/src/orchestration/mod.rs); not the mounted Chat workflow |

Graph depth defaults to one and permits 0–5. Evaluation validates nonempty cases before spawning work; nested result fields remain snake_case while event/envelope fields become camelCase. See [graph](knowledge-graph.md), [evaluation](evaluation.md) and [orchestration](orchestration.md) for their exact contracts.

## Python bridge and stream lifetime

[bridge.rs](../app/src-tauri/src/bridge.rs) chooses nonblank `CAPSTONE_PYTHON`, else `python3`. It launches that executable with `-m src.cli` in the resolved root and sends one JSON request on stdin. Unary `call` reads one JSON response; `call_streaming` parses newline JSON events from stdout. The CLI does not require the optional REST server.

Streaming stderr is drained concurrently into a bounded 64 KiB tail so a full stderr pipe cannot deadlock stdout progress. A nonzero exit after an `ok: false` line returns that line's `error` message; other nonzero exits/read errors may include the stderr diagnostic. `StreamingChild` kills/reaps a still-running child on drop and joins its drain thread. This is ownership cleanup, not a universal secret-safe error contract: legacy errors can contain details. New Chat/evaluation/orchestration boundaries have more specific validation/sanitization.

Ordinary frontend Chat/index/evaluation event-channel unsubscribe functions are no-ops. Closing consumption, changing session or unmounting a page is therefore not proof that Python stopped or writes rolled back. Evaluation publishes its artifact before completion delivery; a lost terminal frame does not prove no artifact exists. Durable orchestration has separate attempt/receipt ownership and must not be inferred from ordinary Chat.

## Security and distribution limits

[tauri.conf.json](../app/src-tauri/tauri.conf.json) currently sets `csp: null`; [capabilities](../app/src-tauri/capabilities/default.json) define the configured desktop grants. Paper IDs are restricted and resolved paths must remain under the canonical corpus root. Those checks do not grant arbitrary file access or pin files against subsequent external edits.

Launch from `app/` with the configured Python environment using [setup](getting-started.md). Renderer builds alone do not establish native bridge readiness. A Tauri bundle alone does not establish packaging of the Python runtime, dependencies, corpus and configured data paths.

## Verification map

Existing bridge/stream tests live in [bridge.rs](../app/src-tauri/src/bridge.rs), [chat.rs](../app/src-tauri/src/chat.rs), [indexing.rs](../app/src-tauri/src/indexing.rs), [evaluation/](../app/src-tauri/src/evaluation/), [orchestration/](../app/src-tauri/src/orchestration/), and [desktop tests](../app/tests/). Use focused fake-process and state/transport checks; actual visual acceptance is separate. No application/provider/store tests were run to write this reference.
