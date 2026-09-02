import type { PaperMatch } from "../../lib/api";

export const PAPERS_SEARCH_PREFIX = "PAPERS_SEARCH";

export function paperSearchId(query: string): string {
  return `${PAPERS_SEARCH_PREFIX}:${query.trim()}`;
}

export interface PaperSearchCollection {
  id: string;
  results: PaperMatch[];
}

export interface PaperSearchState {
  query: string;
  collections: Record<string, PaperSearchCollection>;
  searching: boolean;
  enabled: boolean;
  reason: string | null;
  error: string | null;
  hasSearched: boolean;
}

export const initialPaperSearchState: PaperSearchState = {
  query: "",
  collections: {},
  searching: false,
  enabled: true,
  reason: null,
  error: null,
  hasSearched: false,
};
