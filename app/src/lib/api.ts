// Thin typed wrappers over the Tauri command handlers registered in the Rust
// backend. Every call goes through `invoke`; papers/PDF are served natively,
// while chat/sessions/messages/search/health are proxied to the Python CLI.

import { invoke, Channel } from "@tauri-apps/api/core";
import { orchestrationApi } from "./orchestration-api";
export type { OrchestrationEvent, SnapshotEvent, TurnIdentity } from "./orchestration-api";
import type {
  EvaluationStreamEvent,
  EvaluationTestCase,
} from "./evaluation-types";
import { EVALUATION_WIRE_RETRIEVAL_TYPE } from "./evaluation-types";

export {
  EVALUATION_RETRIEVAL_TYPE,
  EVALUATION_WIRE_RETRIEVAL_TYPE,
} from "./evaluation-types";

export type {
  EvaluationAggregateScores,
  EvaluationCaseError,
  EvaluationCaseResult,
  EvaluationJudgeScores,
  EvaluationRetrievalType,
  EvaluationRetrievedChunk,
  EvaluationStreamEvent,
  EvaluationTestCase,
} from "./evaluation-types";

export interface PaperInfo {
  id: string;
  filename: string;
  title: string | null;
  authors: string[];
  topics: string[];
  metadata_status: "available" | "missing" | "unavailable";
  size_bytes: number;
}

export interface PdfData {
  id: string;
  filename: string;
  mime_type: string;
  bytes: number[];
}

// One persisted per-paper indexing outcome, returned by `paper_statuses` so the
// UI can restore indicators after a restart.
export interface PaperStatusInfo {
  paper_id: string;
  filename: string;
  status: string;
  chunk_count: number;
  error: string | null;
  updated_at: string;
}

// Coverage of the persisted BM25 lexical index, returned by `bm25_status`. The
// UI marks a paper's BM25 dot as indexed when its id appears in `paper_ids`.
export interface Bm25StatusInfo {
  built: boolean;
  chunk_count: number;
  paper_ids: string[];
}

// Outcome of `reset_all_indices`: which of the three paper indexes were wiped
// (Chroma vectors, the BM25 file, and the SQLite per-paper status table).
export interface ResetIndicesResult {
  ok: boolean;
  reset: { vector: boolean; bm25: boolean; statuses: boolean };
}

export interface SessionInfo {
  session_id: string;
  title: string;
  message_count: number;
  exchange_count: number;
}

export interface ChatMessage {
  role: string;
  content: string;
  exchange_id?: number;
  sources?: ChatSource[];
}

export interface ChatSource {
  rank: number;
  paper_id: string;
  chunk_index: number;
  filename: string;
  title: string;
  page: number;
  score: number;
  snippet: string;
}

export interface SearchMatch {
  exchange_id: number;
  session_id: string;
  user_message: string;
  assistant_message: string;
}

export interface PaperMatch {
  paper_id: string;
  title: string;
  filename: string;
  page: number | null;
  snippet: string;
  score: number;
}

export interface ChatSubquery {
  id: string;
  index: number;
  query: string;
}

export type ChatCoverageStatus = "reviewing" | "complete" | "followup";

// Discriminated union mirroring the Rust `ChatStreamEvent` (internally tagged
// on `event`). `delta` remains the final-answer token event for compatibility.
export type ChatStreamEvent =
  | { event: "plan"; subqueries: ChatSubquery[] }
  | {
      event: "subquery";
      id: string;
      index: number;
      total: number;
      query: string;
    }
  | {
      event: "subanswer";
      id: string;
      index: number;
      total: number;
      query: string;
      answer: string;
      sources: ChatSource[];
    }
  | {
      event: "coverage";
      status: ChatCoverageStatus;
      total: number;
      subqueries: ChatSubquery[];
    }
  | { event: "delta"; delta: string }
  | {
      event: "done";
      reply: string;
      chatMode: string;
      indexed: boolean;
      sources: ChatSource[];
    }
  | { event: "error"; message: string };

