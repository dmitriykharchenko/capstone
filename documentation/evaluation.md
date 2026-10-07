# RAG evaluation reference

[Documentation index](README.md) · [Architecture](architecture.md) · [Retrieval and chat](retrieval-and-chat.md) · [Orchestration](orchestration.md)

**Source inspection date: 2026-09-25.** This guide describes the current working tree, including uncommitted implementation. It does not assert clean-checkout parity or runtime acceptance. No app, provider, live store, application test, or installation is run for this documentation.

## 1. What evaluation measures and which path runs

An evaluation case supplies a question, grading notes, and reference source filenames. The runner produces an answer using the indexed paper corpus, gives the retrieved chunks and answer to an LLM judge, and reports seven structured criteria. This is not a deterministic ground-truth checker, an embedding-only similarity metric, or an automatic pass/fail gate for the application.

| Surface | Actual implementation | Important difference |
| --- | --- | --- |
| Tauri application in `app/` | Svelte upload UI → `api.runEvaluation` → `evaluate_stream` → Python `src.cli` | Python stage checkpoints, two deferred retry rounds, structured failure diagnostics, artifact schema v2 |
| Python library | `EvaluationRunner.run`, `run_file`, `start_case`, `resume_case` | Returns results; library calls alone do not publish an artifact |

The UI labels the sole retrieval choice **Multihop Hybrid**. Its identifier is `multihop-hybrid`, while the wire value is `hybrid`; see `EVALUATION_RETRIEVAL_TYPE` and `EVALUATION_WIRE_RETRIEVAL_TYPE` in [evaluation-types.ts](../app/src/lib/evaluation-types.ts).

Evaluation is stateless with respect to conversations: it uses no session history as model input and writes no chat exchanges or history vectors. It reads retrieval stores, may call providers, and publishes a completed-run artifact through the CLI transport wrapper. Treat a live run as potentially paid, not a health check.

## 2. Dataset and upload contract

### Case shape

[`EvaluationCase`, `validate_test_cases`, `parse_test_cases`, and `load_test_cases`](../src/evaluation_runner.py) define the Python input contract:

| Field | Requirement | Use |
| --- | --- | --- |
| `question` | Nonempty string after a whitespace check | Original question sent to planning and answer generation |
| `grading_notes` | Nonempty string | Reference answer/rubric supplied to the judge |
| `sources` | Array of nonempty strings; an empty array is valid | Ground-truth relevant sources for judging, not a retrieval filter |
| `performance_budget` | Optional nonempty object of numeric `PerformanceBudget` limits | Per-case overrides of the runner's performance limits (see section 6) |

The entire top-level value must be a nonempty array and is validated before execution. `parse_test_cases` accepts text/bytes or parsed values; `load_test_cases` reads UTF-8. The loader permits other extra keys but ignores them; an invalid `performance_budget` (unknown limit, null, negative, non-integer count) rejects the whole input. It does not require PDF basenames, check source existence or impose a case-count limit.

Shape example, using a fictitious filename rather than claiming corpus membership:

<augment_code_snippet mode="EXCERPT">
````json
[
  {"question":"What does the paper claim?","grading_notes":"Identify the stated claim and its limitation.","sources":["example.pdf"]}
]
````
</augment_code_snippet>

[`parseEvaluationTestCases`](../app/src/lib/evaluation-upload.ts) performs the equivalent nonempty-array/string/source-list checks in the Tauri renderer. [`RagEvals.svelte`](../app/src/components/RagEvals.svelte) checks the `.json` extension and file-read errors before parsing, then immediately dispatches `runEvaluation(parsed.testCases)`. Uploading a valid test file starts evaluation; there is no separate preview-and-confirm stage in this component. The file input and retrieval selector are disabled while a run is active.

### Current checked-in datasets

