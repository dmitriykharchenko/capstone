# Getting started

[Index](README.md) · [Architecture](architecture.md) · [Configuration](configuration.md) · [Operations and testing](operations-and-testing.md)

**Source inspection: September 25, 2026.** The commands below describe checked-in/current scripts; they were not executed for this documentation task. Installation, app launch, indexing, evaluation, and packaging have different side effects. Do not treat a setup guide as permission to run paid workloads or reset existing data.

## Desktop launch boundary

Launch the Tauri/Python desktop application from `app/`. A Vite page loading successfully is **not** proof that its desktop bridge is available: a plain browser lacks the native invoke/channel runtime.

## Prerequisites

| Tool/resource | Requirement |
| --- | --- |
| Node and npm | Frontend and Tauri CLI; locked Vite 8.2.2 requires `^20.19.0` or `>=22.12.0` |
| Rust/toolchain | Tauri 2 and platform-native build prerequisites; manifest declares Rust 1.77 |
| Conda/Python | `ragcourse` environment, Python 3.11, declared Python dependencies |
| `rtk` | Optional output wrapper used in this guide; omit the prefix if unavailable |
| Local PDFs | Default `ResearchPapers/`, or an explicit paper root |
| Provider account | Needed for genuine model/embedding behavior |

The Tauri manifest's minimum is not a guarantee that every newly resolved dependency works with that compiler. Use the committed locks and an appropriate current toolchain. This repository does not establish fully validated cross-platform deployment through documentation alone.

Sources: [Python environment](../environment.yml), [Tauri package](../app/package.json), [Tauri Cargo manifest](../app/src-tauri/Cargo.toml), and the application's lockfiles.

## Set up Tauri/Python

1. From the repository root, create the declared environment with `conda env create -f environment.yml` if it does not exist. If it already exists, review an environment update rather than creating another copy. This installs dependencies; it is not a read-only check.
2. Install the locked frontend dependencies with `rtk npm ci` from `app/`. This replaces that directory's installed dependency tree; do not use it while a running app depends on that tree.
3. Configure the provider and data locations using [Configuration](configuration.md). Keep credentials in a protected existing environment/configuration mechanism, never in command arguments or committed files.
4. From `app/`, launch the desktop application with `rtk npm run tauri -- dev`.

The package's `tauri` script uses an existing `CAPSTONE_PYTHON` value or uses `$CONDA_PREFIX/bin/python` of the Conda `ragcourse` environment (a prefix lookup, so pyenv shims cannot substitute another `python`). If that lookup produces no usable value, the Rust bridge falls back to `python3`. At launch the app runs one Python `health` request and prints `python backend ready: ...` or `python backend failed to start: ...` in the terminal.

[`tauri.conf.json`](../app/src-tauri/tauri.conf.json) starts `npm run dev`, expects port 1420, and points the native webview at that server. [`vite.config.ts`](../app/vite.config.ts) uses a strict port: an occupied port is a failure, not automatic selection of another one.

### Other Tauri scripts

| Command, from `app/` | What it does |
| --- | --- |
| `rtk npm run dev` | Starts Vite only; not the native app |
| `rtk npm run check` | Runs Svelte/TypeScript checks |
| `rtk npm run build` | Builds frontend assets only |
| `rtk npm run preview` | Serves frontend build output without a Tauri bridge |
| `rtk npm run tauri -- build` | Invokes Tauri's build path; not proof that Python, corpus, and runtime resources are distributable |

The Python path currently resolves source and data from the project layout. A frontend build or Tauri bundle alone does not establish a self-contained Python-free distribution.

## First-use checklist

1. Confirm the intended desktop app is running, not just its development server.
2. Open **Papers** and confirm the expected library. If empty/unavailable, check the configured paper root before indexing anything.
3. Open a known PDF through the native/webview byte-preview path. Reading does not perform OCR or create a searchable index.
4. Before **Index papers**, confirm the corpus, writable stores, embedding model/space, provider access, and expected cost. Indexing also extracts graph metadata with the configured model.
5. Inspect per-paper outcomes rather than only the overall completion count. Empty text, failed metadata, or unavailable dependencies require different remedies.
6. Submit a paper search, then a grounded Chat question after indexing. Mock responses are development behavior, not quality verification.
7. Use **History** to search saved completed exchanges and return to their session. Index-search failure and canonical-save failure are different conditions.
8. Run **RAG Evals** only after checking provider/cost and data readiness. Begin with a small deliberate case set rather than assuming a full corpus evaluation is a harmless startup check.

The [Frontend](frontend.md), [Indexing](paper-indexing.md), and [Evaluation](evaluation.md) chapters explain each workflow's states and limits.

## When startup succeeds but a feature fails

Read [Operations and testing](operations-and-testing.md) before changing data. Common causes include the wrong Python interpreter, missing Chroma/Cozo runtime, a provider key selecting the wrong endpoint, or an incompatible embedding store. None is safely resolved by deleting every database.
