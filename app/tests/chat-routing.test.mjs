import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import test, { after, before } from "node:test";
import { runSaga, stdChannel } from "redux-saga";
import { createServer } from "vite";

const appRoot = fileURLToPath(new URL("..", import.meta.url));
const settle = () => new Promise((resolve) => setImmediate(resolve));
let vite, chatRouteSaga, routerSaga, routeChanged, replaceWith, navigateTo, activateChat, selectSession, createSession;

before(async () => {
  vite = await createServer({ root: appRoot, appType: "custom", logLevel: "silent", server: { middlewareMode: true } });
  ({ chatRouteSaga } = await vite.ssrLoadModule("/src/slices/chat/sagas/chat-saga.ts"));
  ({ routerSaga } = await vite.ssrLoadModule("/src/slices/router/sagas/router-saga.ts"));
  ({ routeChanged, replaceWith, navigateTo } = await vite.ssrLoadModule("/src/slices/router/router-slice.ts"));
  ({ activateChat, selectSession, createSession } = await vite.ssrLoadModule("/src/slices/chat/chat-slice.ts"));
});

after(async () => vite.close());

function runChatRoute(services) {
  const input = stdChannel();
  const actions = [];
  const task = runSaga({ channel: input, dispatch: (action) => actions.push(action), getState: () => ({}) }, chatRouteSaga, services);
  return { input, actions, task };
}

test("startup resolution replaces the URL before hydrating the chosen session", async () => {
  const input = stdChannel();
  const actions = [];
  const replacements = [];
  const events = [];
  let location = { pathname: "/chat", search: "" };
  const navigation = {
    readLocation: () => location,
    push: () => assert.fail(),
    replace: (url) => {
      replacements.push(url);
      const parsed = new URL(url, "http://chat.test");
      location = { pathname: parsed.pathname, search: parsed.search };
    },
    subscribe: () => () => {},
  };
  const services = {
    listSessions: async () => ({ sessions: [{ session_id: "recent" }] }),
    getMessages: async (id) => {
      events.push(`messages:${id}`);
      return { messages: [] };
    },
    createSessionId: () => "fresh",
    afterRender: async () => {},
    scrollToExchange: () => {},
  };
  const dispatch = (action) => {
    actions.push(action);
    input.put(action);
  };
  const chatTask = runSaga({ channel: input, dispatch, getState: () => ({}) }, chatRouteSaga, services);
  const routerTask = runSaga({ channel: input, dispatch, getState: () => ({}) }, routerSaga, navigation);
  await settle();
  await settle();
  assert.deepEqual(replacements, ["/chat?sessionId=recent"]);
  assert.deepEqual(events, ["messages:recent"]);
  assert.ok(actions.findIndex((action) => action.type === "chat/setSessionId") < actions.findIndex((action) => action.type === "chat/setMessages"));
  routerTask.cancel();
  chatTask.cancel();
  await Promise.all([routerTask.toPromise(), chatTask.toPromise()]);
});

test("Chat tab activation resolves its session before one visible route transition", async () => {
  const input = stdChannel();
  const actions = [];
  const pushes = [];
  const replacements = [];
  const messageLoads = [];
  let location = { pathname: "/history", search: "" };
  const navigation = {
    readLocation: () => location,
    push: (url) => {
      pushes.push(url);
      const parsed = new URL(url, "http://chat.test");
      location = { pathname: parsed.pathname, search: parsed.search };
    },
    replace: (url) => replacements.push(url),
    subscribe: () => () => {},
  };
  const services = {
    listSessions: async () => ({ sessions: [{ session_id: "recent" }] }),
    getMessages: async (id) => {
      messageLoads.push(id);
      return { messages: [] };
    },
    createSessionId: () => "fresh",
    afterRender: async () => {},
    scrollToExchange: () => {},
  };
  const dispatch = (action) => {
    actions.push(action);
    input.put(action);
  };
  const chatTask = runSaga({ channel: input, dispatch, getState: () => ({}) }, chatRouteSaga, services);
  const routerTask = runSaga({ channel: input, dispatch, getState: () => ({}) }, routerSaga, navigation);
  await settle();
  actions.length = 0;

  input.put(activateChat());
  await settle();
  await settle();
  await settle();

  assert.deepEqual(pushes, ["/chat?sessionId=recent"]);
  assert.deepEqual(replacements, []);
  assert.deepEqual(
    actions.filter((action) => action.type === routeChanged.type).map((action) => action.payload[0]),
    [{ name: "chat", sessionId: "recent" }],
  );
  assert.deepEqual(messageLoads, ["recent"]);
  routerTask.cancel();
  chatTask.cancel();
  await Promise.all([routerTask.toPromise(), chatTask.toPromise()]);
});

test("missing session resolves the most recent persisted session", async () => {
  const services = { listSessions: async () => ({ sessions: [{ session_id: "recent" }] }), getMessages: async () => assert.fail(), createSessionId: () => "fresh", afterRender: async () => assert.fail(), scrollToExchange: () => assert.fail() };
  const run = runChatRoute(services);
  run.input.put(routeChanged({ name: "chat" }));
  await settle();
  assert.deepEqual(run.actions.at(-1), replaceWith({ name: "chat", sessionId: "recent" }));
  run.task.cancel();
  await run.task.toPromise();
});

test("empty history creates a fresh session and session intents navigate", async () => {
  const services = { listSessions: async () => ({ sessions: [] }), getMessages: async () => ({ messages: [] }), createSessionId: () => "fresh", afterRender: async () => {}, scrollToExchange: () => {} };
  const run = runChatRoute(services);
  run.input.put(routeChanged({ name: "chat" }));
  await settle();
  assert.deepEqual(run.actions.at(-1), replaceWith({ name: "chat", sessionId: "fresh" }));
  run.input.put(selectSession("selected"));
  await settle();
  assert.deepEqual(run.actions.at(-1), navigateTo({ name: "chat", sessionId: "selected" }));
  run.input.put(createSession());
  await settle();
  assert.deepEqual(run.actions.at(-1), navigateTo({ name: "chat", sessionId: "fresh" }));
  run.task.cancel();
  await run.task.toPromise();
});

test("hydration precedes valid exchange scrolling", async () => {
  const events = [];
  const services = { listSessions: async () => ({ sessions: [] }), getMessages: async () => ({ messages: [{ role: "user", content: "q", exchange_id: 7 }] }), createSessionId: () => "fresh", afterRender: async () => events.push("paint"), scrollToExchange: (id) => events.push(`scroll:${id}`) };
  const run = runChatRoute(services);
  const originalPush = run.actions.push.bind(run.actions);
  run.actions.push = (action) => { if (action.type === "chat/setMessages") events.push("render"); return originalPush(action); };
  run.input.put(routeChanged({ name: "chat", sessionId: "session", exchangeId: 7 }));
  await settle();
  await settle();
  assert.deepEqual(events, ["render", "paint", "scroll:7"]);
  run.task.cancel();
  await run.task.toPromise();
});

test("unknown exchange is canonicalized away without scrolling", async () => {
  let scrolled = false;
  const services = { listSessions: async () => ({ sessions: [] }), getMessages: async () => ({ messages: [] }), createSessionId: () => "fresh", afterRender: async () => {}, scrollToExchange: () => { scrolled = true; } };
  const run = runChatRoute(services);
  run.input.put(routeChanged({ name: "chat", sessionId: "session", exchangeId: 99 }));
  await settle();
  await settle();
  assert.equal(scrolled, false);
  assert.deepEqual(run.actions.at(-1), replaceWith({ name: "chat", sessionId: "session" }));
  run.task.cancel();
  await run.task.toPromise();
});