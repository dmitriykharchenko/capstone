import assert from "node:assert/strict";
import test, { after, before } from "node:test";
import { fileURLToPath } from "node:url";
import { createServer } from "vite";
import { runSaga, stdChannel } from "redux-saga";
import { evaluationMetricTotals, evaluationReliabilityTotals, formatCost, formatLatency, formatMetric, formatMetricTotal, sanitizeEvaluationMetrics, sanitizeEvaluationPerformance } from "../src/lib/evaluation-metrics.ts";
let sanitizeEvaluationResult;

function metrics() {
  const tokens = { input: 400, cached_input: 120, output: 100, total: 500, known_input: 400, known_output: 100, known_total: 500, reported_calls: 5 };
  const cost = { estimated_usd: 0.002, known_usd: 0.002, priced_calls: 5, provider_reported_calls: 1, rate_estimated_calls: 4 };
  const context = { chunks: 6, unique_chunks: 2, duplicate_chunks: 0, characters: 1200, utf8_bytes: 1400, max_chunks: 2, max_characters: 400, prompt_characters: 3200, max_prompt_characters: 800 };
  return {
    version: 1, latency_ms: { total: 1000, rag: 700, judge: 300 }, model_calls: { total: 5, rag: 3, judge: 1, embedding: 1 },
    tokens, cost, workflow: { exchanges: 3, tool_calls: 2 }, context,
    stages: { judge: { latency_ms: 300, model_calls: 1, exchanges: 0, tool_calls: 0,
      tokens: { ...tokens, reported_calls: 1 }, cost: { ...cost, priced_calls: 1, rate_estimated_calls: 0 },
      context: { ...context, chunks: 99, characters: 9900 } } },
    pricing: { input_usd_per_million: 1, output_usd_per_million: 2, embedding_usd_per_million: null, cached_input_usd_per_million: 0.25 },
  };
}

const cases = [1, 2].map((index) => ({ question: `Question ${index}`, grading_notes: "Ground truth", sources: [] }));
function result(index = 1, measured = metrics()) {
  return { case_index: index, input: cases[index - 1], retrieval_strategy: "hybrid", retrieved_chunks: [],
    generated_answer: "An answer", judge_scores: null, error: null, attempt_history: [], metrics: measured };
}

test("fresh metrics omit reused searches and preserve the exact numeric contract through diagnostic sanitation", () => {
  const original = metrics();
  assert.ok(!Object.hasOwn(original.workflow, "reused_searches"));
  const safe = sanitizeEvaluationResult(result(1, original));
  assert.deepEqual(JSON.parse(JSON.stringify(safe.metrics)), original);
  assert.notEqual(safe.metrics, original);
  original.context.chunks = 123;
  assert.equal(safe.metrics.context.chunks, 6);
  const extra = metrics();
  extra.metadata = "synthetic-untrusted-metrics-canary";
  extra.tokens.raw_usage = "synthetic-untrusted-metrics-canary";
  extra.stages.judge.prompt = "synthetic-untrusted-metrics-canary";
  assert.doesNotMatch(JSON.stringify(sanitizeEvaluationMetrics(extra)), /synthetic-untrusted/);
  const legacy = metrics();
  legacy.workflow.reused_searches = 1;
  assert.deepEqual(sanitizeEvaluationMetrics(legacy).workflow, { exchanges: 3, tool_calls: 2, reused_searches: 1 });
  for (const invalid of [-1, 0.5, Number.MAX_SAFE_INTEGER + 1, "1"]) {
    const malformedLegacy = metrics();
    malformedLegacy.workflow.reused_searches = invalid;
    assert.equal(sanitizeEvaluationMetrics(malformedLegacy), null);
  }
  const older = metrics();
  delete older.tokens.cached_input;
  delete older.stages.judge.tokens.cached_input;
  delete older.pricing.cached_input_usd_per_million;
  assert.notEqual(sanitizeEvaluationMetrics(older), null);
  for (const invalid of [-1, 0.5, Number.MAX_SAFE_INTEGER + 1, "1", true]) {
    const malformed = metrics();
    malformed.tokens.cached_input = invalid;
    assert.equal(sanitizeEvaluationMetrics(malformed), null);
  }
  const excessive = metrics(); excessive.tokens.cached_input = 401;
  assert.equal(sanitizeEvaluationMetrics(excessive), null);
});

