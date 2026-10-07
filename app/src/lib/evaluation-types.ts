export const EVALUATION_RETRIEVAL_TYPE = "multihop-hybrid" as const;
export type EvaluationRetrievalType = typeof EVALUATION_RETRIEVAL_TYPE;

export const EVALUATION_WIRE_RETRIEVAL_TYPE = "hybrid" as const;
export type EvaluationWireRetrievalType = typeof EVALUATION_WIRE_RETRIEVAL_TYPE;

export interface EvaluationTestCase {
  question: string;
  grading_notes: string;
  sources: string[];
  performance_budget?: Record<string, number>;
}

export type EvaluationPerformanceCheckName =
  | "rag_latency_ms"
  | "rag_model_calls"
  | "retrievals"
  | "followup_queries"
  | "rag_tokens"
  | "rag_cost_usd"
  | "duplicate_chunks";

export interface EvaluationPerformanceCheck {
  value: number | null;
  limit: number;
  status: "pass" | "fail" | "unknown";
}

export interface EvaluationPerformance {
  version: 1;
  status: "pass" | "fail" | "incomplete";
  checks: Partial<Record<EvaluationPerformanceCheckName, EvaluationPerformanceCheck>>;
}

export interface EvaluationRetrievedChunk {
  rank: number;
  text: string;
  score: number;
  paper_id: string;
  chunk_index: number;
  filename: string;
  title: string;
  page: number;
}

export type JudgeConfidence = "high" | "medium" | "low";

export interface JudgeCriterionBase {
  score: number;
  confidence: JudgeConfidence;
  reasoning: string;
  flags: string[];
}

export interface RankingJudgeCriterion extends JudgeCriterionBase {
  relevant_chunk_ranks: number[];
}

export interface UtilizationJudgeCriterion extends JudgeCriterionBase {
  chunks_used: Array<number | string>;
}

export interface SimilarityJudgeCriterion extends JudgeCriterionBase {
  contradicting_claims: string[];
}

export interface CompletenessJudgeCriterion extends JudgeCriterionBase {
  missing_elements: string[];
}

export interface HallucinationJudgeCriterion extends JudgeCriterionBase {
  unsupported_claims: string[];
}

export interface EvaluationJudgeScores {
  context_precision: JudgeCriterionBase;
  context_recall: JudgeCriterionBase;
  relevant_chunk_ranking: RankingJudgeCriterion;
  context_utilization: UtilizationJudgeCriterion;
  semantic_similarity: SimilarityJudgeCriterion;
  completeness: CompletenessJudgeCriterion;
  hallucination_rate: HallucinationJudgeCriterion;
}

export type EvaluationFailureStage = "planner" | "retrieval" | "subanswer" | "coverage" | "followup_retrieval" | "followup_subanswer" | "composer" | "judge" | "model" | "evaluation";
export type EvaluationFailureCategory = "payment_required" | "authentication" | "authorization" | "rate_limit" | "timeout" | "connection" | "http_server_error" | "http_client_error" | "incomplete_response" | "refusal" | "invalid_planner_output" | "retrieval_failure" | "invalid_judge_output" | "cancellation" | "configuration" | "unknown";

export interface EvaluationCaseError {
  stage: EvaluationFailureStage;
  category: EvaluationFailureCategory;
  http_status: number | null;
  retryable: boolean;
  retry_round: number;
  stage_attempt: number;
  hop_index: number | null;
  message: string;
}

export interface EvaluationTokenMetrics {
  input: number | null;
  output: number | null;
  total: number | null;
  known_input: number;
  known_output: number;
  known_total: number;
  reported_calls: number;
}

export interface EvaluationCostMetrics {
  estimated_usd: number | null;
  known_usd: number;
  priced_calls: number;
  provider_reported_calls: number;
  rate_estimated_calls: number;
}

export interface EvaluationContextMetrics {
  chunks: number;
  unique_chunks: number;
  duplicate_chunks: number;
  characters: number;
  utf8_bytes: number;
  max_chunks: number;
  max_characters: number;
  prompt_characters: number;
  max_prompt_characters: number;
}

export interface EvaluationStageMetrics {
  latency_ms: number;
  model_calls: number;
  exchanges: number;
  tool_calls: number;
  tokens: EvaluationTokenMetrics;
  cost: EvaluationCostMetrics;
  context: EvaluationContextMetrics;
}

export interface EvaluationMetrics {
  version: 1;
  latency_ms: { total: number; rag: number; judge: number };
  model_calls: { total: number; rag: number; judge: number; embedding: number };
  tokens: EvaluationTokenMetrics;
  cost: EvaluationCostMetrics;
  workflow: { exchanges: number; tool_calls: number; reused_searches: number };
  context: EvaluationContextMetrics;
  stages: Record<string, EvaluationStageMetrics>;
  pricing: {
    input_usd_per_million: number | null;
    output_usd_per_million: number | null;
    embedding_usd_per_million: number | null;
  };
}

export interface EvaluationCaseResult {
  case_index: number;
  input: EvaluationTestCase;
  retrieval_strategy: EvaluationWireRetrievalType;
  retrieved_chunks: EvaluationRetrievedChunk[];
  generated_answer: string | null;
  judge_scores: EvaluationJudgeScores | null;
  error: EvaluationCaseError | null;
  attempt_history: EvaluationCaseError[];
  metrics?: EvaluationMetrics | null;
  performance?: EvaluationPerformance | null;
}

export interface EvaluationAggregateScores {
  context_precision: number | null;
  context_recall: number | null;
  relevant_chunk_ranking: number | null;
  context_utilization: number | null;
  semantic_similarity: number | null;
  completeness: number | null;
  hallucination_rate: number | null;
}

export type EvaluationStreamEvent =
  | { event: "retryRoundStart"; retryRound: number; total: number; pending: number }
  | { event: "caseRetryStart"; retryRound: number; caseIndex: number; total: number }
  | { event: "caseRetryResult"; retryRound: number; completed: number; total: number; result: EvaluationCaseResult }
  | {
      event: "runStart";
      retrievalType: EvaluationWireRetrievalType;
      total: number;
    }
  | {
      event: "caseStart";
      caseIndex: number;
      total: number;
      retrievalType: EvaluationWireRetrievalType;
      input: EvaluationTestCase;
    }
  | {
      event: "caseSuccess";
      completed: number;
      total: number;
      result: EvaluationCaseResult;
    }
  | {
      event: "caseFailure";
      completed: number;
      total: number;
      result: EvaluationCaseResult;
    }
  | {
      event: "complete";
      retrievalType: EvaluationWireRetrievalType;
      completed: number;
      total: number;
      succeeded: number;
      failed: number;
      aggregateScores: EvaluationAggregateScores;
    }
  | { event: "error"; message: string };