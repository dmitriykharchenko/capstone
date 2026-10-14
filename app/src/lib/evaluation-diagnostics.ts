import type { EvaluationCaseError, EvaluationCaseResult, EvaluationFailureCategory } from "./evaluation-types";
import { sanitizeEvaluationMetrics, sanitizeEvaluationPerformance } from "./evaluation-metrics";

export const failureReasons: Record<EvaluationFailureCategory, readonly [string, boolean]> = {
  payment_required: ["Provider payment is required. Check billing before retrying.", false],
  authentication: ["Provider authentication failed. Check credentials before retrying.", false],
  authorization: ["Provider access was denied. Check permissions before retrying.", false],
  rate_limit: ["Provider rate limit reached. Try again later.", true],
  timeout: ["The operation timed out. Try again later.", true],
  connection: ["The provider could not be reached. Check connectivity and retry.", true],
  http_server_error: ["The provider reported a server error. Try again later.", true],
  http_client_error: ["The provider rejected the request. Check configuration before retrying.", false],
  incomplete_response: ["The provider response was incomplete. Check output limits and retry.", true],
  refusal: ["The provider declined to answer this request.", false],
  invalid_planner_output: ["The planner returned an invalid plan. Try again.", true],
  retrieval_failure: ["Retrieval failed. Check the paper index and retry.", false],
  invalid_judge_output: ["The judge returned invalid scores. Try again.", true],
  cancellation: ["Evaluation was cancelled.", false],
  configuration: ["Evaluation configuration is invalid. Check configuration before retrying.", false],
  unknown: ["Evaluation failed for an unknown reason. Check configuration and retry.", false],
};
const stages = new Set(["planner", "retrieval", "subanswer", "coverage", "followup_retrieval", "followup_subanswer", "composer", "judge", "model", "evaluation"]);
const hopStages = new Set(["retrieval", "subanswer", "followup_retrieval", "followup_subanswer"]);
const fields = new Set(["stage", "category", "http_status", "retryable", "retry_round", "stage_attempt", "hop_index", "message"]);
const statusCategories: Record<number, EvaluationFailureCategory> = { 401: "authentication", 402: "payment_required", 403: "authorization", 408: "timeout", 429: "rate_limit" };
export const evaluationTransportError = "Evaluation transport failed. Check the backend logs and retry.";

function unknownFailure(): EvaluationCaseError {
  return { stage: "evaluation", category: "unknown", http_status: null, retryable: false, retry_round: 0, stage_attempt: 1, hop_index: null, message: failureReasons.unknown[0] };
}

export function sanitizeFailure(value: unknown): EvaluationCaseError | null {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  // Tauri omits absent optional status/hop fields for legacy wire compatibility.
  const v = { http_status: null, hop_index: null, ...value } as EvaluationCaseError;
  if (Object.keys(v).length !== fields.size || Object.keys(v).some((key) => !fields.has(key))) return null;
  if (typeof v.stage !== "string" || typeof v.category !== "string" || !stages.has(v.stage) || !Object.hasOwn(failureReasons, v.category)) return null;
  const [message, retryable] = failureReasons[v.category];
  if (v.http_status !== null) {
    if (!Number.isInteger(v.http_status) || v.http_status < 400 || v.http_status > 599) return null;
    if ((statusCategories[v.http_status] ?? (v.http_status >= 500 ? "http_server_error" : "http_client_error")) !== v.category) return null;
  } else if (["payment_required", "authentication", "authorization", "rate_limit", "http_server_error", "http_client_error"].includes(v.category)) return null;
  if (v.retryable !== (retryable || v.http_status === 409) || typeof v.message !== "string") return null;
  if (!Number.isInteger(v.retry_round) || v.retry_round < 0 || v.retry_round > 2 || !Number.isInteger(v.stage_attempt) || v.stage_attempt < 1 || v.stage_attempt > v.retry_round + 1) return null;
  if (hopStages.has(v.stage) ? !Number.isInteger(v.hop_index) || v.hop_index! < 1 || v.hop_index! > 7 : v.hop_index !== null) return null;
  return { stage: v.stage, category: v.category, http_status: v.http_status, retryable: v.retryable, retry_round: v.retry_round, stage_attempt: v.stage_attempt, hop_index: v.hop_index, message };
}

export function sanitizeEvaluationResult(result: EvaluationCaseResult): EvaluationCaseResult {
  const history: EvaluationCaseError[] = [];
  const counts = new Map<string, number>();
  const raw = result.attempt_history ?? [];
  let valid = Array.isArray(raw) && raw.length <= (result.error ? 3 : 2);
  if (valid) {
    for (const [index, item] of raw.entries()) {
      const safe = sanitizeFailure(item);
      if (!safe || safe.retry_round !== index) { valid = false; break; }
      const key = `${safe.stage}:${safe.hop_index}`;
      const count = (counts.get(key) ?? 0) + 1;
      counts.set(key, count);
      if (safe.stage_attempt !== count) { valid = false; break; }
      history.push(safe);
    }
  }
  const error = result.error == null ? null : sanitizeFailure(result.error);
  if (result.error != null && (!error || JSON.stringify(error) !== JSON.stringify(history.at(-1)))) valid = false;
  return {
    case_index: result.case_index, input: result.input, retrieval_strategy: result.retrieval_strategy,
    retrieved_chunks: result.retrieved_chunks, generated_answer: result.generated_answer, judge_scores: result.judge_scores,
    error: result.error == null ? null : valid ? error : unknownFailure(),
    attempt_history: valid ? history : [],
    metrics: sanitizeEvaluationMetrics(result.metrics),
    performance: sanitizeEvaluationPerformance(result.performance),
  };
}

export function failureSummary(error: EvaluationCaseError): string {
  return `${error.stage}${error.hop_index === null ? "" : ` hop ${error.hop_index}`} · ${error.category}${error.http_status === null ? "" : ` · HTTP ${error.http_status}`} · ${error.retryable ? "Retryable" : "Not retryable"}`;
}