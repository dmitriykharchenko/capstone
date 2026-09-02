import { createAction } from "@augmentcode/themis/utils/store/create-action";
import { createReducer } from "@augmentcode/themis/utils/store/create-reducer";
import { upsertItem } from "@augmentcode/themis/utils/collections/collection-utils";
import type { StoreAction, StoreReducerFunction } from "@augmentcode/themis/types";
import type { SearchMatch } from "../../lib/api";
import {
  chatHistorySearchId,
  initialSearchState,
  type SearchState,
} from "./search-types";

export const runSearch = createAction<[query: string]>("search/run");
export const searchStarted = createAction<[query: string]>("search/started");
export const searchSucceeded = createAction<
  [query: string, results: SearchMatch[], enabled: boolean, reason: string | null]
>("search/succeeded");
export const searchFailed = createAction<[error: string]>("search/failed");
export const clearSearch = createAction("search/clear");

export const searchReducer: StoreReducerFunction<SearchState> =
  createReducer<SearchState>(initialSearchState)
    .with(searchStarted, (state: SearchState, action: StoreAction<[string]>) => ({
      ...state,
      query: action.payload[0],
      searching: true,
      error: null,
    }))
    .with(
      searchSucceeded,
      (
        state: SearchState,
        action: StoreAction<[string, SearchMatch[], boolean, string | null]>,
      ) => {
        const id = chatHistorySearchId(action.payload[0]);
        const collections = upsertItem(state.collections, {
          id,
          results: action.payload[1],
        });
        return {
          ...state,
          searching: false,
          hasSearched: true,
          collections,
          enabled: action.payload[2],
          reason: action.payload[3],
        };
      },
    )
    .with(searchFailed, (state: SearchState, action: StoreAction<[string]>) => ({
      ...state,
      searching: false,
      hasSearched: true,
      error: action.payload[0],
    }))
    .with(clearSearch, (state: SearchState) => ({
      ...initialSearchState,
      collections: state.collections,
    }));
