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
