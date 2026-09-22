import { createAction } from "@augmentcode/themis/utils/store/create-action";
import { createReducer } from "@augmentcode/themis/utils/store/create-reducer";
import type { StoreAction, StoreReducerFunction } from "@augmentcode/themis/types";
import type { IndexReadiness, PaperInfo, PaperStatusInfo } from "../../lib/api";
import {
  beginPaperIndex,
  beginReset,
  completePaperIndex,
  completeReset,
  hydratePaperStatuses,
  initialPapersState,
  markPaperIndexing,
  recordPaperIndexProgress,
  rejectPaperIndex,
  rejectReset,
  setBm25PaperIds,
  type PapersState,
} from "./papers-types";

export const loadPapers = createAction("papers/load");
export const papersLoading = createAction("papers/loading");
export const setPapers = createAction<[papers: PaperInfo[]]>("papers/set");
export const setPaperStatuses = createAction<[statuses: PaperStatusInfo[]]>(
  "papers/setStatuses",
);
export const papersFailed = createAction<[error: string]>("papers/failed");
export const setPaperCapabilities = createAction<[commands: string[]]>("papers/capabilities");
export const bm25Unavailable = createAction("papers/bm25Unavailable");
export const setIndexReadiness = createAction<[readiness: IndexReadiness]>("papers/indexReadiness");

// Selection only. The mounted reader owns bytes, loading, rendering and passwords.
export const openPaper = createAction<[paperId: string]>("papers/open");
export const closePaper = createAction("papers/close");

export const runPaperIndex = createAction("papers/runIndex");
export const paperIndexStarted = createAction<[total: number]>(
  "papers/indexStarted",
);
export const paperIndexPaperStarted = createAction<
  [paperId: string, filename: string]
>("papers/indexPaperStarted");
export const paperIndexProgress = createAction<
  [
    completed: number,
    total: number,
    paperId: string,
    currentFile: string,
    status: string,
  ]
>("papers/indexProgress");
export const paperIndexSucceeded = createAction<
  [
    completed: number,
    total: number,
    indexed: number,
    empty: number,
    failed: number,
    skipped: number,
  ]
>("papers/indexSucceeded");
export const paperIndexFailed = createAction<[error: string]>(
  "papers/indexFailed",
);

// Set the BM25-covered paper ids used to render the per-paper BM25 dot.
export const setBm25Statuses = createAction<[paperIds: string[]]>(
  "papers/setBm25Statuses",
);

// Reset native derived state, including graph; preserve PDFs and history.
export const resetIndices = createAction("papers/resetIndices");
export const resetIndicesSucceeded = createAction(
  "papers/resetIndicesSucceeded",
);
export const resetIndicesFailed = createAction<[error: string]>(
  "papers/resetIndicesFailed",
);

export const papersReducer: StoreReducerFunction<PapersState> =
  createReducer<PapersState>(initialPapersState)
    .with(papersLoading, (state: PapersState) => ({
      ...state,
      loading: true,
      error: null,
      availableCommands: [],
      statusesKnown: false,
      bm25Known: false,
      indexReadiness: "unavailable",
    }))
    .with(setPaperCapabilities, (state: PapersState, action: StoreAction<[string[]]>) => ({
      ...state, availableCommands: action.payload[0],
    }))
    .with(bm25Unavailable, (state: PapersState) => ({ ...state, bm25Known: false }))
    .with(setIndexReadiness, (state: PapersState, action: StoreAction<[IndexReadiness]>) => ({
      ...state, indexReadiness: action.payload[0], statusesKnown: false, bm25Known: false,
    }))
    .with(setPapers, (state: PapersState, action: StoreAction<[PaperInfo[]]>) => ({
      ...state,
      loading: false,
      items: action.payload[0],
    }))
    .with(
      setPaperStatuses,
      (state: PapersState, action: StoreAction<[PaperStatusInfo[]]>) =>
        hydratePaperStatuses(state, action.payload[0]),
    )
    .with(papersFailed, (state: PapersState, action: StoreAction<[string]>) => ({
      ...state,
      loading: false,
      error: action.payload[0],
    }))
    .with(openPaper, (state: PapersState, action: StoreAction<[string]>) => ({
      ...state,
      selectedId: action.payload[0],
    }))
    .with(closePaper, (state: PapersState) => ({
      ...state,
      selectedId: null,
    }))
    .with(
      paperIndexStarted,
      (state: PapersState, action: StoreAction<[number]>) =>
        beginPaperIndex(state, action.payload[0]),
    )
    .with(
      paperIndexPaperStarted,
      (state: PapersState, action: StoreAction<[string, string]>) =>
        markPaperIndexing(state, ...action.payload),
    )
    .with(
      paperIndexProgress,
      (
        state: PapersState,
        action: StoreAction<[number, number, string, string, string]>,
      ) => recordPaperIndexProgress(state, ...action.payload),
    )
    .with(
      paperIndexSucceeded,
      (
        state: PapersState,
        action: StoreAction<[number, number, number, number, number, number]>,
      ) => completePaperIndex(state, ...action.payload),
    )
    .with(
      paperIndexFailed,
      (state: PapersState, action: StoreAction<[string]>) =>
        rejectPaperIndex(state, action.payload[0]),
    )
    .with(
      setBm25Statuses,
      (state: PapersState, action: StoreAction<[string[]]>) =>
        setBm25PaperIds(state, action.payload[0]),
    )
    .with(resetIndices, (state: PapersState) => beginReset(state))
    .with(resetIndicesSucceeded, (state: PapersState) => completeReset(state))
    .with(
      resetIndicesFailed,
      (state: PapersState, action: StoreAction<[string]>) =>
        rejectReset(state, action.payload[0]),
    );
