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
export const selectPaperIndexing = store.createSelector(
  (state) => state.paperSearch.indexing,
);
export const selectPaperIndexedCount = store.createSelector(
  (state) => state.paperSearch.indexedCount,
);
export const selectPaperIndexError = store.createSelector(
  (state) => state.paperSearch.indexError,
);
export const selectPaperIndexCompleted = store.createSelector(
  (state) => state.paperSearch.indexCompleted,
);
export const selectPaperIndexTotal = store.createSelector(
  (state) => state.paperSearch.indexTotal,
);
export const selectPaperIndexCurrentFile = store.createSelector(
  (state) => state.paperSearch.indexCurrentFile,
);
export const selectPaperIndexFailedCount = store.createSelector(
  (state) => state.paperSearch.indexFailedCount,
);
export const selectPaperIndexEmptyCount = store.createSelector(
  (state) => state.paperSearch.indexEmptyCount,
);
