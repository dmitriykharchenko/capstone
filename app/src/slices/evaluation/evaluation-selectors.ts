import { store } from "../../store";

export const selectEvaluationRunning = store.createSelector(
  (state) => state.evaluation.running,
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
export const selectEvaluationRetryRound = store.createSelector((state) => state.evaluation.retryRound);
export const selectEvaluationRetryPending = store.createSelector((state) => state.evaluation.retryPending);
export const selectEvaluationRetryCompleted = store.createSelector((state) => state.evaluation.retryCompleted);