| Resource | Current content and consumer |
| --- | --- |
| [`tests/eval_questions.json`](../tests/eval_questions.json) | 60 cases; Python repository dataset and a file a user may upload |
| [`tests/judge.md`](../tests/judge.md) | Python default judge template, read by `_load_judge_template` |
| [`tests/eval_questions_performance.json`](../tests/eval_questions_performance.json) | 10 performance, efficiency, and reliability cases; performance and efficiency cases carry `performance_budget` limits |

The visible upload workflow supplies its own cases; the checked-in dataset is not automatically started.

The 60-case dataset contains untagged cases and category markers inside `grading_notes`, keeping the three-field schema. Tagged families cover enumeration, nonexistent-source questions, partially supported lists, multi-hop reasoning, contradictions, precise quotes, and boundary/precision questions. Enumeration cases span 2, 4, and 11 unique works; multi-hop cases explicitly specify 2, 3, and 4 hops. Source filenames and aliases are ground-truth annotations, not permission for the retriever to manufacture evidence for an absent title.

[`tests/test_eval_questions.py`](../tests/test_eval_questions.py) checks `test_curated_dataset_stays_at_sixty_questions`, production-loader round trips, exact three-field dataset shape, normalized question uniqueness, unique source lists, PDF basenames, category coverage, enumeration cardinality, hop annotations, and verbatim quotes. Source existence checks depend on local corpus availability; quote checks explicitly skip when the local PDFs or `pypdf` are absent. These tests are source references, not a statement that PDF checks ran during this documentation pass.

## 3. Python construction and retrieval semantics

[`build_evaluation_runner`](../src/cli/models.py) obtains `build_paper_search_index(config)`, fails if unavailable, and constructs `EvaluationRunner`. With configured provider credentials, the runner lazily creates its chat model using `ChatOpenAIConfig`; otherwise the factory uses `MockChatModel`. A configured system prompt overrides the default. No credential values belong in evaluation input or these docs; see [configuration](configuration.md).

`EvaluationRunner` accepts an injected retriever with `retrieve_chunks`, an optional model with `invoke`, a configuration object, judge-template path, system prompt, and optional `PlannerRetryPolicy`. Its lazily constructed evaluation model explicitly has SDK `max_retries=0`, so SDK retries do not multiply the runner's own planner budget. An externally injected model owns its own retry policy.

### Strategy labels are not interchangeable retrieval experiments

Python `EvaluationRunner.run(payload, strategy)` and the CLI validate `vector`, `bm25`, or `hybrid` because those values remain in `EVALUATION_RETRIEVAL_STRATEGIES`. However, the current stage pipeline calls `_retrieve_multihop`, which calls `retrieve_chunks(subquery, n_results=5)` **without** a `strategy` keyword. The standard `PaperSearchIndex` therefore uses its ordinary chat hybrid path; the chosen runner strategy is recorded as metadata, not forwarded as a retriever switch.

[`PaperSearchIndex.retrieve_evaluation_chunks(query, strategy, n_results=5)`](../src/paper_search.py) remains a separate explicit strategy API. Supplying `strategy` to `retrieve_chunks` delegates there. It supports vector, BM25 and full-corpus hybrid ranking, but the standard multihop evaluator does not select that path. A low-level `vector` label is not proof of vector-only retrieval. The UI adapter sends `hybrid`.

Reference sources and grading notes are passed to the judge, not into planner/retrieval/subanswer prompts as an answer shortcut. See [retrieval and chat](retrieval-and-chat.md), [paper indexing](paper-indexing.md), and [knowledge graph](knowledge-graph.md) for indexed evidence and provenance.

## 4. Python stage pipeline and checkpoint boundaries

[`EvaluationStages`](../src/evaluation_stages.py) reuses bounded multihop helpers from [`src/agent_loop.py`](../src/agent_loop.py) while holding an evaluation-only in-memory cursor. It is not the durable SQLite specialist runtime from [orchestration](orchestration.md).