test("legacy, unsupported and malformed metrics are unavailable, never fabricated zero", () => {
  for (const value of [undefined, null, {}, [], { ...metrics(), version: 2 }]) {
    assert.equal(sanitizeEvaluationMetrics(value), null);
  }
  const invalid = [
    (m) => { delete m.context.prompt_characters; },
    (m) => { m.tokens.input = "400"; },
    (m) => { m.latency_ms.total = Infinity; },
    (m) => { m.workflow.tool_calls = -1; },
    (m) => { m.model_calls.embedding = NaN; },
    (m) => { m.stages.judge.cost.known_usd = null; },
    (m) => { m.cost = null; },
    (m) => { delete m.pricing; },
  ];
  for (const change of invalid) {
    const value = metrics(); change(value);
    assert.equal(sanitizeEvaluationMetrics(value), null);
  }
  for (const value of [null, undefined, Infinity, NaN, -1]) assert.equal(formatMetric(value), "Unavailable");
  assert.equal(formatMetric(0), "0");
  assert.equal(formatLatency(0), "0.00 s");
  assert.equal(formatCost(0), "$0.000000");
  assert.equal(formatCost(0.0000001), "< $0.000001");
  assert.equal(formatCost(null), "Unavailable");
});

test("current snapshot totals count each case once and exclude separate judge context", () => {
  const retried = metrics();
  retried.latency_ms = { total: 3000, rag: 2700, judge: 300 };
  retried.model_calls = { total: 15, rag: 9, judge: 3, embedding: 3 };
  const totals = evaluationMetricTotals([result(), null, result(2), result(1, retried)]);
  assert.equal(totals.cases, 2);
  assert.equal(totals.measuredCases, 2);
  assert.equal(totals.latency.value, 4000);
  assert.equal(totals.modelCalls.value, 20);
  assert.equal(totals.chunks.value, 12);
  assert.equal(totals.characters.value, 2400);
  assert.equal(totals.exchanges.value, 6);
  assert.equal(totals.toolCalls.value, 4);
  assert.equal(totals.cachedTokens.value, 240);
});

test("only exact measured Python stage names can enter retained metrics", () => {
  const valid = metrics();
  const names = ["planner", "retrieval", "subanswer", "coverage", "followup_retrieval", "followup_subanswer", "composer", "judge"];
  valid.stages = Object.fromEntries(names.map((name) => [name, structuredClone(valid.stages.judge)]));
  assert.deepEqual(Object.keys(sanitizeEvaluationMetrics(valid).stages), names);
  for (const name of ["embedding", "model", "evaluation", "Judge", "judge ", "__proto__", "constructor", "synthetic-untrusted-stage-canary"]) {
    const invalid = metrics();
    invalid.stages = Object.fromEntries([[name, invalid.stages.judge]]);
    assert.equal(sanitizeEvaluationMetrics(invalid), null, name);
    const retained = sanitizeEvaluationResult(result(1, invalid));
    assert.equal(retained.metrics, null, name);
    assert.doesNotMatch(JSON.stringify(retained), /synthetic-untrusted-stage-canary/);
  }
});

test("all global and stage counters require nonnegative safe integers", () => {
  const sample = metrics();
  const paths = [
    ...Object.keys(sample.model_calls).map((key) => ["model_calls", key]),
    ...Object.keys(sample.workflow).map((key) => ["workflow", key]),
  ];
  for (const prefix of [[], ["stages", "judge"]]) {
    for (const section of ["tokens", "context", "cost"]) {
      for (const key of Object.keys(sample[section]).filter((key) => !["estimated_usd", "known_usd"].includes(key))) {
        paths.push([...prefix, section, key]);
      }
    }
  }
  for (const key of ["model_calls", "exchanges", "tool_calls"]) paths.push(["stages", "judge", key]);
  for (const path of paths) {
    for (const invalid of [0.5, -0.5, Number.MAX_SAFE_INTEGER + 1, Infinity, NaN, true, "1"]) {
      const value = metrics();
      const target = path.slice(0, -1).reduce((current, key) => current[key], value);
      target[path.at(-1)] = invalid;
      assert.equal(sanitizeEvaluationMetrics(value), null, path.join("."));
    }
    for (const boundary of [0, Number.MAX_SAFE_INTEGER]) {
      const value = metrics();
      const target = path.slice(0, -1).reduce((current, key) => current[key], value);
      target[path.at(-1)] = boundary;
      if (path.at(-1) === "input" && boundary === 0) target.cached_input = 0;
      if (path.at(-1) === "cached_input") target.input = boundary;
      assert.notEqual(sanitizeEvaluationMetrics(value), null, path.join("."));
    }
  }
});

