import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import test, { after, before } from "node:test";
import { runSaga, stdChannel } from "redux-saga";
import { createServer } from "vite";

const appRoot = fileURLToPath(new URL("..", import.meta.url));
const settle = () => new Promise((resolve) => setImmediate(resolve));
let vite, chatSaga, sendMessage, setSessionId, streamChatTurn;

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
  ({ chatSaga, streamChatTurn } = await vite.ssrLoadModule(
    "/src/slices/chat/sagas/chat-saga.ts",
  ));
  ({ sendMessage, setSessionId } = await vite.ssrLoadModule(
    "/src/slices/chat/chat-slice.ts",
  ));
});

after(async () => vite.close());

function runTurn(service) {
  const input = stdChannel();
  const actions = [];
  const task = runSaga(
    {
      channel: input,
      dispatch: (action) => actions.push(action),
      getState: () => ({ chat: { sessionId: "session-1" } }),
    },
    streamChatTurn,
    sendMessage("Question"),
    service,
  );
  return { actions, input, task };
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

test("session reset cancels the old worker and immediately permits a new send", async () => {
  const input = stdChannel();
  const actions = [];
  const callbacks = [];
  let sessionId = "old-session";
  const service = (_sessionId, _message, callback) => {
    callbacks.push(callback);
    return new Promise(() => {});
  };
  const task = runSaga(
    {
      channel: input,
      dispatch: (action) => actions.push(action),
      getState: () => ({ chat: { sessionId } }),
    },
    chatSaga,
    service,
  );
  await settle();

  input.put(sendMessage("Old question"));
  await settle();
  input.put(setSessionId("new-session"));
  sessionId = "new-session";
  await settle();
  callbacks[0]({ event: "error", message: "late old-session failure" });
  await settle();

  input.put(sendMessage("New question"));
  await settle();
  assert.equal(callbacks.length, 2);
  assert.equal(actions.some((action) => action.type === "chat/failed"), false);

  callbacks[1]({
    event: "done",
    reply: "New answer",
    chatMode: "rag",
    indexed: true,
    sources: [],
  });
  await settle();
  assert.equal(actions.at(-2).type, "chat/succeeded");
  task.cancel();
  await task.toPromise();
});