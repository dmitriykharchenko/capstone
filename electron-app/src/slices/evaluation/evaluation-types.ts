import type {
  EvaluationAggregateScores,
  EvaluationCaseResult,
  EvaluationRetrievalType,
  EvaluationTestCase,
} from "../../lib/api";

export interface EvaluationState {
  retrievalType: EvaluationRetrievalType | null;
  testCases: EvaluationTestCase[];
  running: boolean;
  total: number;
  completed: number;
  succeeded: number;
  failed: number;
  currentCaseIndex: number | null;
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
    results: Array<EvaluationCaseResult | null>(total).fill(null),
    aggregateScores: null,
    error: null,
  };
}

export function startEvaluationCase(
  state: EvaluationState,
  caseIndex: number,
): EvaluationState {
  return { ...state, currentCaseIndex: caseIndex };
}

export function recordEvaluationCase(
  state: EvaluationState,
  result: EvaluationCaseResult,
  completed: number,
  total: number,
): EvaluationState {
  const results = [...state.results];
  if (result.case_index > 0 && result.case_index <= results.length) {
    results[result.case_index - 1] = result;
  }
  return {
    ...state,
    results,
    completed,
    total,
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
  return { ...state, running: false, currentCaseIndex: null, error };
}