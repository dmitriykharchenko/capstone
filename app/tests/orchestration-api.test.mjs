import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import ts from "typescript";

const source = readFileSync(new URL("../src/lib/orchestration-api.ts", import.meta.url), "utf8");
const compiled = ts.transpileModule(source.replace('import { Channel, invoke } from "@tauri-apps/api/core";', ""), {
  compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 },
}).outputText;

function load() {
  const calls = [];
  class Channel {}
  const exports = {};
  new Function("exports", "Channel", "invoke", compiled)(exports, Channel, (command, args) => { calls.push({ command, args }); return Promise.resolve(); });
  return { ...exports, calls };
}

test("commands carry only session/run/turn IDs and the public query", async () => {
  const { orchestrationApi: api, calls } = load();
  const identity = { session_id: "session", run_id: "run", turn_id: "turn" };
  const seen = [];
  await api.startOrchestration(identity, "Question", e => seen.push(e));
  calls[0].args.onEvent.onmessage({ event: "snapshot" });
  await api.resumeOrchestration(identity, () => {});
  await api.pendingOrchestration("session");
  await api.discardOrchestration(identity);
  await api.orchestrationTrace("session", 42);
  assert.deepEqual(calls.map(c => c.command), ["orchestration_start", "orchestration_resume", "orchestration_pending", "orchestration_discard", "orchestration_trace"]);
  assert.deepEqual(Object.keys(calls[0].args).sort(), ["message", "onEvent", "runId", "sessionId", "turnId"]);
  assert.deepEqual(calls[3].args, { sessionId: "session", runId: "run", turnId: "turn" });
  assert.deepEqual(calls[4].args, { sessionId: "session", exchangeId: 42 });
  assert.equal(seen.length, 1);
});

test("only finalizer text maps into legacy visible streaming", () => {
  const { legacyOrchestrationEvent: legacy } = load();
  for (const event of ["snapshot", "agent_started", "agent_completed", "agent_retried", "tool_started", "tool_completed", "question_added", "question_status", "question_answered", "question_failed", "evidence", "warning"]) {
    assert.equal(legacy({ event, delta: "Intermediate text" }), null);
  }
  assert.deepEqual(legacy({ event: "final_delta", delta: "Final answer" }), { event: "delta", delta: "Final answer" });
  assert.deepEqual(legacy({ event: "done", reply: "Final answer", chat_mode: "mock", indexed: false, sources: [] }), { event: "done", reply: "Final answer", chatMode: "mock", indexed: false, sources: [] });
});