test("latency, USD cost and configured rates still allow finite nonnegative decimals", () => {
  const value = metrics();
  value.latency_ms = { total: 1000.75, rag: 700.25, judge: 300.5 };
  value.stages.judge.latency_ms = 300.5;
  value.pricing = { input_usd_per_million: 0.5, output_usd_per_million: 0.75, embedding_usd_per_million: 0.125, cached_input_usd_per_million: 0.1 };
  assert.deepEqual(JSON.parse(JSON.stringify(sanitizeEvaluationMetrics(value))), value);
});

test("unknown token and cost totals stay unavailable with explicitly known partial subtotals", () => {
  const partial = metrics();
  partial.tokens = { input: null, output: null, total: null, known_input: 40, known_output: 10, known_total: 50, reported_calls: 1 };
  partial.cost = { estimated_usd: null, known_usd: 0.001, priced_calls: 1, provider_reported_calls: 1, rate_estimated_calls: 0 };
  const totals = evaluationMetricTotals([result(), result(2, partial)]);
  assert.deepEqual(totals.tokens, { value: null, known: 550 });
  assert.deepEqual(totals.cost, { value: null, known: 0.003 });
  assert.equal(formatMetricTotal(totals.tokens), "Unavailable (known subtotal: 550)");
  assert.equal(formatMetricTotal(totals.cost, formatCost), "Unavailable (known subtotal: $0.003000)");
  const mixedLegacy = evaluationMetricTotals([result(), result(2, null)]);
  assert.deepEqual(mixedLegacy.latency, { value: null, known: 1000 });
  assert.equal(mixedLegacy.measuredCases, 1);
  for (const input of [[], [null], [result(1, null)]]) {
    assert.deepEqual(evaluationMetricTotals(input).tokens, { value: null, known: null });
  }
});

let vite, Component, MetricsComponent, render, beginEvaluation, initialEvaluationState, recordEvaluationCase, startEvaluationRetryRound, completeEvaluation;
let evaluationSaga, evaluationReducer, runEvaluation;
before(async () => {
  vite = await createServer({
    root: fileURLToPath(new URL("..", import.meta.url)), appType: "custom", logLevel: "silent", server: { middlewareMode: true },
    plugins: [{ name: "offline-metrics-store", enforce: "pre", load(id) {
      if (id.endsWith("/src/store.ts")) return `export const store = { dispatch() {}, createSelector(select) { return () => ({ subscribe(fn) { fn(select({ evaluation: globalThis.__evaluationMetricsState })); return () => {}; } }); } };`;
    } }],
  });
  ({ default: Component } = await vite.ssrLoadModule("/src/components/RagEvals.svelte"));
  ({ default: MetricsComponent } = await vite.ssrLoadModule("/src/components/EvaluationRuntimeMetrics.svelte"));
  ({ render } = await vite.ssrLoadModule("svelte/server"));
  ({ sanitizeEvaluationResult } = await vite.ssrLoadModule("/src/lib/evaluation-diagnostics.ts"));
  ({ beginEvaluation, initialEvaluationState, recordEvaluationCase, startEvaluationRetryRound, completeEvaluation } = await vite.ssrLoadModule("/src/slices/evaluation/evaluation-types.ts"));
  ({ evaluationSaga } = await vite.ssrLoadModule("/src/slices/evaluation/sagas/evaluation-saga.ts"));
  ({ evaluationReducer, runEvaluation } = await vite.ssrLoadModule("/src/slices/evaluation/evaluation-slice.ts"));
});
after(async () => { await vite?.close(); delete globalThis.__evaluationMetricsState; });

