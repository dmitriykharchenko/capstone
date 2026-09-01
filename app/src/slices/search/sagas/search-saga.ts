import { call, put, takeLatest } from "typed-redux-saga";
import { api } from "../../../lib/api";
import {
  runSearch,
  searchFailed,
  searchStarted,
  searchSucceeded,
} from "../search-slice";

// Rank chat-history exchanges by semantic similarity via the Chroma-backed index
// (Python CLI). When chromadb is unavailable the CLI returns enabled=false, which
// the reducer surfaces so the UI can explain search is disabled.
export function* searchSaga() {
  yield* takeLatest(runSearch, function* (action) {
    const query = action.payload[0];
    if (!query.trim()) {
      return;
    }
    yield* put(searchStarted(query));
    try {
      const result = yield* call(api.search, query, 5);
      yield* put(
        searchSucceeded(result.matches, result.enabled, result.reason ?? null),
      );
    } catch (error) {
      yield* put(searchFailed(String(error)));
    }
  });
}
