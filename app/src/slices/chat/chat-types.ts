import type { ChatMessage, ChatSource } from "../../lib/api";

export type ChatSubqueryStatus = "pending" | "active" | "complete";

export interface ChatSubqueryProgress {
  id: string;
  index: number;
  query: string;
  status: ChatSubqueryStatus;
  answer: string | null;
  sources: ChatSource[];
}

export interface ChatTurnProgress {
  subqueries: ChatSubqueryProgress[];
  finalAnswer: string;
}

export interface ChatState {
  sessionId: string;
  messages: ChatMessage[];
  pending: boolean;
  streaming: boolean;
  activeTurn: ChatTurnProgress | null;
  chatMode: string | null;
  error: string | null;
}

export const initialChatState: ChatState = {
  sessionId: "",
  messages: [],
  pending: false,
  streaming: false,
  activeTurn: null,
  chatMode: null,
  error: null,
};
