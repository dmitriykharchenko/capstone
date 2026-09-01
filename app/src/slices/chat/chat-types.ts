import type { ChatMessage } from "../../lib/api";

export interface ChatState {
  sessionId: string;
  messages: ChatMessage[];
  pending: boolean;
  chatMode: string | null;
  error: string | null;
}

export const initialChatState: ChatState = {
  sessionId: "default",
  messages: [],
  pending: false,
  chatMode: null,
  error: null,
};
