import assert from "node:assert/strict";
import test, { after, before } from "node:test";
import { fileURLToPath } from "node:url";
import { createServer } from "vite";
import { compile } from "svelte/compiler";
import { readFile } from "node:fs/promises";
let failureReasons, sanitizeFailure, sanitizeEvaluationResult;
let beginEvaluation, initialEvaluationState, recordEvaluationCase, startEvaluationRetryRound, completeEvaluation;

const CANARY = "synthetic-untrusted-diagnostic-canary";
const cases = [{ question: "first", grading_notes: "g", sources: [] }, { question: "second", grading_notes: "g", sources: [] }];
function record(category = "payment_required", round = 0, extra = {}) {
  return { stage: "planner", category, http_status: ({ payment_required: 402, authentication: 401, authorization: 403, rate_limit: 429, http_client_error: 400, http_server_error: 503 })[category] ?? null,
    retryable: failureReasons[category][1], retry_round: round, stage_attempt: round + 1, hop_index: null, message: CANARY, ...extra };
}
function result(index = 1, round = 0, category = "payment_required") {
  const history = Array.from({ length: round + 1 }, (_, i) => record(category, i));
  return { case_index: index, input: cases[index - 1], retrieval_strategy: "hybrid", retrieved_chunks: [], generated_answer: null, judge_scores: null, error: history.at(-1), attempt_history: history };
}

test("fixed diagnostic reasons survive sanitation without raw text", async (t) => {
  for (const category of Object.keys(failureReasons)) {
    await t.test(`fixed ${category} reason survives sanitation without raw text`, () => {
      const safe = sanitizeEvaluationResult(result(1, 2, category));
      assert.equal(safe.error.category, category);
      assert.equal(safe.error.message, failureReasons[category][0]);
      assert.deepEqual(safe.attempt_history.map((r) => r.retry_round), [0, 1, 2]);
      assert.doesNotMatch(JSON.stringify(safe), new RegExp(CANARY));
    });
  }
});

test("unknown fields, malformed types and contradictory diagnostics fail closed", () => {
  const omitted = record("timeout");
  delete omitted.http_status;
  delete omitted.hop_index;
  assert.equal(sanitizeFailure(omitted).category, "timeout");
  assert.equal(sanitizeFailure(omitted).http_status, null);
  for (const change of [
    { category: CANARY }, { category: "__proto__" }, { category: {} }, { stage: CANARY }, { body: CANARY },
    { http_status: "402" }, { http_status: 401 }, { http_status: null }, { retryable: true },
    { retry_round: -1 }, { retry_round: 3 }, { retry_round: true }, { stage_attempt: 0 }, { stage_attempt: 2 }, { hop_index: 1 }, { message: {} },
  ]) {
    const bad = { ...record(), ...change };
    assert.equal(sanitizeFailure(bad), null);
    const safe = sanitizeEvaluationResult({ ...result(), error: bad, attempt_history: [bad] });
    assert.equal(safe.error.category, "unknown");
    assert.deepEqual(safe.attempt_history, []);
    assert.doesNotMatch(JSON.stringify(safe), new RegExp(CANARY));
  }
  for (const history of [null, CANARY, [record("payment_required", 1)], [record(), record("payment_required", 1, { stage_attempt: 1 })], [record(), record(), record(), record()]]) {
    const safe = sanitizeEvaluationResult({ ...result(), attempt_history: history });
    assert.deepEqual(safe.attempt_history, []);
    assert.equal(safe.error.category, "unknown");
  }
});

