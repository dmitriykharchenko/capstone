import {
  getItem,
  getItems,
} from "@augmentcode/themis/utils/collections/collection-utils";
import { store } from "../../store";
import { paperSearchId } from "./paper-search-types";

export const selectPaperSearchMatches = store.createSelector(
  (state) => {
    const route = state.router.activeRoute;
    if (route.name !== "papers") {
      return [];
    }
    return (
      getItem(
        state.paperSearch.searchResultsCollection,
        paperSearchId(route.query ?? ""),
      )?.results ?? []
    );
  },
);
export const selectPaperSearchCollections = store.createSelector(
  (state) => getItems(state.paperSearch.searchResultsCollection),
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
