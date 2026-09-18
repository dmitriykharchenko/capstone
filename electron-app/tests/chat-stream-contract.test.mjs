import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const api = readFileSync(new URL("../src/lib/api.ts", import.meta.url), "utf8");

test("ChatStreamEvent exposes ordered multihop transport stages", () => {
  for (const contract of [
    'event: "plan"; subqueries: ChatSubquery[]',
    'event: "subquery";',
    'event: "subanswer";',
    'event: "coverage";',
    'status: ChatCoverageStatus',
    'event: "delta"; delta: string',
    'event: "done";',
    'event: "error"; message: string',
  ]) {
    assert.ok(api.includes(contract), `missing ${contract}`);
  }
});

test("subquery progress has stable identity and ordering fields", () => {
  for (const field of [
    "id: string",
    "index: number",
    "total: number",
    "query: string",
    "answer: string",
    "sources: ChatSource[]",
  ]) {
    assert.ok(api.includes(field), `missing ${field}`);
  }
});

test("adaptive coverage contract is public-safe and bounded", () => {
  assert.ok(api.includes('"reviewing" | "complete" | "followup"'));
  assert.ok(api.includes("subqueries: ChatSubquery[]"));
  assert.doesNotMatch(api, /coverageReason|privateReasoning|chainOfThought/);
});