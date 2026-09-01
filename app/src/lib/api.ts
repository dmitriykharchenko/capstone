// Thin typed wrappers over the Tauri command handlers registered in the Rust
// backend. Every call goes through `invoke`; papers/PDF are served natively,
// while chat/sessions/messages/search/health are proxied to the Python CLI.

import { invoke } from "@tauri-apps/api/core";

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
  message_count: number;
  exchange_count: number;
}

export interface ChatMessage {
  role: string;
  content: string;
}

export interface SearchMatch {
  exchange_id: number;
  session_id: string;
  user_message: string;
  assistant_message: string;
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
}

export const api = {
  listPapers: () => invoke<PaperInfo[]>("list_papers"),
  getPaper: (paperId: string) => invoke<PaperInfo>("get_paper", { paperId }),
  readPdf: (paperId: string) => invoke<PdfData>("read_pdf", { paperId }),
  health: () => invoke<HealthInfo>("health"),
  chat: (sessionId: string, message: string) =>
    invoke<ChatResult>("chat", { sessionId, message }),
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
};

// Turn raw PDF bytes returned by the backend into an object URL a webview can
// render inline (revoke it when the viewer unmounts to avoid leaks).
export function pdfBytesToObjectUrl(pdf: PdfData): string {
  const bytes = new Uint8Array(pdf.bytes);
  const blob = new Blob([bytes], { type: pdf.mime_type });
  return URL.createObjectURL(blob);
}
