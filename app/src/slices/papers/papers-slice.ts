import { createAction } from "@augmentcode/themis/utils/store/create-action";
import { createReducer } from "@augmentcode/themis/utils/store/create-reducer";
import type { StoreAction, StoreReducerFunction } from "@augmentcode/themis/types";
import type { PaperInfo } from "../../lib/api";
import { initialPapersState, type PapersState } from "./papers-types";

export const loadPapers = createAction("papers/load");
export const papersLoading = createAction("papers/loading");
export const setPapers = createAction<[papers: PaperInfo[]]>("papers/set");
export const papersFailed = createAction<[error: string]>("papers/failed");

// Open a paper: the saga fetches PDF bytes and turns them into an object URL.
export const openPaper = createAction<[paperId: string]>("papers/open");
export const pdfLoading = createAction<[paperId: string]>("papers/pdfLoading");
export const setPdfUrl = createAction<[paperId: string, url: string]>(
  "papers/setPdfUrl",
);
export const pdfFailed = createAction<[error: string]>("papers/pdfFailed");
export const closePaper = createAction("papers/close");

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
    }));
