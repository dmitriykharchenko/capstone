# Operations, troubleshooting, and testing

[Index](README.md) · [Setup](getting-started.md) · [Configuration](configuration.md) · [Storage](data-storage.md) · [Evaluation](evaluation.md)

**Source inspection: September 25, 2026.** This is a runbook for the current working tree, not a report that the applications or their suites passed today. The documentation task runs only static documentation checks.

## Start with the operation boundary

Before reproducing a problem, identify:

1. **Surface:** Tauri development window, packaged app or direct Python CLI.
2. **Operation:** browsing, indexing, search, Chat, evaluation, or durable lifecycle API.
3. **Stage:** configuration, extraction, retrieval, planning, composition, judging, publication, transport, or rendering.
4. **Persistence outcome:** no write attempted, verified save, derived-index warning, or unknown final outcome.
5. **Environment:** interpreter/tool versions, presence—not values—of required configuration, capability/readiness state, and deliberate test-data isolation.

Do not reproduce a full paid evaluation merely to learn that a dependency is unavailable. Do not clear a database merely to make an error disappear.

## Troubleshooting table

| Symptom | What to check first | What not to infer/do |
| --- | --- | --- |
| UI loads but calls fail | Correct desktop launcher and Tauri invoke runtime | Vite HTTP success does not prove desktop readiness |
| Tauri reports Python bridge/import failure | `CAPSTONE_PYTHON` resolution, `ragcourse`, declared packages, project root | Do not install into an unrelated interpreter blindly |
| Chat says offline mock | Python credential selection and dotenv loading path; no secret output | A mock answer does not verify provider connectivity or retrieval quality |
| Papers listed but indexing sees another corpus | Tauri shell environment versus Python-only dotenv overrides | Visible filenames do not prove both processes resolved the same root |
| Paper search has no results | Index availability and count; explicit indexing status | Current Python search returns an empty-index reason; it does not lazily index despite a stale handler docstring |
| Chroma dimension/space mismatch | Model, endpoint, mock versus real embedder, and the store's creation configuration | Same filename or model display label is not compatible vector identity |
| Indexing appears stuck on one filename | Stage duration, concurrent work, provider response, process ownership | No new UI event is not sufficient evidence of deadlock |
| One PDF fails indexing | Text extraction, protected/malformed PDF, chunk/batch limits, metadata author/schema validation | Preview success does not prove text extraction/indexing succeeds |
| Graph unavailable/locked | Cozo installation and active owner; indexing may own the store. Index and Reset stop a previous `src.cli` owner automatically | Do not delete graph files or lock files while an owner may be running |
| Reply saved but missing from history search | Canonical SQLite versus derived history index outcome | Do not assume the answer was lost or resubmit immediately |
| Terminal response lost | Read saved state before retrying | Closing stream consumption does not cancel Python or guarantee rollback |
| Eval fails before any chunks | Structured `stage`, category, and HTTP status, if supplied | Zero chunks at planning does not demonstrate a broken index |
| Eval reports payment/authentication failure | Provider account/configuration using trusted tools | More retries cannot supply credit or fix credentials |
| Eval completes with failed rows | Final per-case diagnostics and failed-attempt history | A completed run is not an all-success result or a zero-quality score |
| Blank PDF preview | Valid whole-file bytes, Blob URL, actual webview rendering | An iframe or transferred bytes alone does not establish visible content |
| Package cannot run workflows | Python runtime/packages, project root and corpus configuration | A frontend/native bundle does not establish Python resource distribution |

Follow the linked subsystem chapters before applying a fix. Some error contracts are deliberately fixed/sanitized; unavailable raw provider text is not permission to add it to logs.

## Inspect configuration without exposing it

Use the variable-name reference in [Configuration](configuration.md). A useful diagnostic reports that a required setting is present or absent, which app consumes it, and whether the path/capability validation passed. It does not print `.env`, complete process environments, request headers, provider responses, PDF passwords, or system-prompt values.

The Python health handler reports configured mode and Chroma import availability; it does **not** perform a model request, validate billing or prove matching embeddings.

Legacy Python chat/retrieval logs may contain full research text and conversation context. Restrict access and retention; extract a minimal non-sensitive diagnostic rather than attaching an entire log. New evaluation diagnostics are allowlisted, but that does not sanitize arbitrary files elsewhere in the app.

## Protect canonical data during maintenance

### Backups

Stop the exact owning application before a cold copy or use database-aware consistent backup. Copying one live SQLite file may omit committed WAL data. Separate SQLite/Chroma/Cozo/BM25 files are not automatically one cross-store checkpoint.

Keep original PDFs, canonical history, and evaluation artifacts distinct from rebuildable indexes. Changing providers, corpus identities, or storage formats requires an explicit migration/rebuild plan, not a bulk deletion command.

### Reset semantics

- **Python/Tauri:** [`handle_reset_all_indices`](../src/cli/commands.py) independently attempts paper-vector collection deletion, BM25-file removal, and paper-status clearing. It returns a `reset` map. It does not perform a cross-store transaction or reset the Cozo graph/chat history collection. Some errors are suppressed; returned flags are not stronger evidence than that implementation provides.

