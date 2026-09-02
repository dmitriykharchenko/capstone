import { store } from "../../store";

export const selectPaperSearchQuery = store.createSelector(
  (state) => state.paperSearch.query,
);
export const selectPaperSearchMatches = store.createSelector(
  (state) => state.paperSearch.matches,
);
export const selectPaperSearching = store.createSelector(
  (state) => state.paperSearch.searching,
);
export const selectPaperSearchEnabled = store.createSelector(
  (state) => state.paperSearch.enabled,
);
export const selectPaperSearchReason = store.createSelector(
  (state) => state.paperSearch.reason,
);
export const selectPaperSearchError = store.createSelector(
  (state) => state.paperSearch.error,
);
export const selectPaperHasSearched = store.createSelector(
  (state) => state.paperSearch.hasSearched,
);
