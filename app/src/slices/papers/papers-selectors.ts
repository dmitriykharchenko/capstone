import { store } from "../../store";

export const selectPapers = store.createSelector((state) => state.papers.items);
export const selectPapersLoading = store.createSelector(
  (state) => state.papers.loading,
);
export const selectPapersError = store.createSelector(
  (state) => state.papers.error,
);
export const selectSelectedPaperId = store.createSelector(
  (state) => state.papers.selectedId,
);
export const selectPdfUrl = store.createSelector((state) => state.papers.pdfUrl);
export const selectPdfLoading = store.createSelector(
  (state) => state.papers.pdfLoading,
);
export const selectPaperIndexing = store.createSelector(
  (state) => state.papers.indexing,
);
export const selectPaperIndexCompleted = store.createSelector(
  (state) => state.papers.indexCompleted,
);
export const selectPaperIndexTotal = store.createSelector(
  (state) => state.papers.indexTotal,
);
export const selectPaperIndexCurrentFile = store.createSelector(
  (state) => state.papers.indexCurrentFile,
);
export const selectPaperIndexCurrentStatus = store.createSelector(
  (state) => state.papers.indexCurrentStatus,
);
export const selectPaperIndexCurrentWarning = store.createSelector(
  (state) => state.papers.indexCurrentWarning,
);
export const selectPaperIndexedCount = store.createSelector(
  (state) => state.papers.indexedCount,
);
export const selectPaperIndexEmptyCount = store.createSelector(
  (state) => state.papers.indexEmptyCount,
);
export const selectPaperIndexFailedCount = store.createSelector(
  (state) => state.papers.indexFailedCount,
);
export const selectPaperIndexSkippedCount = store.createSelector(
  (state) => state.papers.indexSkippedCount,
);
export const selectPaperIndexError = store.createSelector(
  (state) => state.papers.indexError,
);
export const selectPaperIndexStatuses = store.createSelector(
  (state) => state.papers.indexStatuses,
);
export const selectPaperIndexWarnings = store.createSelector(
  (state) => state.papers.indexWarnings,
);
export const selectBm25PaperIds = store.createSelector(
  (state) => state.papers.bm25PaperIds,
);
export const selectPapersResetting = store.createSelector(
  (state) => state.papers.resetting,
);
