import type { StoreAction, StoreReducerFunction } from "@augmentcode/themis/types";
import { createAction } from "@augmentcode/themis/utils/store/create-action";
import { createReducer } from "@augmentcode/themis/utils/store/create-reducer";
import type {
  EvaluationAggregateScores,
  EvaluationCaseResult,
  EvaluationRetrievalType,
  EvaluationTestCase,
} from "../../lib/api";
import {
  beginEvaluation,
  completeEvaluation,
  failEvaluation,
  initialEvaluationState,
  recordEvaluationCase,
  startEvaluationCase,
  type EvaluationState,
} from "./evaluation-types";

export const runEvaluation = createAction<
  [retrievalType: EvaluationRetrievalType, testCases: EvaluationTestCase[]]
>("evaluation/run");
export const evaluationStarted = createAction<
  [retrievalType: EvaluationRetrievalType, testCases: EvaluationTestCase[], total: number]
>("evaluation/started");
export const evaluationCaseStarted = createAction<[caseIndex: number]>(
  "evaluation/caseStarted",
);
export const evaluationCaseSucceeded = createAction<
  [result: EvaluationCaseResult, completed: number, total: number]
>("evaluation/caseSucceeded");
export const evaluationCaseFailed = createAction<
  [result: EvaluationCaseResult, completed: number, total: number]
>("evaluation/caseFailed");
export const evaluationCompleted = createAction<
  [
    completed: number,
    total: number,
    succeeded: number,
    failed: number,
    aggregateScores: EvaluationAggregateScores,
  ]
>("evaluation/completed");
export const evaluationTransportFailed = createAction<[error: string]>(
  "evaluation/transportFailed",
);

export const evaluationReducer: StoreReducerFunction<EvaluationState> =
  createReducer<EvaluationState>(initialEvaluationState)
    .with(
      runEvaluation,
      (
        state: EvaluationState,
        action: StoreAction<[EvaluationRetrievalType, EvaluationTestCase[]]>,
      ) =>
        state.running
          ? state
          : beginEvaluation(
              state,
              action.payload[0],
              action.payload[1],
              action.payload[1].length,
            ),
    )
    .with(
      evaluationStarted,
      (
        state: EvaluationState,
        action: StoreAction<[
          EvaluationRetrievalType,
          EvaluationTestCase[],
          number,
        ]>,
      ) => beginEvaluation(state, ...action.payload),
    )
    .with(
      evaluationCaseStarted,
      (state: EvaluationState, action: StoreAction<[number]>) =>
        startEvaluationCase(state, action.payload[0]),
    )
    .with(
      evaluationCaseSucceeded,
      (
        state: EvaluationState,
        action: StoreAction<[EvaluationCaseResult, number, number]>,
      ) => recordEvaluationCase(state, ...action.payload),
    )
    .with(
      evaluationCaseFailed,
      (
        state: EvaluationState,
        action: StoreAction<[EvaluationCaseResult, number, number]>,
      ) => recordEvaluationCase(state, ...action.payload),
    )
    .with(
      evaluationCompleted,
      (
        state: EvaluationState,
        action: StoreAction<[
          number,
          number,
          number,
          number,
          EvaluationAggregateScores,
        ]>,
      ) => completeEvaluation(state, ...action.payload),
    )
    .with(
      evaluationTransportFailed,
      (state: EvaluationState, action: StoreAction<[string]>) =>
        failEvaluation(state, action.payload[0]),
    );