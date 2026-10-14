import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import test, { after, before } from "node:test";
import { runSaga, stdChannel } from "redux-saga";
import { createServer } from "vite";

const appRoot = fileURLToPath(new URL("..", import.meta.url));
const settle = () => new Promise((resolve) => setImmediate(resolve));
let vite;
let chooseLibrary, initializeLibrary, libraryReducer, librarySaga, libraryLoaded;

before(async () => {
  vite = await createServer({
    root: appRoot,
    appType: "custom",
    logLevel: "silent",
    server: { middlewareMode: true },
  });
  ({ initializeLibrary, librarySaga } = await vite.ssrLoadModule(
    "/src/slices/library/sagas/library-saga.ts",
  ));
  ({ chooseLibrary, libraryLoaded, libraryReducer } = await vite.ssrLoadModule(
    "/src/slices/library/library-slice.ts",
  ));
});

after(async () => vite.close());

const libraryA = {
  path: "/Papers A/研究",
  dataPath: "/Papers A/研究/.research-assistant",
};
const libraryB = {
  path: "/Papers B",
  dataPath: "/Papers B/.research-assistant",
};

function idleState(overrides = {}) {
  return {
    chat: { pending: false, streaming: false },
    sessions: { loading: false },
    papers: {
      loading: false,
      pdfLoading: false,
      indexing: false,
      resetting: false,
    },
    search: { searching: false },
    paperSearch: { searching: false },
    evaluation: { running: false },
    ...overrides,
  };
}

function startChooser(services, state = idleState()) {
  const input = stdChannel();
  const actions = [];
  const task = runSaga(
    {
      channel: input,
      dispatch: (action) => actions.push(action),
      getState: () => state,
    },
    librarySaga,
    services,
  );
  return { actions, input, task };
}

test("bootstrap resolves the native selection before dispatching any domain load", async () => {
  const calls = [];
  const actions = [];
  const task = runSaga(
    { dispatch: (action) => actions.push(action), getState: () => ({}) },
    initializeLibrary,
    {
      getCurrentLibrary: async () => {
        calls.push("getCurrentLibrary");
        return libraryA;
      },
      chooseLibrary: async () => assert.fail("must not choose during bootstrap"),
      reloadApp: () => assert.fail("must not reload during bootstrap"),
    },
  );

  assert.equal(await task.toPromise(), true);
  assert.deepEqual(calls, ["getCurrentLibrary"]);
  assert.deepEqual(actions.map((action) => action.type), [
    "library/loading",
    "library/loaded",
  ]);
  assert.deepEqual(actions[1].payload[0], libraryA);
});

test("invalid saved selection stops bootstrap with a recoverable error", async () => {
  const actions = [];
  const task = runSaga(
    { dispatch: (action) => actions.push(action), getState: () => ({}) },
    initializeLibrary,
    {
      getCurrentLibrary: async () => {
        throw "library selection failed: persisted library is unavailable";
      },
      chooseLibrary: async () => assert.fail(),
      reloadApp: () => assert.fail(),
    },
  );

  assert.equal(await task.toPromise(), false);
  assert.equal(actions.at(-1).type, "library/loadFailed");
  assert.match(actions.at(-1).payload[0], /persisted library is unavailable/);
});

test("cancel and same-folder selection preserve state without reload", async () => {
  const responses = [
    { library: libraryA, changed: false, cancelled: true },
    { library: libraryA, changed: false, cancelled: false },
  ];
  let reloads = 0;
  const run = startChooser({
    getCurrentLibrary: async () => libraryA,
    chooseLibrary: async () => responses.shift(),
    reloadApp: () => reloads += 1,
  });

  for (let index = 0; index < 2; index += 1) {
    run.input.put(chooseLibrary());
    await settle();
  }
  assert.equal(reloads, 0);
  assert.deepEqual(run.actions.map((action) => action.type), [
    "library/choicePreserved",
    "library/choicePreserved",
  ]);

  let state = libraryReducer(undefined, libraryLoaded(libraryA));
  state = libraryReducer(state, chooseLibrary());
  state = libraryReducer(state, run.actions[0]);
  assert.equal(state.current.path, libraryA.path);
  assert.equal(state.choosing, false);
  run.task.cancel();
  await run.task.toPromise();
});

test("A to B to A changes each cross the clean reload boundary", async () => {
  const responses = [
    { library: libraryB, changed: true, cancelled: false },
    { library: libraryA, changed: true, cancelled: false },
  ];
  const reloads = [];
  const run = startChooser({
    getCurrentLibrary: async () => libraryA,
    chooseLibrary: async () => responses.shift(),
    reloadApp: () => reloads.push("reload"),
  });

  run.input.put(chooseLibrary());
  await settle();
  run.input.put(chooseLibrary());
  await settle();
  assert.deepEqual(reloads, ["reload", "reload"]);
  assert.deepEqual(run.actions, []);
  run.task.cancel();
  await run.task.toPromise();
});

test("active work blocks the chooser before the native API", async () => {
  const activeStates = [
    { chat: { pending: true, streaming: false } },
    { chat: { pending: false, streaming: true } },
    { sessions: { loading: true } },
    { papers: { loading: true, pdfLoading: false, indexing: false, resetting: false } },
    { papers: { loading: false, pdfLoading: true, indexing: false, resetting: false } },
    { papers: { loading: false, pdfLoading: false, indexing: true, resetting: false } },
    { papers: { loading: false, pdfLoading: false, indexing: false, resetting: true } },
    { evaluation: { running: true } },
    { search: { searching: true } },
    { paperSearch: { searching: true } },
  ];
  for (const active of activeStates) {
    let calls = 0;
    const run = startChooser({
      getCurrentLibrary: async () => libraryA,
      chooseLibrary: async () => {
        calls += 1;
        return { library: libraryB, changed: true, cancelled: false };
      },
      reloadApp: () => assert.fail(),
    }, idleState(active));
    run.input.put(chooseLibrary());
    await settle();
    assert.equal(calls, 0);
    assert.equal(run.actions.at(-1).type, "library/choiceFailed");
    assert.match(run.actions.at(-1).payload[0], /Finish the active/);
    run.task.cancel();
    await run.task.toPromise();
  }
});

test("native busy and validation errors keep the current library retryable", async () => {
  const run = startChooser({
    getCurrentLibrary: async () => libraryA,
    chooseLibrary: async () => {
      throw "library selection failed: cannot change libraries while an operation is active";
    },
    reloadApp: () => assert.fail(),
  });
  run.input.put(chooseLibrary());
  await settle();
  assert.equal(run.actions.at(-1).type, "library/choiceFailed");
  assert.match(run.actions.at(-1).payload[0], /cannot change libraries/);

  let state = libraryReducer(undefined, libraryLoaded(libraryA));
  state = libraryReducer(state, chooseLibrary());
  state = libraryReducer(state, run.actions.at(-1));
  assert.equal(state.current.path, libraryA.path);
  assert.equal(state.choosing, false);
  assert.match(state.error, /cannot change libraries/);
  run.task.cancel();
  await run.task.toPromise();
});