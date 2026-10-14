import type { LibrarySelection } from "../../lib/api";

export type LibraryStatus = "idle" | "loading" | "ready" | "error";

export interface LibraryState {
  current: LibrarySelection | null;
  status: LibraryStatus;
  choosing: boolean;
  error: string | null;
}

export const initialLibraryState: LibraryState = {
  current: null,
  status: "idle",
  choosing: false,
  error: null,
};

export interface LibraryWorkState {
  chat: { pending: boolean; streaming: boolean };
  sessions: { loading: boolean };
  papers: {
    loading: boolean;
    pdfLoading: boolean;
    indexing: boolean;
    resetting: boolean;
  };
  search: { searching: boolean };
  paperSearch: { searching: boolean };
  evaluation: { running: boolean };
}

export function hasActiveLibraryWork(state: LibraryWorkState): boolean {
  return (
    state.chat.pending ||
    state.chat.streaming ||
    state.sessions.loading ||
    state.papers.loading ||
    state.papers.pdfLoading ||
    state.papers.indexing ||
    state.papers.resetting ||
    state.search.searching ||
    state.paperSearch.searching ||
    state.evaluation.running
  );
}