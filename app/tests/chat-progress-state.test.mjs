import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import test, { after, before } from "node:test";
import { createServer } from "vite";

const appRoot = fileURLToPath(new URL("..", import.meta.url));
let vite, actions, chatReducer;

before(async () => {
  vite = await createServer({
    root: appRoot,
    appType: "custom",
    logLevel: "silent",
    server: { middlewareMode: true },
  });
  actions = await vite.ssrLoadModule("/src/slices/chat/chat-slice.ts");
  ({ chatReducer } = actions);
});

after(async () => vite.close());

const source = {
  rank: 1,
  paper_id: "paper",
  chunk_index: 0,
  filename: "paper.pdf",
  title: "Paper",
  page: 1,
  score: 0.9,
  snippet: "Evidence",
};

test("chat progress follows planner order through subanswers and final stream", () => {
  let state = chatReducer(undefined, actions.chatStarted());
  state = chatReducer(
    state,
    actions.chatPlanReceived([
      { id: "subquery-2", index: 2, query: "Second" },
      { id: "subquery-1", index: 1, query: "First" },
    ]),
  );
  assert.deepEqual(state.activeTurn.subqueries.map(({ id, status }) => [id, status]), [
    ["subquery-1", "pending"],
    ["subquery-2", "pending"],
  ]);

  state = chatReducer(
    state,
    actions.chatSubqueryStarted({ id: "subquery-1", index: 1, query: "First" }),
  );
  state = chatReducer(
    state,
    actions.chatSubanswerReceived("subquery-1", 1, "First", "First answer", [source]),
  );
  state = chatReducer(state, actions.chatChunk("Final "));
  state = chatReducer(state, actions.chatChunk("answer"));

  assert.equal(state.activeTurn.subqueries[0].status, "complete");
  assert.equal(state.activeTurn.subqueries[0].answer, "First answer");
  assert.deepEqual(state.activeTurn.subqueries[0].sources, [source]);
  assert.equal(state.activeTurn.finalAnswer, "Final answer");
  assert.equal(state.streaming, true);
});

test("completion persists only authoritative final answer and clears progress", () => {
  let state = chatReducer(undefined, actions.appendMessage({ role: "user", content: "Question" }));
  state = chatReducer(state, actions.chatStarted());
  state = chatReducer(state, actions.chatChunk("Incomplete"));
  state = chatReducer(state, actions.chatSucceeded("Authoritative", "rag", [source]));

  assert.equal(state.activeTurn, null);
  assert.equal(state.pending, false);
  assert.equal(state.streaming, false);
  assert.equal(Object.hasOwn(state, "expandedSubqueries"), false);
  assert.deepEqual(state.messages, [
    { role: "user", content: "Question" },
    { role: "assistant", content: "Authoritative", sources: [source] },
  ]);
});

test("failure and session intents discard ephemeral evidence details and remain retryable", () => {
  let state = chatReducer(undefined, actions.chatStarted());
  state = chatReducer(state, actions.chatChunk("Incomplete"));
  state = chatReducer(state, actions.chatFailed("transport failed"));
  assert.equal(state.activeTurn, null);
  assert.equal(state.pending, false);
  assert.equal(state.streaming, false);
  assert.equal(state.error, "transport failed");
  assert.equal(Object.hasOwn(state, "expandedSubqueries"), false);

  state = chatReducer(state, actions.chatStarted());
  assert.equal(state.error, null);
  state = chatReducer(state, actions.selectSession("other"));
  assert.equal(state.activeTurn, null);
  assert.equal(Object.hasOwn(state, "expandedSubqueries"), false);
  state = chatReducer(chatReducer(state, actions.chatStarted()), actions.createSession());
  assert.equal(state.activeTurn, null);
  assert.equal(Object.hasOwn(state, "expandedSubqueries"), false);
  state = chatReducer(chatReducer(state, actions.chatStarted()), actions.setSessionId("new"));
  assert.equal(state.activeTurn, null);
  assert.equal(Object.hasOwn(state, "expandedSubqueries"), false);
  const stale = chatReducer(state, actions.chatChunk("late chunk"));
  assert.equal(stale.activeTurn, null);
  assert.equal(stale.streaming, false);
  const lateFailure = chatReducer(stale, actions.chatFailed("late old-session failure"));
  assert.equal(lateFailure.error, null);
  const hydrationFailure = chatReducer(
    lateFailure,
    actions.chatHydrationFailed("history unavailable"),
  );
  assert.equal(hydrationFailure.error, "history unavailable");
});