test("retry state replaces cumulative metrics, preserves failures, and fences terminal updates", () => {
  const failure = { stage: "planner", category: "timeout", http_status: null, retryable: true, retry_round: 0, stage_attempt: 1, hop_index: null, message: "ignored" };
  let state = beginEvaluation(initialEvaluationState, cases, 2);
  state = recordEvaluationCase(state, { ...result(), error: failure, attempt_history: [failure] }, 1, 2);
  state = recordEvaluationCase(state, result(2), 2, 2);
  assert.equal(evaluationMetricTotals(state.results).modelCalls.value, 10);
  state = startEvaluationRetryRound(state, 1, 1);
  const retried = metrics(); retried.model_calls.total = 8;
  state = recordEvaluationCase(state, { ...result(1, retried), attempt_history: [failure] }, 2, 2);
  assert.equal(state.results[0].attempt_history.length, 1);
  assert.equal(evaluationMetricTotals(state.results).modelCalls.value, 13);
  assert.equal(state.completed, 2);
  assert.equal(state.succeeded, 2);
  const terminal = completeEvaluation(state, 2, 2, 2, 0, {});
  assert.equal(recordEvaluationCase(terminal, result(), 2, 2), terminal);
});

test("actual case UI renders runtime, workflow, usage coverage and separate context stages", () => {
  const partial = metrics(); partial.tokens.total = null; partial.cost.estimated_usd = null;
  const { body } = render(MetricsComponent, { props: { metrics: partial } });
  for (const label of ["Runtime and workflow", "RAG latency", "Judge latency", "RAG exchanges", "Retrieval tool calls", "RAG / judge / embedding calls", "Tokens and cost", "RAG context", "Chunk occurrences", "Unique chunks", "Evidence UTF-8 bytes", "Full RAG prompt characters", "Stage breakdown"]) assert.ok(body.includes(label), label);
  assert.match(body, /1\.00 s/);
  assert.match(body, /Unavailable \(known subtotal: 500\)/);
  assert.match(body, /Unavailable \(known subtotal: \$0\.002000\)/);
  assert.match(body, /Cached input tokens<\/dt>\s*<dd[^>]*>120<\/dd>/);
  assert.match(body, /Cached input<\/th>/);
  assert.match(body, /Token usage reported for 5 of 5 model calls/);
  assert.match(body, /1 provider-reported, 4 rate-estimated/);
  assert.match(body, /<th scope="row"[^>]*>Judge<\/th>/);
  assert.match(body, /excludes deferred-round waits and UI pauses/);
  assert.match(body, /SDK-internal retries are not observable/);
  assert.match(body, /Evidence sizes include formatting/);
  assert.match(body, /9,900/);
  assert.doesNotMatch(body, /Reused searches/);
});

test("actual summary derives snapshots without aggregate event and shows legacy metrics unavailable", () => {
  globalThis.__evaluationMetricsState = { ...beginEvaluation(initialEvaluationState, cases, 2), results: [result(), result(2, null)], completed: 2 };
  const { body } = render(Component);
  assert.match(body, /Runtime totals/);
  assert.match(body, /Metrics available for 1 of 2 current case results/);
  assert.match(body, /Unavailable \(known subtotal: 1\.00 s\)/);
  assert.match(body, /Unavailable — this result has no supported runtime metrics/);
  assert.doesNotMatch(body, /NaN|undefined/);
  assert.doesNotMatch(body, /Aggregate means/);
  for (const metrics of [undefined, null, { version: 2 }]) {
    assert.match(render(MetricsComponent, { props: { metrics } }).body, /Unavailable/);
  }
});

function performance(status = "fail") {
  return { version: 1, status, checks: {
    rag_latency_ms: { value: 700, limit: 60000, status: "pass" },
    followup_queries: { value: status === "fail" ? 1 : 0, limit: 0, status: status === "fail" ? "fail" : "pass" },
  } };
}

test("performance checks survive sanitation only when internally consistent", () => {
  const good = performance();
  assert.deepEqual(sanitizeEvaluationResult({ ...result(), performance: good }).performance, good);
  assert.equal(sanitizeEvaluationResult(result()).performance, null);
  const bad = [
    (p) => { p.version = 2; },
    (p) => { p.status = "pass"; },
    (p) => { p.checks.followup_queries.status = "pass"; },
    (p) => { p.checks.private = { value: 1, limit: 1, status: "pass" }; },
    (p) => { p.checks.rag_latency_ms.value = "700"; },
    (p) => { p.checks.rag_latency_ms = { value: null, limit: 1, status: "pass" }; },
  ];
  for (const change of bad) {
    const value = performance(); change(value);
    assert.equal(sanitizeEvaluationPerformance(value), null);
  }
  const unknown = { version: 1, status: "incomplete", checks: { rag_tokens: { value: null, limit: 10, status: "unknown" } } };
  assert.deepEqual(sanitizeEvaluationPerformance(unknown), unknown);
  assert.equal(sanitizeEvaluationPerformance({ ...unknown, status: "pass" }), null);
});

