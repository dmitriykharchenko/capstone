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
  const state = evaluationReducer(undefined, runEvaluation("vector", testCases));
  const duplicate = evaluationReducer(state, runEvaluation("hybrid", testCases));
  assert.equal(state.running, true);
  assert.equal(state.total, 1);
  assert.deepEqual(state.testCases, testCases);
  assert.equal(duplicate, state);
});

test("takeLeading ignores duplicate evaluation requests until completion", async () => {
  const callbacks = [];
  const resolves = [];
  let calls = 0;
  const service = (_retrievalType, _cases, onEvent) => {
    calls += 1;
    callbacks.push(onEvent);
    return new Promise((resolve) => resolves.push(resolve));
  };
  const run = startSaga(service);
  await settle();
  run.input.put(runEvaluation("hybrid", testCases));
  run.input.put(runEvaluation("vector", testCases));
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

  run.input.put(runEvaluation("vector", testCases));
  await settle();
  assert.equal(calls, 2);
  run.task.cancel();
  resolves[1]();
  await run.task.toPromise();
});

test("transport rejection emits one terminal failure action", async () => {
  const run = startSaga(async () => {
    throw new Error("api_key=secret at /private/secret/tests/judge.md");
  });
  await settle();
  run.input.put(runEvaluation("bm25", testCases));
  await settle();
  await settle();

  assert.equal(run.actions.at(-1).type, "evaluation/transportFailed");
  assert.equal(
    run.actions.at(-1).payload[0],
    "Evaluation transport failed. Check the backend logs and retry.",
  );
  assert.doesNotMatch(JSON.stringify(run.actions), /secret|api_key/);
  run.task.cancel();
  await run.task.toPromise();
});