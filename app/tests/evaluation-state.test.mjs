import assert from "node:assert/strict";
import test from "node:test";

import {
  beginEvaluation,
  completeEvaluation,
  failEvaluation,
  initialEvaluationState,
  recordEvaluationCase,
  startEvaluationCase,
} from "../src/slices/evaluation/evaluation-types.ts";

const cases = [
  { question: "first", grading_notes: "one", sources: ["a.pdf"] },
  { question: "second", grading_notes: "two", sources: [] },
];

function failedResult(caseIndex) {
  return {
    case_index: caseIndex,
    input: cases[caseIndex - 1],
    retrieval_strategy: "hybrid",
    retrieved_chunks: [],
    generated_answer: null,
    judge_scores: null,
    error: { stage: "retrieval", message: "no chunks" },
  };
}

const aggregates = {
  context_precision: 4,
  context_recall: 3,
  relevant_chunk_ranking: 2,
  context_utilization: 4,
  semantic_similarity: 5,
  completeness: 3,
  hallucination_rate: 1,
};

test("evaluation state preserves input and result order", () => {
  const started = beginEvaluation(initialEvaluationState, "hybrid", cases, 2);
  const current = startEvaluationCase(started, 2);
  const second = recordEvaluationCase(current, failedResult(2), 1, 2);
  const first = recordEvaluationCase(second, failedResult(1), 2, 2);

  assert.deepEqual(started.testCases, cases);
  assert.notEqual(started.testCases, cases);
  assert.equal(current.currentCaseIndex, 2);
  assert.deepEqual(first.results.map((result) => result.case_index), [1, 2]);
  assert.equal(first.failed, 2);
});

test("terminal completion records every aggregate and unlocks the run", () => {
  const state = completeEvaluation(
    beginEvaluation(initialEvaluationState, "vector", cases, 2),
    2,
    2,
    1,
    1,
    aggregates,
  );

  assert.equal(state.running, false);
  assert.equal(state.currentCaseIndex, null);
  assert.equal(state.succeeded, 1);
  assert.equal(state.failed, 1);
  assert.deepEqual(state.aggregateScores, aggregates);
});

test("transport failure terminates cleanly and remains retryable", () => {
  const failed = failEvaluation(
    beginEvaluation(initialEvaluationState, "bm25", cases, 2),
    "bridge disconnected",
  );
  const retry = beginEvaluation(failed, "bm25", cases, 2);

  assert.equal(failed.running, false);
  assert.equal(failed.error, "bridge disconnected");
  assert.equal(retry.running, true);
  assert.equal(retry.error, null);
});