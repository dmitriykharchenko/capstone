import { store } from "../../store";
import { chatHistorySearchId } from "./search-types";

export const selectSearchQuery = store.createSelector(
  (state) => state.search.query,
);
export const selectSearchMatches = store.createSelector(
  (state) => {
    const route = state.router.activeRoute;
    if (route.name !== "history") {
      return [];
    }
    return state.search.collections[chatHistorySearchId(route.query ?? "")]?.results ?? [];
  },
);
export const selectSearchCollections = store.createSelector(
  (state) => state.search.collections,
);
export const selectSearching = store.createSelector(
  (state) => state.search.searching,
);
export const selectSearchEnabled = store.createSelector(
  (state) => state.search.enabled,
);
export const selectSearchReason = store.createSelector(
  (state) => state.search.reason,
);
export const selectSearchError = store.createSelector(
  (state) => state.search.error,
);
export const selectHasSearched = store.createSelector(
  (state) => state.search.hasSearched,
);
