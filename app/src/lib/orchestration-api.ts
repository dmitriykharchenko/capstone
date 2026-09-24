import { Channel, invoke } from "@tauri-apps/api/core";
import type { ChatSource, ChatStreamEvent } from "./api";

export type WorkStatus = "queued" | "running" | "waiting" | "completed" | "failed" | "cancelled";
export type RunStatus = "running" | "interrupted" | "completed" | "failed" | "discarded";
export type AgentKind = "intake" | "question" | "finalizer";

export interface OrchestrationQuestion {
  question_id: string;
  text: string;
  parent_id: string | null;
  depth: number;
  status: WorkStatus;
  answer: string;
  evidence_ids: string[];
}

export interface OrchestrationAgent {
  agent_id: string;
  kind: AgentKind;
  question_id: string | null;
  parent_id: string | null;
  depth: number;
  status: WorkStatus;
  retry_count: number;
  attempt: number;
}

export interface OrchestrationTool {
  tool_call_id: string;
  agent_id: string;
  attempt: number;
  tool_name: "retrieve_context" | "add_question" | "answer_question";
  question_id: string | null;
  evidence_ids: string[];
}

export interface OrchestrationEvidence {
  evidence_id: string;
  question_id: string | null;
  source: ChatSource;
}

export interface RunSnapshot {
  checkpoint_sequence: number;
  status: RunStatus;
  exchange_id: number | null;
  questions: OrchestrationQuestion[];
  agents: OrchestrationAgent[];
  tools: OrchestrationTool[];
  evidence: OrchestrationEvidence[];
}

export interface TurnIdentity {
  session_id: string;
  run_id: string;
  turn_id: string;
}

interface Envelope extends TurnIdentity {
  version: 1;
  sequence: number;
}

export type SnapshotEvent = Envelope & { event: "snapshot"; snapshot: RunSnapshot };
export type OrchestrationEvent = Envelope & (
  | { event: "snapshot"; snapshot: RunSnapshot }
  | { event: "agent_started" | "agent_completed" | "agent_retried"; agent: OrchestrationAgent }
  | { event: "tool_started" | "tool_completed"; tool: OrchestrationTool }
  | { event: "question_added" | "question_status" | "question_answered" | "question_failed"; question: OrchestrationQuestion }
  | { event: "evidence"; evidence: OrchestrationEvidence[] }
  | { event: "warning"; code: "duplicate_question" | "question_limit" | "empty_evidence" | "retrieval_failed" | "question_failed" | "handler_retry" | "derived_index" }
  | { event: "final_delta"; agent_id: string; kind: "finalizer"; delta: string }
  | { event: "done"; exchange_id: number; reply: string; chat_mode: "real" | "mock"; indexed: boolean; sources: ChatSource[] }
  | { event: "error"; code: "interrupted" | "failed" | "discarded" | "invalid_stream"; message: string; resumable: boolean }
);

function argumentsFor(identity: TurnIdentity) {
  return { sessionId: identity.session_id, runId: identity.run_id, turnId: identity.turn_id };
}

function stream(command: string, identity: TurnIdentity, onEvent: (event: OrchestrationEvent) => void, message?: string) {
  const channel = new Channel<OrchestrationEvent>();
  channel.onmessage = onEvent;
  return invoke<void>(command, { ...argumentsFor(identity), ...(message === undefined ? {} : { message }), onEvent: channel });
}

// Opt-in transport for the later trace/recovery UI. Existing chatStream is unchanged.
export const orchestrationApi = {
  startOrchestration: (identity: TurnIdentity, message: string, onEvent: (event: OrchestrationEvent) => void) => stream("orchestration_start", identity, onEvent, message),
  resumeOrchestration: (identity: TurnIdentity, onEvent: (event: OrchestrationEvent) => void) => stream("orchestration_resume", identity, onEvent),
  pendingOrchestration: (sessionId: string) => invoke<{ pending: SnapshotEvent | null }>("orchestration_pending", { sessionId }),
  discardOrchestration: (identity: TurnIdentity) => invoke<{ snapshot: SnapshotEvent }>("orchestration_discard", argumentsFor(identity)),
  orchestrationTrace: (sessionId: string, exchangeId: number) => invoke<{ snapshot: SnapshotEvent }>("orchestration_trace", { sessionId, exchangeId }),
};

// Never map specialist/tool/question text into the visible assistant transcript.
// Snapshot replaces any uncommitted partial text; that reset belongs to the UI.
export function legacyOrchestrationEvent(event: OrchestrationEvent): ChatStreamEvent | null {
  switch (event.event) {
    case "final_delta": return { event: "delta", delta: event.delta };
    case "done": return { event: "done", reply: event.reply, chatMode: event.chat_mode, indexed: event.indexed, sources: event.sources };
    case "error": return { event: "error", message: event.message };
    default: return null;
  }
}