| Stage | Work | Checkpointed output before advancing |
| --- | --- | --- |
| `planner` | `_plan_multihop` with empty history and evaluation planner retry policy | Validated ordered `subqueries`, `initial_count` |
| `retrieval` | `_retrieve_multihop` for the current initial hop, excluding earlier evidence by `(paper_id, chunk_index)` and normalized text | Context, unique hop chunks, and reuse status |
| `subanswer` | `_answer_multihop` from original question, subquery, and retrieved context | Validated subanswer; next hop cursor |
| `coverage` | `_review_multihop_coverage` once after initial hops | Validated follow-ups or decision to compose |
| `followup_retrieval` | Retrieval for a coverage follow-up | Its context/chunks |
| `followup_subanswer` | Answer for that follow-up | Subanswer and cursor |
| `composer` | Aggregate/deduplicate chunks; compose from ordered subanswers with empty history | Final answer and ranked aggregate chunks |
| `judge` | Render the reference/rubric prompt and validate seven scores | Final result; cursor becomes `done` |

Current constants are 2–5 initial subqueries, at most 2 follow-ups, at most 7 total hops, 5 chunks per hop, and at most 35 aggregate source records. Retrieval requests five plus the number of earlier chunks, then uses the first five unseen excerpts. A search with no unseen excerpts answers from its own deduplicated top excerpts and is recorded as reused; that is the only permitted repeated evidence. Retrieval with no chunks is a stage failure; it does not generate an unsupported answer. Coverage review is a single bounded pass, not an unlimited search loop.

`EvaluationStages.advance` executes exactly one non-judge stage. Successful validated outputs are committed to the run-local object before advancing. Failed calls leave the cursor and prior successful work intact. The object retains public query/context/chunk/answer state, not provider handles, raw exception objects, rejected model output, or chat stores.

`EvaluationCaseState` in [`src/evaluation_runner.py`](../src/evaluation_runner.py) adds one-based case index, strategy, judge template, current result, last retry round, stage/hop attempt counters, and safe failure history. `start_case` allocates this state without provider/retrieval work. `resume_case` advances until completion or one failed stage.

## 5. Two retry layers, with different rules

### Deferred case-stage rounds

`EvaluationRunner.run` creates one state per input and processes round 0 for every case in input order before beginning any deferred retry. Rounds **1 and 2** each operate only on cases still unfinished at the beginning of that round, again in original input order. There is no round 3.

- A retry resumes the exact failed stage and one-based hop. A subanswer failure does not repeat retrieval; a judge failure does not repeat planning or answer generation.
- If the retry advances and then fails later, the next round resumes the new failing stage.
- `resume_case` requires rounds to advance consecutively from 0 through 2; invalid/out-of-order calls are rejected without work. A done state returns its stored result.
- Each case contributes exactly one final result, while failed attempts remain in `attempt_history`.
- These checkpoints survive only within the running process and call chain. Restarting the process or starting a new run does not reload a stage checkpoint from an artifact.
- Deferred rounds retry remaining failures even when a diagnostic says `retryable: false`. That field describes the cause; it does not suppress the two-round policy. Billing/auth/refusal failures can therefore appear once in each round.

### Immediate planner attempts

[`PlannerRetryPolicy`](../src/planner_retry.py) applies only inside evaluation planner-stage executions. The normal chat planner gets no policy by default and retains its single-attempt behavior.

| Policy | Current behavior |
| --- | --- |
| Attempts per planner-stage execution | Default 3; configurable only from 1 to 3 |
| Backoff | Deterministic 1-second and 2-second delays for the default three attempts |
| Retry-after handling | Trusted typed HTTP response header; integer seconds or timezone-aware HTTP date, bounded to 8 seconds |
| Immediate transient retry | Typed timeout/connection errors; HTTP 408, 409, 429, and 5xx |
| Schema/truncation retry | Invalid plan or incomplete output; one fixed `PLAN_CORRECTION` instruction is inserted, without rejected output |
| No immediate retry | Other permanent HTTP errors, refusals, configuration/unknown failures, cancellation |

`_plan_with_retries` uses one shared attempt budget for mixed schema/provider failures. A permanent failure ends the remaining immediate attempts. Each later deferred planner round gets a fresh immediate budget; a planner that stays transiently failed can therefore invoke the model up to nine times across all three rounds. `stage_attempt` counts planner-stage executions, not individual model attempts.

