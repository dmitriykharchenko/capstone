// Typed renderer facade. Only the isolated preload can talk to Electron main.
import { CANCEL } from "redux-saga";
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
  preview_id: string;
  total_bytes: number;
}
export interface PdfRange { preview_id: string; offset: number; bytes: number[] }

export type IndexReadiness = "ready" | "recovering" | "recovery_required" | "rebuild_required" | "unavailable";
// Durable active publication, revalidated against source/store truth. Attempt
// detail is session-only: null after restart means unknown, not success.
export interface PaperStatusInfo {
  paper_id: string;
  filename: string;
  status: string;
  chunk_count: number;
  version_id: string | null;
  attempt_scope: "session";
  attempt: { status: string; error: string | null; observed_at: string } | null;
}

// Coverage of the persisted BM25 lexical index, returned by `bm25_status`. The
// UI marks a paper's BM25 dot as indexed when its id appears in `paper_ids`.
export interface Bm25StatusInfo {
  readiness: IndexReadiness;
  built: boolean;
  chunk_count: number;
  paper_ids: string[];
}

// Only native derived state is cleared, including graph and exchange vectors.
export interface ResetIndicesResult {
  ok: boolean;
  reset: { vector: boolean; bm25: boolean; statuses: boolean; graph: boolean };
  preserved: { pdfs: boolean; history: boolean };
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
  turn_id?: string | null;
  history_index_status?: HistoryIndexStatus;
  history_index_status_observed_at?: HistoryIndexObservation | null;
}

export type HistoryIndexStatus = 'indexed' | 'pending' | 'failed' | 'blocked' | 'rebuild_required';
export interface HistoryIndexObservation { reset_epoch: number; retrieval_revision: number }

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
  protocol_version: 1;
  backend: "rust";
  stage: string;
  commands: { command: string; owner: string; streaming: boolean; available: boolean }[];
}

export interface ChatResult {
  session_id: string;
  turn_id: string;
  exchange_id: number;
  reply: string;
  chat_mode: string;
  indexed: boolean;
  sources: ChatSource[];
  history_index_status: HistoryIndexStatus;
  history_index_status_observed_at: HistoryIndexObservation | null;
  warning: string | null;
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
      sessionId: string;
      turnId: string;
      exchangeId: number;
      historyIndexStatus: HistoryIndexStatus;
      historyIndexStatusObservedAt: HistoryIndexObservation | null;
      warning: string | null;
      reply: string;
      chatMode: string;
      indexed: boolean;
      sources: ChatSource[];
    }
  | { event: "error"; message: string };

// Discriminated union mirroring the Rust `index_papers_stream` public events
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

export type CancellablePromise<T> = Promise<T> & { cancel?: () => void };
export interface RequestHandle<T> { done: Promise<T>; cancel(): void }
export interface RendererApi {
  listPapers(): CancellablePromise<PaperInfo[]>;
  getPaper(paperId: string): CancellablePromise<PaperInfo>;
  readPdf(paperId: string): CancellablePromise<PdfData>;
  readPdfRange(previewId: string, begin: number, end: number): CancellablePromise<PdfRange>;
  closePdf(previewId: string): CancellablePromise<null>;
  health(): CancellablePromise<HealthInfo>;
  chat(sessionId: string, message: string, turnId?: string): CancellablePromise<ChatResult>;
  chatStream(sessionId: string, message: string, onEvent: (event: ChatStreamEvent) => void, turnId?: string): CancellablePromise<void>;
  listSessions(): CancellablePromise<{ sessions: SessionInfo[] }>;
  getMessages(sessionId: string): CancellablePromise<{ session_id: string; messages: ChatMessage[] }>;
  search(query: string, limit?: number, sessionId?: string): CancellablePromise<{ enabled: boolean; matches: SearchMatch[]; reason?: string }>;
  searchPapers(query: string, limit?: number): CancellablePromise<{ enabled: boolean; matches: PaperMatch[]; reason?: string }>;
  graphNeighborhood(nodeId: string, maxDepth?: number): CancellablePromise<GraphNeighborhoodResponse>;
  indexPapers(reindex?: boolean): CancellablePromise<{ enabled: boolean; indexed: number; summary: { completed: number; total: number; indexed: number; empty: number; failed: number; skipped: number } }>;
  paperStatuses(): CancellablePromise<{ readiness: IndexReadiness; statuses: PaperStatusInfo[] }>;
  bm25Status(): CancellablePromise<Bm25StatusInfo>;
  resetAllIndices(): CancellablePromise<ResetIndicesResult>;
  indexPapersStream(onEvent: (event: IndexStreamEvent) => void, reindex?: boolean): CancellablePromise<void>;
  runEvaluation(testCases: EvaluationTestCase[], onEvent: (event: EvaluationStreamEvent) => void): CancellablePromise<void>;
}
export type DesktopBridge = {
  [K in keyof RendererApi]: (...args: Parameters<RendererApi[K]>) =>
    RequestHandle<K extends "graphNeighborhood" ? unknown : Awaited<ReturnType<RendererApi[K]>>>;
} & { toggleMaximize(): Promise<void> };
declare global { interface Window { research: DesktopBridge } }

function request<T>(method: keyof RendererApi, ...args: unknown[]): CancellablePromise<T> {
  try {
    const fn = window.research[method] as (...args: unknown[]) => RequestHandle<T>;
    const handle = fn(...args);
    return Object.assign(Promise.resolve(handle.done), { cancel: handle.cancel, [CANCEL]: handle.cancel });
  } catch {
    return Promise.reject(new Error("The secure desktop bridge is unavailable. Start the Electron app."));
  }
}
export const api: RendererApi = {
  listPapers: () => request("listPapers"),
  getPaper: (id) => request("getPaper", id),
  readPdf: (id) => request("readPdf", id),
  readPdfRange: (id, begin, end) => request("readPdfRange", id, begin, end),
  closePdf: (id) => request("closePdf", id),
  health: () => request("health"),
  chat: (id, message, turnId) => request("chat", id, message, turnId),
  chatStream: (id, message, onEvent, turnId) => request("chatStream", id, message, onEvent, turnId),
  listSessions: () => request("listSessions"),
  getMessages: (id) => request("getMessages", id),
  search: (query, limit, id) => request("search", query, limit, id),
  searchPapers: (query, limit) => request("searchPapers", query, limit),
  graphNeighborhood: (id, depth = 1) => {
    const result = request<unknown>("graphNeighborhood", id, depth);
    return Object.assign(result.then(parseGraphNeighborhoodPayload), { cancel: result.cancel, [CANCEL]: result.cancel });
  },
  indexPapers: (reindex) => request("indexPapers", reindex),
  paperStatuses: () => request("paperStatuses"),
  bm25Status: () => request("bm25Status"),
  resetAllIndices: () => request("resetAllIndices"),
  indexPapersStream: (onEvent, reindex) => request("indexPapersStream", onEvent, reindex),
  runEvaluation: (testCases, onEvent) => request("runEvaluation", testCases, onEvent),
};