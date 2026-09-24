// Thin typed wrappers over the Tauri command handlers registered in the Rust
// backend. Every call goes through `invoke`; papers/PDF are served natively,
// while chat/sessions/messages/search/health are proxied to the Python CLI.

import { invoke, Channel } from "@tauri-apps/api/core";
import { orchestrationApi } from "./orchestration-api";
export type { OrchestrationEvent, SnapshotEvent, TurnIdentity } from "./orchestration-api";
import {
  parseGraphNeighborhoodPayload,
  type GraphNeighborhoodResponse,
} from "./graph-types";

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
export type {
  GraphEdge,
  GraphEdgeKind,
  GraphNeighborhoodResponse,
  GraphNode,
  GraphNodeKind,
} from "./graph-types";

export interface PaperInfo {
  id: string;
  filename: string;
  title: string;
  size_bytes: number;
}

export interface PdfData {
  id: string;
  filename: string;
  mime_type: string;
  bytes: number[];
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

export interface HealthInfo {
  ok: boolean;
  chat_mode: string;
  search_enabled: boolean;
  database_path: string;
  papers_directory: string;
  chroma_directory: string;
}

export interface ChatResult {
  session_id: string;
  reply: string;
  chat_mode: string;
  indexed: boolean;
  sources: ChatSource[];
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
    }
  | { event: "error"; message: string };

export const api = {
  ...orchestrationApi,
  listPapers: () => invoke<PaperInfo[]>("list_papers"),
  getPaper: (paperId: string) => invoke<PaperInfo>("get_paper", { paperId }),
  readPdf: (paperId: string) => invoke<PdfData>("read_pdf", { paperId }),
  health: () => invoke<HealthInfo>("health"),
  chat: (sessionId: string, message: string) =>
    invoke<ChatResult>("chat", { sessionId, message }),
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
  // Fetch one bounded, allow-listed graph neighborhood. The parser validates
  // the unknown sidecar payload before it can enter application state.
  graphNeighborhood: async (
    nodeId: string,
    maxDepth = 1,
  ): Promise<GraphNeighborhoodResponse> =>
    parseGraphNeighborhoodPayload(
      await invoke<unknown>("graph_neighborhood", { nodeId, maxDepth }),
    ),
  // Index (or re-index) every PDF in the papers directory into Chroma.
  indexPapers: () =>
    invoke<{ enabled: boolean; indexed: number; reason?: string }>(
      "index_papers",
    ),
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
