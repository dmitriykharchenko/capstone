import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import test, { after, before } from "node:test";
import { runSaga, stdChannel } from "redux-saga";
import { createServer } from "vite";

const appRoot = fileURLToPath(new URL("..", import.meta.url));
const settle = () => new Promise((resolve) => setImmediate(resolve));
let vite, evaluationReducer, evaluationSaga, runEvaluation;

before(async () => {
  vite = await createServer({
    root: appRoot,
    appType: "custom",
    logLevel: "silent",
    server: { middlewareMode: true },
  });
  ({ evaluationSaga } = await vite.ssrLoadModule(
    "/src/slices/evaluation/sagas/evaluation-saga.ts",
  ));
  ({ evaluationReducer, runEvaluation } = await vite.ssrLoadModule(
    "/src/slices/evaluation/evaluation-slice.ts",
  ));
});

after(async () => vite.close());

const testCases = [{ question: "q", grading_notes: "g", sources: [] }];
const aggregateScores = {
  context_precision: null,
  context_recall: null,
  relevant_chunk_ranking: null,
  context_utilization: null,
  semantic_similarity: null,
  completeness: null,
  hallucination_rate: null,
};

function startSaga(service) {
  const input = stdChannel();
  const actions = [];
  const task = runSaga(
    {
      channel: input,
      dispatch: (action) => actions.push(action),
      getState: () => ({}),
    },
    evaluationSaga,
    { runEvaluation: service },
  );
  return { input, actions, task };
}

test("run intent locks controls immediately before transport events arrive", () => {
  const state = evaluationReducer(undefined, runEvaluation(testCases));
  const duplicate = evaluationReducer(state, runEvaluation(testCases));
  assert.equal(state.running, true);
  assert.equal(state.retrievalType, "multihop-hybrid");
  assert.equal(state.total, 1);
  assert.deepEqual(state.testCases, testCases);
  assert.equal(duplicate, state);
});

test("takeLeading ignores duplicate evaluation requests until completion", async () => {
  const callbacks = [];
  const resolves = [];
  let calls = 0;
  const service = (_cases, onEvent) => {
    calls += 1;
    callbacks.push(onEvent);
    return new Promise((resolve) => resolves.push(resolve));
  };
  const run = startSaga(service);
  await settle();
  run.input.put(runEvaluation(testCases));
  run.input.put(runEvaluation(testCases));
  await settle();
  assert.equal(calls, 1);

  callbacks[0]({ event: "runStart", retrievalType: "hybrid", total: 1 });
  callbacks[0]({
    event: "caseStart",
    retrievalType: "hybrid",
    caseIndex: 1,
    total: 1,
    input: testCases[0],
  });
  callbacks[0]({
    event: "complete",
    retrievalType: "hybrid",
    completed: 1,
    total: 1,
    succeeded: 0,
    failed: 1,
    aggregateScores,
  });
  resolves[0]();
  await settle();
  assert.deepEqual(
    run.actions.map((action) => action.type),
    ["evaluation/started", "evaluation/caseStarted", "evaluation/completed"],
  );

  run.input.put(runEvaluation(testCases));
  await settle();
  assert.equal(calls, 2);
  run.task.cancel();
  resolves[1]();
  await run.task.toPromise();
});

test("transport rejection emits one terminal failure action with the backend message", async () => {
  const run = startSaga(async () => {
    throw "python bridge failed: paper search index unavailable";
  });
  await settle();
  run.input.put(runEvaluation(testCases));
  await settle();
  await settle();

  assert.equal(run.actions.at(-1).type, "evaluation/transportFailed");
  assert.equal(
    run.actions.at(-1).payload[0],
    "python bridge failed: paper search index unavailable",
  );
  assert.equal(
    run.actions.filter((action) => action.type === "evaluation/transportFailed").length,
    1,
  );
  run.task.cancel();
  await run.task.toPromise();
});

test("synchronous retries are buffered, sanitized and replace original rows", async () => {
  const reason = (round) => ({ stage: "planner", category: "payment_required", http_status: 402, retryable: false, retry_round: round, stage_attempt: round + 1, hop_index: null, message: "synthetic-untrusted-canary" });
  const result = (round) => ({ case_index: 1, input: testCases[0], retrieval_strategy: "hybrid", retrieved_chunks: [], generated_answer: null, judge_scores: null, error: reason(round), attempt_history: Array.from({ length: round + 1 }, (_, i) => reason(i)) });
  const run = startSaga(async (_cases, emit) => {
    emit({ event: "runStart", retrievalType: "hybrid", total: 1 });
    emit({ event: "caseFailure", completed: 1, total: 1, result: result(0) });
    for (const retryRound of [1, 2]) {
      emit({ event: "retryRoundStart", retryRound, total: 1, pending: 1 });
      emit({ event: "caseRetryStart", retryRound, caseIndex: 1, total: 1 });
      emit({ event: "caseRetryResult", retryRound, completed: 1, total: 1, result: result(retryRound) });
    }
    emit({ event: "complete", retrievalType: "hybrid", completed: 1, total: 1, succeeded: 0, failed: 1, aggregateScores });
    emit({ event: "error", message: "synthetic-untrusted-canary" });
  });
  run.input.put(runEvaluation(testCases));
  await settle();
  const state = run.actions.reduce((state, action) => evaluationReducer(state, action), undefined);
  assert.equal(state.running, false);
  assert.equal(state.results.length, 1);
  assert.equal(state.results[0].error.category, "payment_required");
  assert.deepEqual(state.results[0].attempt_history.map((r) => r.retry_round), [0, 1, 2]);
  assert.equal(state.completed, 1);
  assert.equal(state.retryRound, 2);
  assert.equal(run.actions.filter((a) => a.type === "evaluation/completed").length, 1);
  assert.doesNotMatch(JSON.stringify(run.actions), /synthetic-untrusted/);
  run.task.cancel();
  await run.task.toPromise();
});