// Discriminated union mirroring the Rust `index_papers_stream` Channel events
// (internally tagged on `event`): a `start` with the total paper count,
// per-paper `progress`, a terminal `done` summary, or an `error`.
export type IndexStreamEvent =
  | { event: "start"; completed: number; total: number }
  | {
      event: "started";
      completed: number;
      total: number;
      paperId: string;
      filename: string;
    }
  | {
      event: "progress";
      completed: number;
      total: number;
      paperId: string;
      filename: string;
      status: string;
      chunkCount: number;
      error: string | null;
    }
  | {
      event: "done";
      completed: number;
      total: number;
      indexed: number;
      empty: number;
      failed: number;
      skipped: number;
    }
  | { event: "error"; message: string };

export const api = {
  ...orchestrationApi,
  listPapers: () => invoke<PaperInfo[]>("list_papers"),
  readPdf: (paperId: string) => invoke<PdfData>("read_pdf", { paperId }),
  // Stream a chat turn: each token delta and the terminal event are delivered
  // to `onEvent` via a Tauri Channel; the returned promise settles when the
  // backend command finishes (or throws on a bridge/spawn failure).
  chatStream: (
    sessionId: string,
    message: string,
    onEvent: (event: ChatStreamEvent) => void,
  ) => {
    const channel = new Channel<ChatStreamEvent>();
    channel.onmessage = onEvent;
    return invoke<void>("chat_stream", { sessionId, message, onEvent: channel });
  },
  listSessions: () =>
    invoke<{ sessions: SessionInfo[] }>("list_sessions"),
  getMessages: (sessionId: string) =>
    invoke<{ session_id: string; messages: ChatMessage[] }>("get_messages", {
      sessionId,
    }),
  search: (query: string, limit?: number, sessionId?: string) =>
    invoke<{ enabled: boolean; matches: SearchMatch[]; reason?: string }>(
      "search",
      { query, limit, sessionId },
    ),
  // Rank indexed papers by semantic similarity via the Chroma `documents`
  // collection (Python CLI). Returns enabled=false when chromadb is missing.
  searchPapers: (query: string, limit?: number) =>
    invoke<{ enabled: boolean; matches: PaperMatch[]; reason?: string }>(
      "search_papers",
      { query, limit },
    ),
  // Persisted per-paper indexing outcomes (SQLite) used to hydrate indicators
  // on startup so an already-indexed paper stays marked after a restart.
  paperStatuses: () =>
    invoke<{ statuses: PaperStatusInfo[] }>("paper_statuses"),
  // Coverage of the persisted BM25 index, used to render the per-paper BM25 dot
  // alongside the vector-index dot.
  bm25Status: () => invoke<Bm25StatusInfo>("bm25_status"),
  // Wipe every paper index (Chroma vectors, BM25 file, SQLite statuses) so the
  // next run re-indexes from scratch.
  resetAllIndices: () => invoke<ResetIndicesResult>("reset_all_indices"),
  // Streaming index: each `start`/`progress`/`done`/`error` event is delivered
  // to `onEvent` via a Tauri Channel so the UI can show determinate progress;
  // the returned promise settles when the backend command finishes.
  indexPapersStream: (onEvent: (event: IndexStreamEvent) => void) => {
    const channel = new Channel<IndexStreamEvent>();
    channel.onmessage = onEvent;
    return invoke<void>("index_papers_stream", { onEvent: channel });
  },
  runEvaluation: (
    testCases: EvaluationTestCase[],
    onEvent: (event: EvaluationStreamEvent) => void,
  ) => {
    const channel = new Channel<EvaluationStreamEvent>();
    channel.onmessage = onEvent;
    return invoke<void>("evaluate_stream", {
      retrievalType: EVALUATION_WIRE_RETRIEVAL_TYPE,
      testCases,
      onEvent: channel,
    });
  },
};

// Turn raw PDF bytes returned by the backend into an object URL a webview can
// render inline (revoke it when the viewer unmounts to avoid leaks).
export function pdfBytesToObjectUrl(pdf: PdfData): string {
  const bytes = new Uint8Array(pdf.bytes);
  const blob = new Blob([bytes], { type: pdf.mime_type });
  return URL.createObjectURL(blob);
}
