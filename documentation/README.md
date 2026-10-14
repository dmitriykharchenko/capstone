# Research assistant: current application documentation

This guide describes the **current source tree as inspected on September 25, 2026**. It explains how to run, use, understand, and extend the application. This directory is the present-tense technical reference.

The inspected tree includes uncommitted and untracked implementation. These documents describe those files as well as committed code. They are not a claim that the current behavior is reproducible from the Git commit alone, that every feature is enabled locally, or that a packaged release has passed acceptance.

## What the application does

Research assistant is a desktop application for a local PDF research library. It can index extracted paper text, search documents and saved conversations, generate paper-grounded answers, display author–paper–topic relationships, and run question-based RAG evaluations.

This guide covers the **Tauri/Python application**:

| Application | Frontend and desktop shell | Domain backend | Persistent retrieval |
| --- | --- | --- | --- |
| Tauri/Python | Svelte in `app/`, Tauri Rust commands | Python JSON/NDJSON CLI under `src/` | Chroma, persisted BM25, embedded CozoDB; SQLite chat |
The Rust desktop shell handles local browsing and bridges application workflows to Python; it is not an independent Rust RAG backend.

## Documentation map

| Chapter | Read it for |
| --- | --- |
| [Architecture](architecture.md) | Process boundaries, data flows, ownership, command paths, and integration status |
| [Getting started](getting-started.md) | Prerequisites, desktop launch commands, first-use checklist, and packaging limits |
| [Configuration](configuration.md) | Environment variable reference, provider selection, and shell/CLI path rules |
| [Frontend](frontend.md) | Routes, screens, state, sagas, and streaming presentation |
| [Paper indexing](paper-indexing.md) | Extraction, chunking, embeddings, provenance, progress, batching, and reset |
| [Retrieval and Chat](retrieval-and-chat.md) | Ranking, multihop answering, sources, sessions, history search, and failure policies |
| [Data storage](data-storage.md) | Canonical and derived records, files, schema ownership, persistence, and recovery |
| [Knowledge graph](knowledge-graph.md) | Metadata extraction, graph queries, explorer filtering, and layout |
| [Native desktop](native-desktop.md) | Tauri commands, Python process bridge, and transport boundaries |
| [PDF viewer](pdf-viewer.md) | Whole-file native access, Blob URLs, and webview iframe presentation |
| [Orchestration](orchestration.md) | Ephemeral LangGraph execution and separate durable agents, tools, checkpoints, and APIs |
| [Evaluation](evaluation.md) | Question/judge schemas, stages, retries, diagnostics, UI, and result artifacts |
| [Operations and testing](operations-and-testing.md) | Safe troubleshooting, validation commands, regression map, and maintenance boundaries |

All repository source links are relative to this directory. Documents link to other chapters instead of reproducing the same subsystem's full specification.

## Suggested reading paths

- **Use the app:** Getting started → Configuration → Frontend → Retrieval and Chat.
- **Understand RAG:** Architecture → Paper indexing → Retrieval and Chat → Evaluation.
- **Work on persistence:** Data storage → Native desktop → Orchestration.
- **Work on reading/visualization:** Frontend → PDF viewer → Knowledge graph.
- **Diagnose a failure:** Operations and testing → the relevant subsystem chapter and its source/test references.

## Current integration boundaries

| Surface | What is present | Qualification |
| --- | --- | --- |
| Chat, History, Papers, RAG Evals | Routes and implementations under `app/` | Backend availability still depends on configuration and dependencies |
| Ordinary Tauri Chat | Adaptive multihop via `api.chatStream` | Does not call durable orchestration start/resume |
| Durable orchestration | Python state, tools, runtime, publication, CLI, Tauri/TypeScript APIs | Dedicated trace/recovery UI is not wired into ordinary Chat |
| Aspect-based durable Intake | Current source contains part/question planning | Source presence does not establish completed integration or quality acceptance |
| Python evaluation retries | Immediate planner retries plus two deferred failed-stage rounds and structured diagnostics | Run-local state, not durable app-restart recovery |
| Provider configuration | Shell environment and Python dotenv loading | No general in-app settings screen |
| PDF preview | Native whole-file read followed by an iframe Blob URL | Actual rendering depends on the host webview |
| Distribution | Tauri build command | A bundle alone does not establish distribution of Python and corpus resources |

## How to interpret this guide

- **Implemented** means code exists and its call path was inspected.
- **Connected** means the relevant current consumer invokes it, not merely that an API is exported.
- **Available** can depend on dependencies, configuration, data readiness, and provider access.
- **Verified now** is limited to the documentation checks reported for this writing task. No application suite, live model evaluation, native visual acceptance, or distribution test was rerun to write these chapters.

Source links identify definitions and entrypoints; existing test links identify where future changes should be checked. They are not claims that those tests currently pass.

## Important operating rules

1. Keep original PDFs and canonical conversation history separate from rebuildable indexes.
2. Treat indexing, embedding search, Chat, and evaluation as potentially paid operations when providers are configured.
3. Do not use a production store as a disposable test fixture or reset indexes simply to dismiss an error.
4. Do not print or publish credentials, PDF passwords, provider bodies, or full private retrieval logs.
5. Closing a UI stream does not guarantee that the Python subprocess stopped or rolled back writes.
6. Verify both the Python producer and desktop consumer when changing their shared transport contract.

For implementation changes, update the relevant chapter and regression tests together. Preserve the historical account in `docs/` rather than rewriting it to look like the latest architecture.
