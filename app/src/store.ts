// Application store: registers every slice reducer and starts each slice saga.
// Selectors read from the typed state map; sagas own all Tauri `invoke` calls.

import { Store } from "@augmentcode/themis/svelte-store";
import { chatReducer } from "./slices/chat/chat-slice";
import { chatSaga } from "./slices/chat/sagas/chat-saga";
import { sessionsReducer } from "./slices/sessions/sessions-slice";
import { sessionsSaga } from "./slices/sessions/sagas/sessions-saga";
import { papersReducer } from "./slices/papers/papers-slice";
import { papersSaga } from "./slices/papers/sagas/papers-saga";
import { searchReducer } from "./slices/search/search-slice";
import { searchSaga } from "./slices/search/sagas/search-saga";
import { paperSearchReducer } from "./slices/paper-search/paper-search-slice";
import { paperSearchSaga } from "./slices/paper-search/sagas/paper-search-saga";

const reducers = {
  chat: chatReducer,
  sessions: sessionsReducer,
  papers: papersReducer,
  search: searchReducer,
  paperSearch: paperSearchReducer,
};

// Single application store instance. Selectors are created against it at module
// scope; `App.svelte` owns its lifecycle (`init()` / `dispose()` + `runSaga`).
export const store = new Store(reducers, undefined, {
  throttledSelectorFrequency: 64,
});

export type AppStore = typeof store;

// Every app saga; started explicitly after `store.init()` (the package saga
// manager does not auto-start app sagas).
export const appSagas = [
  chatSaga,
  sessionsSaga,
  papersSaga,
  searchSaga,
  paperSearchSaga,
];