test("reliability totals use the latest result per case and count recovered retries", () => {
  const failure = { stage: "planner", category: "timeout", http_status: null, retryable: true, retry_round: 0, stage_attempt: 1, hop_index: null, message: "ignored" };
  const scored = (index, extra) => ({ ...result(index), judge_scores: {}, ...extra });
  const totals = evaluationReliabilityTotals([
    { ...result(1), error: failure, attempt_history: [failure] },
    scored(2, { performance: performance("pass") }),
    scored(1, { attempt_history: [failure], performance: performance() }),
    { ...result(3), error: failure, attempt_history: [failure, failure] },
  ]);
  assert.equal(totals.cases, 3);
  assert.equal(totals.succeeded, 2);
  assert.equal(totals.firstTry, 1);
  assert.equal(totals.recovered, 1);
  assert.equal(totals.failed, 1);
  assert.equal(totals.failedAttempts, 3);
  assert.equal(totals.performanceChecked, 2);
  assert.equal(totals.performancePass, 1);
  assert.equal(totals.performanceFail, 1);
  assert.deepEqual(totals.failedChecks, [["followup_queries", 1]]);
});

test("UI shows run-level performance and reliability and per-case checks", () => {
  const { body: caseBody } = render(MetricsComponent, { props: { metrics: metrics(), performance: performance() } });
  for (const label of ["Performance checks", "Follow-up queries", "RAG latency", "over at least one limit", "Fail"]) assert.ok(caseBody.includes(label), label);
  assert.match(render(MetricsComponent, { props: { metrics: metrics() } }).body, /this result has no performance checks/);
  globalThis.__evaluationMetricsState = { ...beginEvaluation(initialEvaluationState, cases, 2),
    results: [{ ...result(), judge_scores: null, performance: performance() }, { ...result(2), performance: performance("pass") }], completed: 2 };
  const { body } = render(Component);
  for (const pattern of [/>Performance</, />Reliability</, /Over a limit/, /1 of 2/, /Follow-up queries \(1\)/, /Success rate/, /Recovered after retry/, /Performance: over limit/, /Performance: within limits/]) assert.match(body, pattern);
  assert.doesNotMatch(body, /NaN|undefined/);
});

test("streamed success, failure and retry metrics reach state without a complete-event metrics aggregate", async () => {
  const channel = stdChannel();
  const actions = [];
  const failure = { stage: "planner", category: "timeout", http_status: null, retryable: true, retry_round: 0, stage_attempt: 1, hop_index: null, message: "ignored" };
  const retried = metrics(); retried.latency_ms.total = 3000;
  const task = runSaga({ channel, dispatch: (action) => actions.push(action), getState: () => ({}) }, evaluationSaga, {
    runEvaluation: async (_cases, emit) => {
      emit({ event: "runStart", retrievalType: "hybrid", total: 2 });
      emit({ event: "caseFailure", completed: 1, total: 2, result: { ...result(), error: failure, attempt_history: [failure] } });
      emit({ event: "caseSuccess", completed: 2, total: 2, result: result(2) });
      emit({ event: "retryRoundStart", retryRound: 1, total: 2, pending: 1 });
      emit({ event: "caseRetryStart", retryRound: 1, total: 2, caseIndex: 1 });
      emit({ event: "caseRetryResult", retryRound: 1, completed: 2, total: 2, result: { ...result(1, retried), attempt_history: [failure] } });
      emit({ event: "complete", retrievalType: "hybrid", completed: 2, total: 2, succeeded: 2, failed: 0, aggregateScores: {} });
    },
  });
  try {
    channel.put(runEvaluation(cases));
    await new Promise((resolve) => setImmediate(resolve));
    const state = actions.reduce((state, action) => evaluationReducer(state, action), undefined);
    assert.equal(state.running, false);
    assert.equal(state.succeeded, 2);
    assert.equal(evaluationMetricTotals(state.results).latency.value, 4000);
    assert.deepEqual(JSON.parse(JSON.stringify(state.results[0].metrics)), retried);
    assert.deepEqual(JSON.parse(JSON.stringify(state.results[1].metrics)), metrics());
    assert.equal(state.results[0].attempt_history.length, 1);
  } finally {
    task.cancel();
    await task.toPromise();
  }
});
