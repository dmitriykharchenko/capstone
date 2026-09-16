import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import test, { after, before } from "node:test";
import { runSaga } from "redux-saga";
import { createServer } from "vite";

const appRoot = fileURLToPath(new URL("..", import.meta.url));
const settle = () => new Promise((resolve) => setImmediate(resolve));
let vite, sendMessage, streamChatTurn;

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

before(async () => {
  vite = await createServer({
    root: appRoot,
    appType: "custom",
    logLevel: "silent",
    server: { middlewareMode: true },
  });
  ({ streamChatTurn } = await vite.ssrLoadModule("/src/slices/chat/sagas/chat-saga.ts"));
  ({ sendMessage } = await vite.ssrLoadModule("/src/slices/chat/chat-slice.ts"));
});

after(async () => vite.close());

function runTurn(service) {
  const actions = [];
  const task = runSaga(
    {
      dispatch: (action) => actions.push(action),
      getState: () => ({ chat: { sessionId: "session-1" } }),
    },
    streamChatTurn,
    sendMessage("Question"),
    service,
  );
  return { actions, task };
}

test("saga maps every ordered multihop event to chat progress actions", async () => {
  let onEvent;
  const run = runTurn((_sessionId, _message, callback) => {
    onEvent = callback;
    return new Promise(() => {});
  });
  await settle();
  for (const event of [
    { event: "plan", subqueries: [{ id: "subquery-1", index: 1, query: "First" }] },
    { event: "subquery", id: "subquery-1", index: 1, total: 1, query: "First" },
    {
      event: "subanswer",
      id: "subquery-1",
      index: 1,
      total: 1,
      query: "First",
      answer: "Evidence answer",
      sources: [source],
    },
    { event: "delta", delta: "Final" },
    { event: "done", reply: "Final", chatMode: "rag", indexed: true, sources: [] },
  ]) {
    onEvent(event);
    await settle();
  }
  await run.task.toPromise();

  assert.deepEqual(run.actions.map((action) => action.type), [
    "chat/appendMessage",
    "chat/started",
    "chat/planReceived",
    "chat/subqueryStarted",
    "chat/subanswerReceived",
    "chat/chunk",
    "chat/succeeded",
    "sessions/load",
  ]);
});

test("transport failure emits one retryable terminal action", async () => {
  const run = runTurn(async () => {
    throw new Error("bridge disconnected");
  });
  await run.task.toPromise();
  assert.equal(run.actions.at(-1).type, "chat/failed");
  assert.equal(run.actions.filter((action) => action.type === "chat/failed").length, 1);
});