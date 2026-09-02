import type { SearchMatch } from "../../lib/api";

export const CHAT_HISTORY_SEARCH_PREFIX = "CHAT_HISTORY_SEARCH";

export function chatHistorySearchId(query: string): string {
  return `${CHAT_HISTORY_SEARCH_PREFIX}:${query.trim()}`;
}

export interface SearchCollection {
  id: string;
  results: SearchMatch[];
}

export interface SearchState {
  query: string;
  collections: Record<string, SearchCollection>;
  searching: boolean;
  enabled: boolean;
  reason: string | null;
  error: string | null;
  hasSearched: boolean;
}

export const initialSearchState: SearchState = {
  query: "",
  collections: {},
  searching: false,
  enabled: true,
  reason: null,
  error: null,
  hasSearched: false,
};
