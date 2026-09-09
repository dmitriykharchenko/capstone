import { store } from "../../store";

export const selectEvaluationRunning = store.createSelector(
  (state) => state.evaluation.running,
);
export const selectEvaluationRetrievalType = store.createSelector(
  (state) => state.evaluation.retrievalType,
);
export const selectEvaluationTestCases = store.createSelector(
  (state) => state.evaluation.testCases,
);
export const selectEvaluationTotal = store.createSelector(
  (state) => state.evaluation.total,
);
export const selectEvaluationCompleted = store.createSelector(
  (state) => state.evaluation.completed,
);
export const selectEvaluationSucceeded = store.createSelector(
  (state) => state.evaluation.succeeded,
);
export const selectEvaluationFailed = store.createSelector(
  (state) => state.evaluation.failed,
);
export const selectEvaluationCurrentCaseIndex = store.createSelector(
  (state) => state.evaluation.currentCaseIndex,
);
export const selectEvaluationResults = store.createSelector(
  (state) => state.evaluation.results,
);
export const selectEvaluationAggregateScores = store.createSelector(
  (state) => state.evaluation.aggregateScores,
);
export const selectEvaluationError = store.createSelector(
  (state) => state.evaluation.error,
);