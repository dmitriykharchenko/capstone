import { createAction } from "@augmentcode/themis/utils/store/create-action";
import { createReducer } from "@augmentcode/themis/utils/store/create-reducer";
import type { StoreAction, StoreReducerFunction } from "@augmentcode/themis/types";
import type { PaperInfo, PaperStatusInfo } from "../../lib/api";
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

// Open a paper: the saga fetches PDF bytes and turns them into an object URL.
export const openPaper = createAction<[paperId: string]>("papers/open");
export const pdfLoading = createAction<[paperId: string]>("papers/pdfLoading");
export const setPdfUrl = createAction<[paperId: string, url: string]>(
  "papers/setPdfUrl",
);
export const pdfFailed = createAction<[error: string]>("papers/pdfFailed");
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
    error: string | null,
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

// Reset every paper index (Chroma vectors, BM25 file, SQLite statuses).
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
    .with(pdfLoading, (state: PapersState, action: StoreAction<[string]>) => ({
      ...state,
      selectedId: action.payload[0],
      pdfLoading: true,
      pdfUrl: null,
      error: null,
    }))
    .with(
      setPdfUrl,
      (state: PapersState, action: StoreAction<[string, string]>) => ({
        ...state,
        selectedId: action.payload[0],
        pdfUrl: action.payload[1],
        pdfLoading: false,
      }),
    )
    .with(pdfFailed, (state: PapersState, action: StoreAction<[string]>) => ({
      ...state,
      pdfLoading: false,
      error: action.payload[0],
    }))
    .with(closePaper, (state: PapersState) => ({
      ...state,
      selectedId: null,
      pdfUrl: null,
      pdfLoading: false,
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
        action: StoreAction<[
          number,
          number,
          string,
          string,
          string,
          string | null,
        ]>,
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
