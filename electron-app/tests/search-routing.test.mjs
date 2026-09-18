import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import test, { after, before } from "node:test";
import { runSaga, stdChannel } from "redux-saga";
import { createServer } from "vite";

const appRoot = fileURLToPath(new URL("..", import.meta.url));
const settle = () => new Promise((resolve) => setImmediate(resolve));
let vite;
let api;
let searchSaga;
let paperSearchSaga;
let runSearch;
let runPaperSearch;
let routeChanged;
let replaceWith;

before(async () => {
  vite = await createServer({
    root: appRoot,
    appType: "custom",
    logLevel: "silent",
    server: { middlewareMode: true },
  });
  ({ api } = await vite.ssrLoadModule("/src/lib/api.ts"));
  ({ searchSaga } = await vite.ssrLoadModule(
    "/src/slices/search/sagas/search-saga.ts",
  ));
  ({ paperSearchSaga } = await vite.ssrLoadModule(
    "/src/slices/paper-search/sagas/paper-search-saga.ts",
  ));
  ({ runSearch } = await vite.ssrLoadModule("/src/slices/search/search-slice.ts"));
  ({ runPaperSearch } = await vite.ssrLoadModule(
    "/src/slices/paper-search/paper-search-slice.ts",
  ));
  ({ routeChanged, replaceWith } = await vite.ssrLoadModule(
    "/src/slices/router/router-slice.ts",
  ));
});

after(async () => {
  await vite.close();
});

function start(saga, state) {
  const channel = stdChannel();
  const actions = [];
  const task = runSaga(
    { channel, dispatch: (action) => actions.push(action), getState: () => state },
    saga,
  );
  return { actions, channel, task };
}

test("history intent updates the URL before the route starts a limited search", async () => {
  const calls = [];
  api.search = async (...args) => {
    calls.push(args);
    return { enabled: true, matches: [] };
  };
  const running = start(searchSaga, {
    router: { activeRoute: { name: "history" } },
    search: { error: null },
  });

  running.channel.put(runSearch("  RAG & URLs  "));
  await settle();
  assert.deepEqual(running.actions[0].payload[0], {
    name: "history",
    query: "RAG & URLs",
  });
  assert.deepEqual(calls, []);

  running.channel.put(routeChanged({ name: "history", query: "RAG & URLs" }));
  await settle();
  assert.deepEqual(calls, [["RAG & URLs", 5]]);
  assert.equal(running.actions.at(-1).type, "search/succeeded");
  running.task.cancel();
  await running.task.toPromise();
});

test("submitting the active query does not duplicate navigation or requests", async () => {
  let calls = 0;
  api.search = async () => {
    calls += 1;
    return { enabled: true, matches: [] };
  };
  const running = start(searchSaga, {
    router: { activeRoute: { name: "history", query: "same query" } },
    search: { error: null },
  });

  running.channel.put(runSearch(" same query "));
  await settle();
  assert.deepEqual(running.actions, []);
  assert.equal(calls, 0);
  running.task.cancel();
  await running.task.toPromise();
});

test("route changes clear, canonicalize, and take only the latest history result", async () => {
  const resolvers = new Map();
  api.search = (query) => new Promise((resolve) => resolvers.set(query, resolve));
  const running = start(searchSaga, {
    router: { activeRoute: { name: "chat" } },
    search: { error: null },
  });

  running.channel.put(routeChanged({ name: "history", query: "  spaced  " }));
  await settle();
  assert.deepEqual(running.actions[0], replaceWith({
    name: "history",
    query: "spaced",
  }));
  assert.deepEqual(running.actions[0].payload[0], {
    name: "history",
    query: "spaced",
  });
  assert.equal(resolvers.size, 0);

  running.channel.put(routeChanged({ name: "history", query: "first" }));
  await settle();
  running.channel.put(routeChanged({ name: "history", query: "second" }));
  await settle();
  resolvers.get("second")({ enabled: true, matches: [{ exchange_id: 2 }] });
  await settle();
  resolvers.get("first")({ enabled: true, matches: [{ exchange_id: 1 }] });
  await settle();
  const successes = running.actions.filter((action) => action.type === "search/succeeded");
  assert.equal(successes.length, 1);
  assert.equal(successes[0].payload[1][0].exchange_id, 2);

  running.channel.put(routeChanged({ name: "history" }));
  await settle();
  assert.equal(running.actions.at(-1).type, "search/clear");
  running.task.cancel();
  await running.task.toPromise();
});

test("paper intents and route changes preserve the paper search limit and errors", async () => {
  api.searchPapers = async (query, limit) => {
    assert.equal(query, "vector/graph");
    assert.equal(limit, 8);
    throw new Error("paper search failed");
  };
  const running = start(paperSearchSaga, {
    router: { activeRoute: { name: "papers" } },
    paperSearch: { error: null },
  });

  running.channel.put(runPaperSearch(" vector/graph "));
  await settle();
  assert.deepEqual(running.actions[0].payload[0], {
    name: "papers",
    query: "vector/graph",
  });
  running.channel.put(routeChanged({ name: "papers", query: "  vector/graph  " }));
  await settle();
  assert.deepEqual(
    running.actions.at(-1),
    replaceWith({ name: "papers", query: "vector/graph" }),
  );
  running.channel.put(routeChanged({ name: "papers", query: "vector/graph" }));
  await settle();
  assert.equal(running.actions.at(-1).type, "paperSearch/failed");

  running.channel.put(routeChanged({ name: "papers" }));
  await settle();
  assert.equal(running.actions.at(-1).type, "paperSearch/clear");
  running.task.cancel();
  await running.task.toPromise();
});

test("search components dispatch intents without owning APIs or browser history", async () => {
  const [history, papers] = await Promise.all([
    readFile(new URL("../src/components/HistorySearch.svelte", import.meta.url), "utf8"),
    readFile(new URL("../src/components/PapersList.svelte", import.meta.url), "utf8"),
  ]);
  assert.match(history, /selectActiveRoute/);
  assert.match(history, /sessionId: match\.session_id/);
  assert.match(history, /exchangeId: match\.exchange_id/);
  assert.match(papers, /selectActiveRoute/);
  assert.match(papers, /runPaperSearch\(""\)/);
  for (const component of [history, papers]) {
    assert.doesNotMatch(component, /api\.|window\.(location|history)|popstate/);
  }
});