import { call, put, takeLatest } from "typed-redux-saga";
import { api } from "../../../lib/api";
import {
  runPaperSearch,
  paperSearchFailed,
  paperSearchStarted,
  paperSearchSucceeded,
} from "../paper-search-slice";

// Rank indexed papers by semantic similarity via the Chroma-backed `documents`
// collection (Python CLI). When chromadb is unavailable the CLI returns
// enabled=false, which the reducer surfaces so the UI can explain search is off.
export function* paperSearchSaga() {
  yield* takeLatest(runPaperSearch, function* (action) {
    const query = action.payload[0];
    if (!query.trim()) {
      return;
    }
    yield* put(paperSearchStarted(query));
    try {
      const result = yield* call(api.searchPapers, query, 8);
      yield* put(
        paperSearchSucceeded(
          result.matches,
          result.enabled,
          result.reason ?? null,
        ),
      );
    } catch (error) {
      yield* put(paperSearchFailed(String(error)));
    }
  });
}
