import { createAction } from "@augmentcode/themis/utils/store/create-action";
import { createReducer } from "@augmentcode/themis/utils/store/create-reducer";
import type { StoreAction, StoreReducerFunction } from "@augmentcode/themis/types";
import type { SearchMatch } from "../../lib/api";
import { initialSearchState, type SearchState } from "./search-types";

export const runSearch = createAction<[query: string]>("search/run");
export const searchStarted = createAction<[query: string]>("search/started");
export const searchSucceeded = createAction<
  [matches: SearchMatch[], enabled: boolean, reason: string | null]
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
        action: StoreAction<[SearchMatch[], boolean, string | null]>,
      ) => ({
        ...state,
        searching: false,
        hasSearched: true,
        matches: action.payload[0],
        enabled: action.payload[1],
        reason: action.payload[2],
      }),
    )
    .with(searchFailed, (state: SearchState, action: StoreAction<[string]>) => ({
      ...state,
      searching: false,
      hasSearched: true,
      error: action.payload[0],
    }))
    .with(clearSearch, () => initialSearchState);
