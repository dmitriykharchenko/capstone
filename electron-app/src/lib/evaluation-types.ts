export const EVALUATION_RETRIEVAL_TYPE = "multihop-hybrid" as const;
export type EvaluationRetrievalType = typeof EVALUATION_RETRIEVAL_TYPE;

export const EVALUATION_WIRE_RETRIEVAL_TYPE = "hybrid" as const;
export type EvaluationWireRetrievalType = typeof EVALUATION_WIRE_RETRIEVAL_TYPE;

export interface EvaluationTestCase {
  question: string;
  grading_notes: string;
  sources: string[];
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

export interface EvaluationCaseError {
  stage: string;
  message: string;
}

export interface EvaluationCaseResult {
  case_index: number;
  input: EvaluationTestCase;
  retrieval_strategy: EvaluationWireRetrievalType;
  retrieved_chunks: EvaluationRetrievedChunk[];
  generated_answer: string | null;
  judge_scores: EvaluationJudgeScores | null;
  error: EvaluationCaseError | null;
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