Cancellation and broken-pipe signals are terminal, not case failures to put into the next round. `EvaluationStopped`, `_StageModel`, and `_raise_if_terminal` keep transport shutdown out of per-case retry handling. Closing the caller-pulled stream before its terminal boundary does not prefetch retries or publish a completed artifact.

## 6. Result and safe diagnostic contract

Each Python case result contains:

| Field | Meaning |
| --- | --- |
| `case_index` | One-based position in the input array |
| `input` | The three-field case |
| `retrieval_strategy` | Validated wire/runner strategy label |
| `retrieved_chunks` | Ranked full-text/provenance records when answer generation reaches the judge |
| `generated_answer` | Composed answer, or null if generation has not reached the judge |
| `judge_scores` | Validated seven-criterion object, or null |
| `error` | Final failure record, or null |
| `attempt_history` | Ordered safe records for prior/current failed rounds |
| `metrics` | Numeric latency, call, token, cost, workflow, and context snapshot |
| `performance` | Deterministic budget checks over `metrics` (below) |

`check_performance` in [`src/evaluation_metrics.py`](../src/evaluation_metrics.py) compares RAG-only values (judge work excluded; retried work counts again) with the runner's `PerformanceBudget` merged with the case's `performance_budget`:

| Check | Value | Default limit |
| --- | --- | --- |
| `rag_latency_ms` | Active RAG stage time | 60000 |
| `rag_model_calls` | RAG model invocations | 12 |
| `retrievals` | Retriever calls | 7 |
| `followup_queries` | Coverage-review follow-up subqueries | 2 |
| `rag_tokens` | Provider-reported RAG tokens | Off |
| `rag_cost_usd` | Estimated RAG cost | Off |
| `duplicate_chunks` | Repeated non-reused excerpts sent to RAG subanswers | 0 |

Each check is `pass`, `fail`, or `unknown` (value not reported or not yet reached). The case `status` is `fail` if any check fails, otherwise `incomplete` if any check is unknown or the case did not finish, otherwise `pass`. `aggregate_metrics.performance` counts `pass`/`fail`/`incomplete` cases and failures per check. Metrics also report `workflow.reused_searches`, while `context.duplicate_chunks` excludes allowed reuse and judge context. Performance checks are reported alongside judge scores; they do not change judge scores or case success.

Chunk fields are `rank`, `text`, `score`, `paper_id`, `chunk_index`, `filename`, `title`, and `page`. `_serialize_chunks` requires 1–35 chunks, consecutive ranks from 1, finite non-boolean scores, nonempty identity/display/text strings, zero-based nonnegative chunk index, and positive page. Pre-judge failed results can show no published chunks even when useful retrieval work remains in an internal checkpoint. A judge failure keeps the generated answer and chunks for diagnosis.

[`src/evaluation_diagnostics.py`](../src/evaluation_diagnostics.py) defines `classify_failure`, `failure_record`, `valid_record`, `sanitize_record`, and `sanitize_result_diagnostics`. Structured failures use exactly:

| Diagnostic field | Constraint |
| --- | --- |
| `stage` | Known stage label; includes `model`/`evaluation` fallback labels |
| `category` | Allowlisted reason code |
| `http_status` | Null or trusted numeric 400–599 status consistent with category |
| `retryable` | Fixed category-derived boolean, also true for HTTP 409 |
| `retry_round` | 0, 1, or 2 |
| `stage_attempt` | One-based execution count for that stage/hop, at most round + 1 |
| `hop_index` | One-based 1–7 only for retrieval/subanswer and follow-up counterparts; otherwise null |
| `message` | Rebuilt fixed safe text; never provider error text |

Categories include `payment_required`, `authentication`, `authorization`, `rate_limit`, `timeout`, `connection`, `http_server_error`, `http_client_error`, `incomplete_response`, `refusal`, `invalid_planner_output`, `retrieval_failure`, `invalid_judge_output`, `cancellation`, `configuration`, and `unknown`.

