import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import test, { after, before } from "node:test";
import { createServer } from "vite";

let vite;
let search;
let paperSearch;
let searchSelectors;
let paperSearchSelectors;

before(async () => {
  vite = await createServer({
    root: fileURLToPath(new URL("..", import.meta.url)),
    appType: "custom",
    logLevel: "silent",
    server: { middlewareMode: true },
  });
  search = await vite.ssrLoadModule("/src/slices/search/search-slice.ts");
  searchSelectors = await vite.ssrLoadModule(
    "/src/slices/search/search-selectors.ts",
  );
  paperSearch = await vite.ssrLoadModule(
    "/src/slices/paper-search/paper-search-slice.ts",
  );
  paperSearchSelectors = await vite.ssrLoadModule(
    "/src/slices/paper-search/paper-search-selectors.ts",
  );
});

after(async () => vite.close());

test("chat-history search retains and replaces query-keyed collections", () => {
  const first = [{ exchange_id: 1 }];
  const second = [{ exchange_id: 2 }];
  let state = search.searchReducer(undefined, search.searchSucceeded("first", first, true, null));
  state = search.searchReducer(state, search.searchSucceeded("second", second, true, null));
  assert.deepEqual(state.collections, {
    idField: "id",
    ids: ["CHAT_HISTORY_SEARCH:first", "CHAT_HISTORY_SEARCH:second"],
    map: {
      "CHAT_HISTORY_SEARCH:first": { id: "CHAT_HISTORY_SEARCH:first", results: first },
      "CHAT_HISTORY_SEARCH:second": { id: "CHAT_HISTORY_SEARCH:second", results: second },
    },
    refsCount: {},
  });
  state = search.searchReducer(state, search.searchSucceeded("first", [{ exchange_id: 3 }], true, null));
  assert.deepEqual(state.collections.map["CHAT_HISTORY_SEARCH:first"].results, [{ exchange_id: 3 }]);
  assert.deepEqual(
    searchSelectors.selectSearchCollections.select({ search: state }),
    [
      { id: "CHAT_HISTORY_SEARCH:first", results: [{ exchange_id: 3 }] },
      { id: "CHAT_HISTORY_SEARCH:second", results: second },
    ],
  );
  assert.deepEqual(
    searchSelectors.selectSearchMatches.select({
      search: state,
      router: { activeRoute: { name: "history", query: " first " } },
    }),
    [{ exchange_id: 3 }],
  );
  assert.deepEqual(
    searchSelectors.selectSearchMatches.select({
      search: state,
      router: { activeRoute: { name: "history", query: "missing" } },
    }),
    [],
  );
  const cleared = search.searchReducer(state, search.clearSearch());
  assert.deepEqual(cleared.collections, state.collections);
});

test("paper search retains and replaces query-keyed collections", () => {
  const first = [{ paper_id: "paper-1" }];
  const second = [{ paper_id: "paper-2" }];
  let state = paperSearch.paperSearchReducer(
    undefined,
    paperSearch.paperSearchSucceeded("first", first, true, null),
  );
  state = paperSearch.paperSearchReducer(
    state,
    paperSearch.paperSearchSucceeded("second", second, true, null),
  );
  assert.deepEqual(state.collections, {
    idField: "id",
    ids: ["PAPERS_SEARCH:first", "PAPERS_SEARCH:second"],
    map: {
      "PAPERS_SEARCH:first": { id: "PAPERS_SEARCH:first", results: first },
      "PAPERS_SEARCH:second": { id: "PAPERS_SEARCH:second", results: second },
    },
    refsCount: {},
  });
  assert.deepEqual(
    paperSearchSelectors.selectPaperSearchMatches.select({
      paperSearch: state,
      router: { activeRoute: { name: "papers", query: "missing" } },
    }),
    [],
  );
  assert.deepEqual(
    paperSearchSelectors.selectPaperSearchMatches.select({
      paperSearch: state,
      router: { activeRoute: { name: "papers", query: " second " } },
    }),
    second,
  );
  assert.deepEqual(
    paperSearchSelectors.selectPaperSearchCollections.select({ paperSearch: state }),
    [
      { id: "PAPERS_SEARCH:first", results: first },
      { id: "PAPERS_SEARCH:second", results: second },
    ],
  );
  state = paperSearch.paperSearchReducer(
    state,
    paperSearch.paperSearchSucceeded("first", [{ paper_id: "paper-3" }], true, null),
  );
  assert.deepEqual(state.collections.map["PAPERS_SEARCH:first"].results, [
    { paper_id: "paper-3" },
  ]);
  const cleared = paperSearch.paperSearchReducer(state, paperSearch.clearPaperSearch());
  assert.deepEqual(cleared.collections, state.collections);
});