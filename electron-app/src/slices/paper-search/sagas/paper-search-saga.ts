import { all, call, put, select, takeLatest } from "typed-redux-saga";
import { api } from "../../../lib/api";
import { navigateTo, replaceWith, routeChanged } from "../../router/router-slice";
import {
  clearPaperSearch,
  runPaperSearch,
  paperSearchFailed,
  paperSearchStarted,
  paperSearchSucceeded,
} from "../paper-search-slice";

// Rank indexed papers through the native backend. Disabled capabilities and
// unavailable commands remain visible to the UI rather than using a JS fallback.
export function* paperSearchSaga() {
  yield* all([
    takeLatest(runPaperSearch, function* (action) {
      const query = action.payload[0].trim();
      const state = yield* select((current) => current);
      const activeRoute = state.router.activeRoute;
      if (
        activeRoute.name === "papers" &&
        (activeRoute.query ?? "") === query &&
        state.paperSearch.error === null
      ) {
        return;
      }
      yield* put(navigateTo(query ? { name: "papers", query } : { name: "papers" }));
    }),
    takeLatest(routeChanged, function* (action) {
      const route = action.payload[0];
      if (route.name !== "papers") {
        return;
      }
      const query = route.query?.trim() ?? "";
      if (query !== (route.query ?? "")) {
        yield* put(
          replaceWith(query ? { name: "papers", query } : { name: "papers" }),
        );
        return;
      }
      if (!query) {
        yield* put(clearPaperSearch());
        return;
      }
      yield* put(paperSearchStarted(query));
      try {
        const result = yield* call(api.searchPapers, query, 8);
        yield* put(
          paperSearchSucceeded(
            query,
            result.matches,
            result.enabled,
            result.reason ?? null,
          ),
        );
      } catch (error) {
        yield* put(paperSearchFailed(String(error)));
      }
    }),
  ]);
}