`classify_failure` reads known exception types and bounded cause chains, not arbitrary exception strings, response bodies, headers, URLs, or duck-typed status attributes. HTTP mappings are 401 authentication, 402 payment, 403 authorization, 408 timeout, 429 rate limit, then 5xx server error versus other 4xx client error. `_StageModel` preserves the safe cause of the last attempted provider invocation even when shared multihop helpers wrap the exception.

Publication sanitization drops unknown result fields and reconstructs messages from the allowlist. Failure history has at most three entries, one per failed round; recovered successes have at most two. A final error must agree with the last history entry. Malformed diagnostics fail closed to safe fallback information. Legacy `{stage, message}` errors without history remain accepted but get a rebuilt message.

These safety checks protect diagnostics, not the inherent privacy of an evaluation artifact: artifacts still contain user questions, grading notes, paper chunk text, generated answers, and judge explanations. Store and share them accordingly.

## 7. Judge schema and score interpretation

`_load_judge_template` requires the exact `SYSTEM PROMPT:` and `USER PROMPT (fill per example):` sections plus the runtime placeholders in [`tests/judge.md`](../tests/judge.md). `_render_judge_prompt` supplies the question, numbered retrieved chunk text, reference source array (or `NOT PROVIDED`), generated answer, and grading notes as the reference answer. The same configured model interface performs generation and judging in the standard Python construction.

`parse_judge_output` accepts strict JSON only: no Markdown fences, prose, duplicate keys, NaN/Infinity, missing criteria, or unexpected criterion fields. All seven criteria contain integer `score` from 0 to 5, `confidence` in `high`/`medium`/`low`, nonempty `reasoning`, and a string-array `flags`.

| Criterion | Question asked by the rubric | Extra field |
| --- | --- | --- |
| `context_precision` | How much retrieved context is relevant? | None |
| `context_recall` | How much information needed for the answer is present? | None |
| `relevant_chunk_ranking` | Are relevant chunks near the top? | `relevant_chunk_ranks`: integer ranks 1–35 |
| `context_utilization` | Does the answer use the retrieved evidence? | `chunks_used`: ranks 1–35 or nonempty descriptive strings |
| `semantic_similarity` | Does the answer match the reference's meaning? | `contradicting_claims`: string array |
| `completeness` | Does the answer cover the requested/reference elements? | `missing_elements`: string array |
| `hallucination_rate` | Is the answer grounded in retrieved chunks? | `unsupported_claims`: string array |

**Higher is better for every score**, including `hallucination_rate`: 5 means no unsupported claims; 0 means largely unsupported. The name is not a literal percentage/rate. Semantic similarity is an LLM judgment, not cosine similarity. Hallucination is assessed against retrieved context, not merely agreement with the reference. Missing ground truth should lower judge confidence and produce flags rather than false certainty.

`_evaluation_result` computes aggregates from the latest final-at-that-point results with no error and a judge-score object. Each criterion is an unweighted arithmetic mean across successful judged cases; failures are excluded, not treated as zero. Terminal means use only the final successful results after retry replacement. With no successes, every aggregate is null. There is no combined overall score or threshold that changes a case from transport/model success to failure.

The UI formats scores to two decimals and uses [`evaluationScoreColor`](../app/src/lib/evaluation-score.ts) for a clamped red-to-green 0–5 scale. Display rounding and color do not change stored judge scores or aggregate calculations.

## 8. Python stream, Tauri transport, and renderer state

The CLI request is `command: evaluate`, `stream: true`, `retrieval_type`, and `test_cases`. The caller must provide the cases; there is no implicit file load at that transport entry point. [`handle_evaluation_stream`](../src/cli/commands.py) delegates to `evaluation_events` in [`src/cli/stream_workflows.py`](../src/cli/stream_workflows.py).

