import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import test from "node:test";

const root = fileURLToPath(new URL("..", import.meta.url));

test("evaluation API always requests the canonical multihop hybrid pipeline", async () => {
  const api = await readFile(`${root}/electron/preload.cjs`, "utf8");
  const start = api.indexOf("runEvaluation:");
  const transport = api.slice(start, api.indexOf("\n", start));

  assert.match(transport, /request\('runEvaluation'/);
  assert.match(transport, /retrieval_type: 'hybrid'/);
  assert.doesNotMatch(transport, /retrievalType: EvaluationRetrievalType/);
  assert.match(transport, /test_cases/);
  assert.doesNotMatch(transport, /apiKey|config|Path|directory/i);
});

test("evaluation contracts preserve provenance, judge detail, and aggregates", async () => {
  const types = await readFile(`${root}/src/lib/evaluation-types.ts`, "utf8");
  for (const field of [
    "text: string",
    "paper_id: string",
    "chunk_index: number",
    "generated_answer: string | null",
    "judge_scores: EvaluationJudgeScores | null",
    "context_precision: number | null",
    "hallucination_rate: number | null",
  ]) {
    assert.ok(types.includes(field), field);
  }
  for (const event of ["runStart", "caseStart", "caseSuccess", "caseFailure", "complete"]) {
    assert.ok(types.includes(`event: "${event}"`), event);
  }
  assert.match(types, /EVALUATION_RETRIEVAL_TYPE = "multihop-hybrid" as const/);
  assert.match(types, /EVALUATION_WIRE_RETRIEVAL_TYPE = "hybrid" as const/);
  assert.doesNotMatch(types, /"vector"|"bm25"/);
});