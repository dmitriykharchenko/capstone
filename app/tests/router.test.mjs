import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import test, { after, before } from "node:test";
import { runSaga, stdChannel } from "redux-saga";
import { createServer } from "vite";

import {
  parseRoute,
  serializeRoute,
} from "../src/slices/router/router-types.ts";

const appRoot = fileURLToPath(new URL("..", import.meta.url));
let vite;
let routerSaga;
let navigateTo;
let routeChanged;
let routerReducer;

before(async () => {
  vite = await createServer({
    root: appRoot,
    appType: "custom",
    logLevel: "silent",
    server: { middlewareMode: true },
  });
  ({ routerSaga } = await vite.ssrLoadModule(
    "/src/slices/router/sagas/router-saga.ts",
  ));
  ({ navigateTo, routeChanged, routerReducer } = await vite.ssrLoadModule(
    "/src/slices/router/router-slice.ts",
  ));
});

after(async () => {
  await vite.close();
});

function parseUrl(url) {
  const parsed = new URL(url, "http://routing.test");
  return { pathname: parsed.pathname, search: parsed.search };
}

function fakeNavigation(initialUrl) {
  let location = parseUrl(initialUrl);
  let listener;
  let unsubscribed = false;
  const pushes = [];
  const replacements = [];
  return {
    adapter: {
      readLocation: () => location,
      push: (url) => {
        pushes.push(url);
        location = parseUrl(url);
      },
      replace: (url) => {
        replacements.push(url);
        location = parseUrl(url);
      },
      subscribe: (next) => {
        listener = next;
        return () => {
          unsubscribed = true;
          listener = undefined;
        };
      },
    },
    emit(url) {
      location = parseUrl(url);
      listener?.(location);
    },
    pushes,
    replacements,
    wasUnsubscribed: () => unsubscribed,
  };
}

const settle = () => new Promise((resolve) => setImmediate(resolve));

test("route parsing and serialization safely round-trip parameters", () => {
  const chat = parseRoute({
    pathname: "/chat",
    search: "?sessionId=session%2Fone&exchangeId=42",
  });
  assert.deepEqual(chat, {
    name: "chat",
    sessionId: "session/one",
    exchangeId: 42,
  });
  assert.equal(
    serializeRoute(chat),
    "/chat?sessionId=session%2Fone&exchangeId=42",
  );
  assert.equal(
    serializeRoute({ name: "history", query: "RAG & routing/URLs" }),
    "/history?q=RAG+%26+routing%2FURLs",
  );
  assert.deepEqual(
    parseRoute({ pathname: "/papers", search: "?q=%E0%A4%A" }),
    { name: "papers", query: "�%A" },
  );
  assert.deepEqual(parseRoute({ pathname: "/evals", search: "?ignored=1" }), {
    name: "evals",
  });
  assert.equal(serializeRoute({ name: "evals" }), "/evals");
  assert.deepEqual(parseRoute({ pathname: "/graph", search: "" }), { name: "chat" });
});

test("unknown paths and empty parameters fall back to canonical chat", () => {
  assert.deepEqual(
    parseRoute({ pathname: "/unknown", search: "?sessionId=ignored" }),
    { name: "chat" },
  );
  assert.deepEqual(parseRoute({ pathname: "/history", search: "?q=" }), {
    name: "history",
    query: undefined,
  });
  assert.equal(serializeRoute({ name: "chat" }), "/chat");
  assert.deepEqual(
    parseRoute({ pathname: "/chat", search: "?sessionId=s&exchangeId=zero" }),
    { name: "chat", sessionId: "s", exchangeId: undefined },
  );
});

test("router reducer records startup synchronization", () => {
  const initial = routerReducer(undefined, { type: "test/init" });
  assert.equal(initial.initialized, false);
  const changed = routerReducer(initial, routeChanged({ name: "history" }));
  assert.equal(changed.initialized, true);
  assert.deepEqual(changed.activeRoute, { name: "history" });
});

test("saga canonicalizes startup, pushes intents, and consumes popstate", async () => {
  const browser = fakeNavigation("/not-found?old=value");
  const input = stdChannel();
  const actions = [];
  const task = runSaga(
    { channel: input, dispatch: (action) => actions.push(action), getState: () => ({}) },
    routerSaga,
    browser.adapter,
  );

  await settle();
  assert.deepEqual(browser.replacements, ["/chat"]);
  assert.deepEqual(actions.at(-1).payload[0], { name: "chat" });

  input.put(navigateTo({ name: "papers", query: "vector search" }));
  await settle();
  assert.deepEqual(browser.pushes, ["/papers?q=vector+search"]);
  assert.deepEqual(actions.at(-1).payload[0], {
    name: "papers",
    query: "vector search",
  });

  browser.emit("/history?q=prior+turns");
  await settle();
  assert.deepEqual(actions.at(-1).payload[0], {
    name: "history",
    query: "prior turns",
  });
  assert.deepEqual(browser.pushes, ["/papers?q=vector+search"]);

  browser.emit("/evals");
  await settle();
  assert.deepEqual(actions.at(-1).payload[0], { name: "evals" });

  task.cancel();
  await task.toPromise();
  assert.equal(browser.wasUnsubscribed(), true);
});

test("known direct navigation initializes without rewriting history", async () => {
  const browser = fakeNavigation("/chat?sessionId=s-1&exchangeId=2");
  const actions = [];
  const task = runSaga(
    { channel: stdChannel(), dispatch: (action) => actions.push(action), getState: () => ({}) },
    routerSaga,
    browser.adapter,
  );
  await settle();
  assert.deepEqual(browser.replacements, []);
  assert.deepEqual(actions.at(-1).payload[0], {
    name: "chat",
    sessionId: "s-1",
    exchangeId: 2,
  });
  task.cancel();
  await task.toPromise();
});

test("direct evaluation navigation is canonical and survives back-forward events", async () => {
  const browser = fakeNavigation("/evals");
  const actions = [];
  const input = stdChannel();
  const task = runSaga(
    { channel: input, dispatch: (action) => actions.push(action), getState: () => ({}) },
    routerSaga,
    browser.adapter,
  );
  await settle();
  assert.deepEqual(browser.replacements, []);
  assert.deepEqual(actions.at(-1).payload[0], { name: "evals" });

  input.put(navigateTo({ name: "papers" }));
  await settle();
  assert.deepEqual(browser.pushes, ["/papers"]);
  browser.emit("/evals?ignored=1");
  await settle();
  assert.deepEqual(browser.replacements, ["/evals"]);
  assert.deepEqual(actions.at(-1).payload[0], { name: "evals" });
  task.cancel();
  await task.toPromise();
});

test("invalid exchange identifiers are removed from direct chat URLs", async () => {
  const browser = fakeNavigation("/chat?sessionId=s-1&exchangeId=not-a-number");
  const actions = [];
  const task = runSaga(
    { channel: stdChannel(), dispatch: (action) => actions.push(action), getState: () => ({}) },
    routerSaga,
    browser.adapter,
  );
  await settle();
  assert.deepEqual(browser.replacements, ["/chat?sessionId=s-1"]);
  assert.deepEqual(actions.at(-1).payload[0], {
    name: "chat",
    sessionId: "s-1",
    exchangeId: undefined,
  });
  task.cancel();
  await task.toPromise();
});