| Python event | Key payload / behavior |
| --- | --- |
| `run_start` | `retrieval_type`, `total` after validation/setup |
| `case_start` | `case_index`, `total`, `retrieval_type`, `input` for initial pass |
| `case_success`, `case_failure` | `completed`, `total`, complete current `result` |
| `retry_round_start` | `retry_round`, `total`, number `pending` |
| `case_retry_start` | `case_index`, `total`, `retry_round` |
| `case_retry_result` | `retry_round`, `completed`, `total`, replacement `result` |
| `complete` | Final `retrieval_type`, `completed`, `total`, `succeeded`, `failed`, `aggregate_scores` |

The graph stores results by original case index. `completed` counts distinct cases with a result, not attempts, so it remains at total throughout deferred retries. Retry progress is separately tracked. A compatibility evaluator that exposes only `.run` rather than `start_case`/`resume_case` uses the fallback runner interface; exact-stage resume guarantees describe the production `EvaluationRunner`, not arbitrary injected fakes.

[`app/src-tauri/src/lib.rs`](../app/src-tauri/src/lib.rs) implements `evaluate_stream`, sends the request through the Python bridge, and requires a terminal event. [`app/src-tauri/src/evaluation.rs`](../app/src-tauri/src/evaluation.rs) parses/sanitizes event and result DTOs, preserving nested snake_case fields while exposing camelCase event/envelope fields to TypeScript. Invalid or failed transport uses safe errors rather than raw provider details.

[`evaluationSaga`](../app/src/slices/evaluation/sagas/evaluation-saga.ts) uses `takeLeading` to prevent overlapping UI runs and an expanding event-channel buffer to retain synchronous event bursts. It sanitizes results, ignores events after a terminal frame, detects a promise settling without completion, and dispatches case/retry/aggregate updates. [`evaluation-types.ts`](../app/src/slices/evaluation/evaluation-types.ts) holds results in original input positions and replaces an earlier failure when a retry succeeds.

The Tauri [RagEvals component](../app/src/components/RagEvals.svelte) shows distinct initial-pass and retry-round progress, collapsed score summaries, detailed source text/provenance, generated answers, criterion explanations, safe final failures, and expandable failed-attempt history. It also shows run-level Runtime totals, Performance (cases within limits, over a limit, incomplete, and which limits were exceeded) and Reliability (success rate, first-try successes, recoveries after retry, still-failing cases, failed attempts), all derived from each case's latest result by `evaluationMetricTotals` and `evaluationReliabilityTotals` in [`evaluation-metrics.ts`](../app/src/lib/evaluation-metrics.ts). Each case shows its performance status in the summary and a check-by-check table in its details; `sanitizeEvaluationPerformance` drops inconsistent performance data. It does not turn case failures into chat messages or erase already displayed results because a later case fails. Reloadable artifact browsing and durable process-restart continuation are not implemented by these component/saga APIs.

## 9. Completed-run artifacts and failure boundaries

### Python publication

[`_persisted_evaluation_stream`](../src/cli/__main__.py) accumulates initial case inputs and latest results by one-based index. On the sole `complete` event, it invokes `persist_evaluation_results` **before** forwarding terminal completion to stdout. The artifact contains exactly one result per input in input order. Per-case failure is still a completed run and produces an artifact, even if every case fails. A validation/transport/cancellation failure before terminal completion does not produce a completed-run artifact.

[`src/evaluation_results.py`](../src/evaluation_results.py) defines:

| Item | Value or behavior |
| --- | --- |
| `EVALUATION_RESULTS_SCHEMA` | `rag-evaluation-results` |
| `EVALUATION_RESULTS_SCHEMA_VERSION` | 2 |
| `CANONICAL_RETRIEVAL_IDENTIFIER` | `multihop-hybrid` |
| `DEFAULT_EVALUATION_RESULTS_PATH` | `tests/eval_results.json`, used as a filename template, not overwritten |
| Artifact fields | `schema`, `schema_version`, `metadata`, `input_cases`, `results`, `aggregate_scores`, `summary` |
| Metadata | Canonical retrieval identifier plus requested strategy label |
| Summary | `total`, `succeeded`, `failed` |

