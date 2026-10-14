import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const source = (path) => readFile(new URL(path, import.meta.url), "utf8");

test("typed native library wrappers match the approved command contract", async () => {
  const api = await source("../src/lib/api.ts");
  assert.match(api, /interface LibrarySelection[\s\S]*path: string;[\s\S]*dataPath: string;/);
  assert.match(api, /interface ChooseLibraryResponse[\s\S]*changed: boolean;[\s\S]*cancelled: boolean;/);
  assert.match(api, /invoke<LibrarySelection \| null>\("get_current_library"\)/);
  assert.match(api, /invoke<ChooseLibraryResponse>\("choose_library"\)/);
});

test("startup gates domain loads on selection and never starts provider or index work", async () => {
  const store = await source("../src/store.ts");
  const initialize = store.indexOf("call(initializeLibrary");
  const readyGuard = store.indexOf("if (!initialized) return", initialize);
  const domainSagas = store.indexOf("fork(chatSaga)", readyGuard);
  const sessions = store.indexOf("put(loadSessions())", domainSagas);
  const papers = store.indexOf("put(loadPapers())", sessions);
  assert.ok(initialize >= 0 && readyGuard > initialize);
  assert.ok(domainSagas > readyGuard && sessions > domainSagas && papers > sessions);
  assert.doesNotMatch(store, /runPaperIndex|runEvaluation|sendMessage/);
});

test("changed selection clears URL-scoped stale state before a full reload", async () => {
  const saga = await source("../src/slices/library/sagas/library-saga.ts");
  const clearRoute = saga.indexOf('replaceState(null, "", "/papers")');
  const reload = saga.indexOf("window.location.reload()", clearRoute);
  assert.ok(clearRoute >= 0 && reload > clearRoute);
  assert.match(saga, /if \(result\.changed\)[\s\S]*call\(services\.reloadApp\)/);
  assert.doesNotMatch(saga, /localStorage|sessionStorage/);
});

test("Papers presents an accessible chooser and truncates long Unicode paths", async () => {
  const [chooser, papers, app] = await Promise.all([
    source("../src/components/LibraryChooser.svelte"),
    source("../src/components/PapersList.svelte"),
    source("../src/App.svelte"),
  ]);
  assert.match(papers, /<LibraryChooser \/>/);
  assert.match(chooser, /Open folder/);
  assert.match(chooser, /aria-label="Open Papers folder"/);
  assert.match(chooser, /class="block truncate text-xs" title=\{displayPath\}/);
  assert.match(chooser, /Default papers folder \(no custom folder selected\)/);
  assert.match(app, /Papers folder unavailable/);
  assert.match(app, /Choose another folder to continue/);
});