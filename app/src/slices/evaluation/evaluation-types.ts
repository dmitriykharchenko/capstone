import type {
  EvaluationAggregateScores,
  EvaluationCaseResult,
  EvaluationRetrievalType,
  EvaluationTestCase,
} from "../../lib/api";
import { evaluationTransportError, sanitizeEvaluationResult } from "../../lib/evaluation-diagnostics";

export interface EvaluationState {
  retrievalType: EvaluationRetrievalType | null;
  testCases: EvaluationTestCase[];
  running: boolean;
  total: number;
  completed: number;
  succeeded: number;
  failed: number;
  currentCaseIndex: number | null;
  retryRound: number;
  retryPending: number;
  retryCompleted: number;
  results: Array<EvaluationCaseResult | null>;
  aggregateScores: EvaluationAggregateScores | null;
  error: string | null;
}

export const initialEvaluationState: EvaluationState = {
  retrievalType: null,
  testCases: [],
  running: false,
  total: 0,
  completed: 0,
  succeeded: 0,
  failed: 0,
  currentCaseIndex: null,
  retryRound: 0,
  retryPending: 0,
  retryCompleted: 0,
  results: [],
  aggregateScores: null,
  error: null,
};

export function beginEvaluation(
  state: EvaluationState,
  testCases: EvaluationTestCase[],
  total: number,
): EvaluationState {
  return {
    ...state,
    retrievalType: "multihop-hybrid",
    testCases: [...testCases],
    running: true,
    total,
    completed: 0,
    succeeded: 0,
    failed: 0,
    currentCaseIndex: null,
    retryRound: 0,
    retryPending: 0,
    retryCompleted: 0,
    results: Array<EvaluationCaseResult | null>(total).fill(null),
    aggregateScores: null,
    error: null,
  };
}

export function startEvaluationCase(
  state: EvaluationState,
  caseIndex: number,
): EvaluationState {
  if (!state.running || !Number.isInteger(caseIndex) || caseIndex < 1 || caseIndex > state.total) return state;
  return { ...state, currentCaseIndex: caseIndex };
}

export function startEvaluationRetryRound(state: EvaluationState, round: number, pending: number): EvaluationState {
  if (!state.running || state.completed !== state.total || round !== state.retryRound + 1 || round > 2 || !Number.isInteger(pending) || pending < 1 || pending > state.total) return state;
  return { ...state, retryRound: round, retryPending: pending, retryCompleted: 0, currentCaseIndex: null };
}

export function recordEvaluationCase(
  state: EvaluationState,
  result: EvaluationCaseResult,
  completed: number,
  total: number,
): EvaluationState {
  if (!state.running || total !== state.total || !Number.isInteger(completed) || !Number.isInteger(result.case_index) || result.case_index < 1 || result.case_index > state.total) return state;
  const results = [...state.results];
  if (result.case_index > 0 && result.case_index <= results.length) {
    results[result.case_index - 1] = sanitizeEvaluationResult(result);
  }
  return {
    ...state,
    results,
    completed: Math.min(state.total, Math.max(state.completed, completed)),
    total: state.total,
    retryCompleted: state.retryRound > 0 ? Math.min(state.retryPending, state.retryCompleted + 1) : 0,
    currentCaseIndex: null,
    succeeded: results.filter((item) => item !== null && item.error === null).length,
    failed: results.filter((item) => item?.error != null).length,
  };
}

export function completeEvaluation(
  state: EvaluationState,
  completed: number,
  total: number,
  succeeded: number,
  failed: number,
  aggregateScores: EvaluationAggregateScores,
): EvaluationState {
  return {
    ...state,
    running: false,
    completed,
    total,
    succeeded,
    failed,
    currentCaseIndex: null,
    aggregateScores,
  };
}

export function failEvaluation(
  state: EvaluationState,
  error: string,
): EvaluationState {
  return { ...state, running: false, currentCaseIndex: null, error: error.trim() || evaluationTransportError };
}