test("retry replacement preserves order, recovered history, progress and terminal fencing", () => {
  let state = beginEvaluation(initialEvaluationState, cases, 2);
  state = recordEvaluationCase(state, result(2), 1, 2);
  state = recordEvaluationCase(state, result(1), 2, 2);
  const initial = state;
  state = startEvaluationRetryRound(state, 1, 2);
  assert.equal(state.retryRound, 1);
  state = recordEvaluationCase(state, { ...result(1), error: null }, 2, 2);
  state = recordEvaluationCase(state, result(2, 1), 2, 2);
  assert.equal(state.succeeded, 1);
  assert.equal(state.failed, 1);
  assert.equal(state.retryCompleted, 2);
  assert.equal(state.completed, 2);
  assert.deepEqual(state.results.map((r) => r.case_index), [1, 2]);
  assert.equal(state.results[0].attempt_history.length, 1);
  assert.equal(initial.results[0].error.category, "payment_required");
  state = startEvaluationRetryRound(state, 2, 1);
  state = recordEvaluationCase(state, result(2, 2), 2, 2);
  const completed = completeEvaluation(state, 2, 2, 1, 1, {});
  assert.equal(recordEvaluationCase(completed, result(1), 2, 2), completed);
  assert.equal(startEvaluationRetryRound(completed, 3, 1), completed);
  assert.doesNotMatch(JSON.stringify(state), new RegExp(CANARY));
});

let vite, Component, render;
before(async () => {
  vite = await createServer({
    root: fileURLToPath(new URL("..", import.meta.url)), appType: "custom", logLevel: "silent", server: { middlewareMode: true },
    plugins: [{ name: "offline-evaluation-store", enforce: "pre", load(id) {
      if (id.endsWith("/src/store.ts")) return `export const store = { dispatch() {}, createSelector(select) { return () => ({ subscribe(fn) { fn(select({ evaluation: globalThis.__evaluationRenderState })); return () => {}; } }); } };`;
    } }],
  });
  ({ default: Component } = await vite.ssrLoadModule("/src/components/RagEvals.svelte"));
  ({ render } = await vite.ssrLoadModule("svelte/server"));
  ({ failureReasons, sanitizeFailure, sanitizeEvaluationResult } = await vite.ssrLoadModule("/src/lib/evaluation-diagnostics.ts"));
  ({ beginEvaluation, initialEvaluationState, recordEvaluationCase, startEvaluationRetryRound, completeEvaluation } = await vite.ssrLoadModule("/src/slices/evaluation/evaluation-types.ts"));
});
after(async () => { await vite?.close(); delete globalThis.__evaluationRenderState; });

test("actual RAG Evals rendering includes accessible ordered history and retry progress", async () => {
  let state = beginEvaluation(initialEvaluationState, cases, 2);
  state = recordEvaluationCase(state, result(1, 2), 1, 2);
  state = recordEvaluationCase(state, { ...result(2), error: null }, 2, 2);
  globalThis.__evaluationRenderState = { ...state, retryRound: 2, retryPending: 1, retryCompleted: 0, currentCaseIndex: 1 };
  const { body } = render(Component);
  assert.match(body, /Retry round 2 of 2/);
  assert.match(body, /aria-live="polite"/);
  assert.match(body, /role="progressbar"[^>]*aria-valuenow="2"/);
  assert.match(body, /payment_required/);
  assert.match(body, /HTTP 402/);
  assert.match(body, /Not retryable/);
  assert.match(body, /<summary[^>]*>Failed attempt history/);
  assert.match(body, /<ol[^>]*aria-label="Failed attempts for test 1"/);
  assert.ok(body.indexOf("stage attempt 1") < body.indexOf("stage attempt 2"));
  assert.ok(body.indexOf("stage attempt 2") < body.indexOf("stage attempt 3"));
  assert.ok(body.indexOf('data-case-index="1"') < body.indexOf('data-case-index="2"'));
  assert.doesNotMatch(body, new RegExp(CANARY));
  const source = await readFile(new URL("../src/components/RagEvals.svelte", import.meta.url), "utf8");
  const compiled = compile(source, { filename: "RagEvals.svelte", generate: "server" });
  assert.deepEqual(compiled.warnings.filter((warning) => warning.code.startsWith("a11y")), []);
});
