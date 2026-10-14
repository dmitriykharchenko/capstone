import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import { parseEvaluationTestCases } from "../src/lib/evaluation-upload.ts";

const validCases = [
  { question: "What is RAG?", grading_notes: "Retrieval augmented generation", sources: [] },
  { question: "Where?", grading_notes: "In the paper", sources: ["paper.pdf"] },
];

test("uploaded evaluation tests are fully validated before dispatch", () => {
  assert.deepEqual(parseEvaluationTestCases(JSON.stringify(validCases)), {
    ok: true,
    testCases: validCases,
  });
  for (const invalid of [
    [],
    [null],
    [{ question: " ", grading_notes: "answer", sources: [] }],
    [{ question: "question", grading_notes: " ", sources: [] }],
    [{ question: "question", grading_notes: "answer", sources: "paper.pdf" }],
    [{ question: "question", grading_notes: "answer", sources: [" "] }],
  ]) {
    assert.equal(parseEvaluationTestCases(JSON.stringify(invalid)).ok, false);
  }
  assert.equal(parseEvaluationTestCases("not json").ok, false);
});

test("RAG Evals source contract auto-starts and renders every required state", async () => {
  const component = await readFile(
    new URL("../src/components/RagEvals.svelte", import.meta.url),
    "utf8",
  );
  const parsed = component.indexOf("parseEvaluationTestCases(text)");
  const dispatched = component.indexOf("store.dispatch(runEvaluation", parsed);
  assert.ok(parsed >= 0 && dispatched > parsed, "validation must precede auto-start dispatch");
  assert.match(component, /accept="\.json"/);
  assert.doesNotMatch(component, /<Select/);
  assert.match(component, /store\.dispatch\(runEvaluation\(parsed\.testCases\)\)/);
  assert.doesNotMatch(component, /value="(?:vector|bm25|hybrid)"|>Hybrid</);
  assert.ok((component.match(/disabled=\{\$running\}/g) ?? []).length >= 2);
  for (const contract of [
    'role="alert"',
    'role="progressbar"',
    "Evaluation in progress",
    "Partial failure",
    "Evaluation stopped",
    "Evaluation completed",
    "Aggregate means",
    "Context precision",
    "Context recall",
    "Relevant chunk ranking",
    "Context utilization",
    "Semantic similarity",
    "Completeness",
    "Hallucination rate",
    "<details",
    "Generated answer",
    "Retrieved source provenance",
    "Judge results",
    "Relevant chunk ranks",
    "Chunks used",
    "Contradicting claims",
    "Missing elements",
    "Unsupported claims",
    "Evaluation error",
  ]) assert.ok(component.includes(contract), contract);
  assert.doesNotMatch(component, /window\.(location|history)|popstate|api\.runEvaluation/);
});