Reset is a destructive user operation even when only derived data is removed: rebuilding can cost time and provider usage. Do not run it as a documentation or routine test step.

### Recovery and resume are different mechanisms

Durable orchestration resumes explicit run/attempt state through its separate API. Evaluation retries retain outputs only in memory for the current run. Neither is a universal Resume button for every interrupted workflow.

## Test entrypoints

Choose the smallest relevant test first. The commands here are examples of existing entrypoints, not newly executed results. Run with disposable data and fake providers; inspect suite-specific side effects before widening scope.

| Scope | Working directory | Example command | Notes |
| --- | --- | --- | --- |
| Python eval stage behavior | Repository root | `conda run -n ragcourse python -m pytest -q tests/test_evaluation_stages.py` | Injected models and per-stage fault/retry coverage |
| Python durable state | Repository root | `conda run -n ragcourse python -m pytest -q tests/test_orchestration_store.py` | Checkpoint, receipt, and persistence contracts |
| Tauri state/routing | `app/` | `node --test tests/router.test.mjs tests/search-collections.test.mjs` | Node tests use Vite SSR; no full desktop startup |
| Tauri static frontend | `app/` | `rtk npm run check` | Svelte/type checks; not visual acceptance |
| Tauri Rust | `app/src-tauri/` | `rtk cargo test --locked` | Native/bridge regressions; review process fixtures |
| Renderer build | `app/` | `rtk npm run build` | Writes renderer output, not Python/packaging acceptance |

`pytest` is used by the Python tests but is **not listed in `environment.yml`**. The manifest lists runtime packages. Check whether your chosen development environment supplies pytest before running; do not describe the runtime manifest as a complete reproducible test-tool environment. No dependency was added by this documentation pass.

There is no `test` script in `app/package.json`; use selected Node files instead.

`--offline` prevents Cargo dependency fetching, not network activity by a test binary. Tests can spawn subprocesses and create disposable stores; they are not purely static analysis.

## Regression map

| Change area | Existing checks to start from |
| --- | --- |
| Chunks/embeddings/indexing | [embeddings](../tests/test_embeddings.py), [batching](../tests/test_paper_indexer_batching.py), [stream](../tests/test_cli_index_stream.py) |
| Search/ranking/history | [paper search](../tests/test_paper_search.py), [history index](../tests/test_chat_history_index.py), [SQLite](../tests/test_sqlite_history.py) |
| Chat pipeline/stream | [multihop](../tests/test_agent_loop_multihop.py), [graph wrapper](../tests/test_chat_graph.py) |
| Durable orchestration | [store](../tests/test_orchestration_store.py), [specialists](../tests/test_orchestration_specialists.py), [runtime/graph](../tests/test_orchestration_graph.py), [transport](../tests/test_orchestration_transport.py) |
| Evaluation | [runner](../tests/test_evaluation_runner.py), [planner retry](../tests/test_evaluation_planner_retry.py), [stages](../tests/test_evaluation_stages.py), [diagnostics](../tests/test_evaluation_diagnostics.py) |
| Dataset changes | [question integrity](../tests/test_eval_questions.py); source-backed quote checks need the corpus and `pypdf` |
| Frontend | [router](../app/tests/router.test.mjs), [collections](../app/tests/search-collections.test.mjs), [eval state](../app/tests/evaluation-state.test.mjs) |
| Desktop process/PDF boundary | [bridge](../app/src-tauri/src/bridge.rs), [command tests](../app/src-tauri/src/lib.rs), [paper state](../app/tests/papers-state.test.mjs) |

Update tests when changing behavior; do not merely rerun a test that cannot exercise the changed boundary. Contract changes need producer/consumer parity checks. Cancellation tests need actual resource ownership and late-terminal cases. PDF mocks do not replace native rendering, selection, password, or memory acceptance.

## Quality evaluation is not the unit suite

[`tests/eval_questions.json`](../tests/eval_questions.json) and [`tests/judge.md`](../tests/judge.md) are runtime evaluation resources. A live run can make multiple model/embedding calls per case, including judge calls and retry rounds. Do not classify it as a free offline test merely because its inputs live under `tests/`.

For an intentional quality comparison, record the app variant, dataset identity, corpus/index state, provider/model configuration without credentials, final succeeded/failed counts, scores over successful cases, and returned artifact location. A run dominated by provider failures is not a meaningful retrieval-quality comparison.

## Documentation maintenance and validation

This guide is a snapshot of source, not a rolling runtime monitor. When changing the app:

1. Update the relevant domain chapter and any configuration/setup instructions affected.
2. Keep current behavior separate from proposed work and historical requests.
3. Check relative Markdown/source paths, heading anchors, command names, and defaults against the code.
4. Check whitespace with `rtk git diff --check -- documentation`; newly untracked files also need a direct whitespace scan because Git's diff check does not cover them.
5. Keep secrets, local machine paths, user logs, and generated run artifacts out of the docs.
6. Report exactly which checks ran; never turn test-file existence or past history into a current pass claim.

No application tests, provider calls, database changes, native/browser launches, corpus extraction, or packaging runs are performed merely to write this documentation. Those are separate validation activities with their own data, cost, and environment requirements.