Each call generates a sibling named `eval_results_<UTC-date-and-time-with-microseconds>Z_<UUID>.json` using the supplied template stem/suffix. Existing `eval_results.json` and dated artifacts remain untouched. Serialization is deterministic UTF-8 JSON with sorted keys, indentation, a trailing newline, and non-finite numbers rejected; filename uniqueness/time do not imply reproducible model outputs.

The writer creates a temporary file in the target directory, writes/flushes/fsyncs its contents, and publishes with a no-overwrite hard link. Destination collisions fail rather than erase earlier runs, and the owned temporary link is removed afterward. Publication requires a filesystem supporting that hard-link operation. The code fsyncs the file, not the directory; do not infer a stronger crash-durability guarantee than that implementation supplies.

A successfully published artifact can exist even if downstream stdout delivery subsequently fails. Conversely, failure to persist prevents a successful completion event. A missing terminal frame means outcome uncertainty, not proof that no artifact exists. Version-1 files are not rewritten or migrated by the new writer.

## 10. Existing tests, safe verification, and extension cautions

These test paths are maintained behavior references, not historical pass counts or results from this documentation pass:

| Area | Relevant tests and exact symbols |
| --- | --- |
| Input/judge/generation contracts | [`tests/test_evaluation_runner.py`](../tests/test_evaluation_runner.py) |
| Exact-stage recovery and bounded rounds | [`tests/test_evaluation_stages.py`](../tests/test_evaluation_stages.py): `test_every_stage_and_hop_resumes_without_replaying_validated_work`, `test_advancing_failure_resumes_new_stage_on_next_round_and_preserves_history`, `test_no_round_three_and_new_run_starts_fresh` |
| Immediate planner policy | [`tests/test_evaluation_planner_retry.py`](../tests/test_evaluation_planner_retry.py): `test_mixed_schema_and_provider_failures_use_one_shared_budget`, `test_non_evaluation_callers_keep_one_attempt`, `test_default_evaluation_disables_sdk_retries_without_changing_chat` |
| Safe diagnostic handling | [`tests/test_evaluation_diagnostics.py`](../tests/test_evaluation_diagnostics.py): `test_unknown_errors_never_stringified_or_duck_typed`, `test_402_at_every_stage_retains_three_ordered_attempts`, `test_artifact_boundary_sanitizes_untrusted_history` |
| Stream and artifact boundary | [`tests/test_cli_evaluation_stream.py`](../tests/test_cli_evaluation_stream.py), [`tests/test_cli_stream_workflows.py`](../tests/test_cli_stream_workflows.py), [`tests/test_evaluation_results.py`](../tests/test_evaluation_results.py) |
| Dataset integrity | [`tests/test_eval_questions.py`](../tests/test_eval_questions.py) |
| Tauri UI/API/state/diagnostics | [`app/tests/evaluation-api.test.mjs`](../app/tests/evaluation-api.test.mjs), [`evaluation-saga.test.mjs`](../app/tests/evaluation-saga.test.mjs), [`evaluation-state.test.mjs`](../app/tests/evaluation-state.test.mjs), [`evaluation-diagnostics.test.mjs`](../app/tests/evaluation-diagnostics.test.mjs), [`evaluation-interface.test.mjs`](../app/tests/evaluation-interface.test.mjs), [`evaluation-score.test.mjs`](../app/tests/evaluation-score.test.mjs) |

Update and run focused offline tests before a live evaluation. A new stage needs cursor/commit semantics, failure classification, attempt accounting, transport validation and UI handling. Retry/diagnostic changes need both producer and desktop consumer tests. A real strategy comparison must change retrieval, not just labels.

See [operations and testing](operations-and-testing.md) for verification commands, [getting started](getting-started.md) and [configuration](configuration.md) for environment setup, [data storage](data-storage.md) for persistence boundaries, and [frontend](frontend.md), [native desktop](native-desktop.md), and [PDF viewer](pdf-viewer.md) for their consumer contexts.
