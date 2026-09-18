import { all, call, put, select, takeLatest } from "typed-redux-saga";
import { api } from "../../../lib/api";
import { navigateTo, replaceWith, routeChanged } from "../../router/router-slice";
import {
  clearSearch,
  runSearch,
  searchFailed,
  searchStarted,
  searchSucceeded,
} from "../search-slice";

// Rank history exchanges through the native backend. Disabled capabilities and
// unavailable commands remain visible to the UI rather than using a JS fallback.
export function* searchSaga() {
  yield* all([
    takeLatest(runSearch, function* (action) {
      const query = action.payload[0].trim();
      const state = yield* select((current) => current);
      const activeRoute = state.router.activeRoute;
      if (
        activeRoute.name === "history" &&
        (activeRoute.query ?? "") === query &&
        state.search.error === null
      ) {
        return;
      }
      yield* put(navigateTo(query ? { name: "history", query } : { name: "history" }));
    }),
    takeLatest(routeChanged, function* (action) {
      const route = action.payload[0];
      if (route.name !== "history") {
        return;
      }
      const query = route.query?.trim() ?? "";
      if (query !== (route.query ?? "")) {
        yield* put(
          replaceWith(query ? { name: "history", query } : { name: "history" }),
        );
        return;
      }
      if (!query) {
        yield* put(clearSearch());
        return;
      }
      yield* put(searchStarted(query));
      try {
        const result = yield* call(api.search, query, 5);
        yield* put(
          searchSucceeded(query, result.matches, result.enabled, result.reason ?? null),
        );
      } catch (error) {
        yield* put(searchFailed(String(error)));
      }
    }),
  ]);
}
