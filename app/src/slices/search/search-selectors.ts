import { store } from "../../store";

export const selectSearchQuery = store.createSelector(
  (state) => state.search.query,
);
export const selectSearchMatches = store.createSelector(
  (state) => state.search.matches,
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
