import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test, { after, before } from "node:test";
import { fileURLToPath } from "node:url";
import { createServer } from "vite";

const appRoot = fileURLToPath(new URL("..", import.meta.url));
let vite;
let clampEvaluationScore;
let evaluationScoreColor;

before(async () => {
  vite = await createServer({
    root: appRoot,
    appType: "custom",
    logLevel: "silent",
    server: { middlewareMode: true },
  });
  ({ clampEvaluationScore, evaluationScoreColor } = await vite.ssrLoadModule(
    "/src/lib/evaluation-score.ts",
  ));
});

after(async () => vite.close());

test("evaluation score colors clamp safely across the red-to-green scale", () => {
  assert.equal(clampEvaluationScore(-1), 0);
  assert.equal(clampEvaluationScore(6), 5);
  assert.equal(clampEvaluationScore(Number.NaN), 0);
  assert.equal(evaluationScoreColor(0), "rgb(220 38 38)");
  assert.equal(evaluationScoreColor(5), "rgb(22 163 74)");
  assert.equal(evaluationScoreColor(2.5), "rgb(121 101 56)");
  assert.equal(evaluationScoreColor(-10), evaluationScoreColor(0));
  assert.equal(evaluationScoreColor(10), evaluationScoreColor(5));
});

test("collapsed evaluation summaries include readable scores and no-score states", async () => {
  const component = await readFile(
    new URL("../src/components/RagEvals.svelte", import.meta.url),
    "utf8",
  );
  assert.match(component, /<summary[\s\S]*?aria-label=\{summaryScoreAriaLabel/);
  assert.match(component, /return `Judge scores:/);
  const scoreSubtitle = component.match(
    /<span\s+class="([^"]*text-xs[^"]*)"\s+aria-label=\{summaryScoreAriaLabel\(/,
  )?.[1];
  assert.ok(scoreSubtitle?.includes("overflow-x-auto"), "score subtitle must be horizontally reachable");
  assert.ok(!scoreSubtitle?.includes("truncate"), "score subtitle must not be ellipsized");
  for (const label of [
    "Context precision",
    "Context recall",
    "Relevant chunk ranking",
    "Context utilization",
    "Semantic similarity",
    "Completeness",
    "Hallucination rate",
  ]) assert.ok(component.includes(label), label);
  assert.match(component, /evaluationScoreColor\(criterion\.score\)/);
  assert.match(component, /No judge scores available — evaluation failed/);
  assert.match(component, /No judge scores available yet/);
  assert.match(component, /aria-label=\{summaryScoreAriaLabel\(result\.judge_scores\)\}/);
  assert.match(component, /result\.error \? "Failed" : "Completed"/);
});