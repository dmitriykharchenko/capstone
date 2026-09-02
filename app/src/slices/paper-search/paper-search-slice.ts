import { createAction } from "@augmentcode/themis/utils/store/create-action";
import { createReducer } from "@augmentcode/themis/utils/store/create-reducer";
import type { StoreAction, StoreReducerFunction } from "@augmentcode/themis/types";
import type { PaperMatch } from "../../lib/api";
import {
  paperSearchId,
  initialPaperSearchState,
  type PaperSearchState,
} from "./paper-search-types";

export const runPaperSearch = createAction<[query: string]>("paperSearch/run");
export const paperSearchStarted = createAction<[query: string]>(
  "paperSearch/started",
);
export const paperSearchSucceeded = createAction<
  [query: string, results: PaperMatch[], enabled: boolean, reason: string | null]
>("paperSearch/succeeded");
export const paperSearchFailed = createAction<[error: string]>(
  "paperSearch/failed",
);
export const clearPaperSearch = createAction("paperSearch/clear");

export const paperSearchReducer: StoreReducerFunction<PaperSearchState> =
  createReducer<PaperSearchState>(initialPaperSearchState)
    .with(
      paperSearchStarted,
      (state: PaperSearchState, action: StoreAction<[string]>) => ({
        ...state,
        query: action.payload[0],
        searching: true,
        error: null,
      }),
    )
    .with(
      paperSearchSucceeded,
      (
        state: PaperSearchState,
        action: StoreAction<[string, PaperMatch[], boolean, string | null]>,
      ) => {
        const id = paperSearchId(action.payload[0]);
        return {
          ...state,
          searching: false,
          hasSearched: true,
          collections: {
            ...state.collections,
            [id]: { id, results: action.payload[1] },
          },
          enabled: action.payload[2],
          reason: action.payload[3],
        };
      },
    )
    .with(
      paperSearchFailed,
      (state: PaperSearchState, action: StoreAction<[string]>) => ({
        ...state,
        searching: false,
        hasSearched: true,
        error: action.payload[0],
      }),
    )
    .with(clearPaperSearch, (state: PaperSearchState) => ({
      ...initialPaperSearchState,
      collections: state.collections,
    }));
