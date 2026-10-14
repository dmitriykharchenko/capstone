import { call, put, take, takeLeading } from "typed-redux-saga";
import { END, buffers, eventChannel, type EventChannel } from "redux-saga";
import { evaluationTransportError, sanitizeEvaluationResult } from "../../../lib/evaluation-diagnostics";
import {
  api,
  type EvaluationStreamEvent,
  type EvaluationTestCase,
} from "../../../lib/api";
import {
  evaluationCaseFailed,
  evaluationCaseStarted,
  evaluationCaseSucceeded,
  evaluationCompleted,
  evaluationStarted,
  evaluationRetryRoundStarted,
  evaluationTransportFailed,
  runEvaluation,
} from "../evaluation-slice";

export interface EvaluationServices {
  runEvaluation: typeof api.runEvaluation;
}

const evaluationServices: EvaluationServices = {
  runEvaluation: api.runEvaluation,
};

function createEvaluationChannel(
  testCases: EvaluationTestCase[],
  services: EvaluationServices,
): EventChannel<EvaluationStreamEvent> {
  return eventChannel<EvaluationStreamEvent>((emit) => {
    let terminalEventSeen = false;
    services
      .runEvaluation(testCases, (event) => {
        if (terminalEventSeen) return;
        terminalEventSeen ||= event.event === "complete" || event.event === "error";
        emit("result" in event ? { ...event, result: sanitizeEvaluationResult(event.result) } : event);
      })
      .catch((error: unknown) => {
        if (!terminalEventSeen) {
          terminalEventSeen = true;
          emit({ event: "error", message: String(error) || evaluationTransportError });
        }
      })
      .finally(() => {
        if (!terminalEventSeen) {
          emit({
            event: "error",
            message: "Evaluation stream ended before terminal completion. Please try again.",
          });
        }
        emit(END);
      });
    return () => {};
  }, buffers.expanding());
}

export function* evaluationSaga(services: EvaluationServices = evaluationServices) {
  yield* takeLeading(runEvaluation, function* (action) {
    const [testCases] = action.payload;
    const channel = createEvaluationChannel(testCases, services);
    try {
      while (true) {
        const event = yield* take(channel);
        if (event.event === "runStart") {
          yield* put(evaluationStarted(testCases, event.total));
        } else if (event.event === "caseStart") {
          yield* put(evaluationCaseStarted(event.caseIndex));
        } else if (event.event === "caseSuccess") {
          yield* put(evaluationCaseSucceeded(event.result, event.completed, event.total));
        } else if (event.event === "caseFailure") {
          yield* put(evaluationCaseFailed(event.result, event.completed, event.total));
        } else if (event.event === "retryRoundStart") {
          yield* put(evaluationRetryRoundStarted(event.retryRound, event.pending));
        } else if (event.event === "caseRetryStart") {
          yield* put(evaluationCaseStarted(event.caseIndex));
        } else if (event.event === "caseRetryResult") {
          yield* put((event.result.error ? evaluationCaseFailed : evaluationCaseSucceeded)(event.result, event.completed, event.total));
        } else if (event.event === "complete") {
          yield* put(
            evaluationCompleted(
              event.completed,
              event.total,
              event.succeeded,
              event.failed,
              event.aggregateScores,
            ),
          );
          break;
        } else if (event.event === "error") {
          yield* put(evaluationTransportFailed(event.message));
          break;
        }
      }
    } catch {
      yield* put(evaluationTransportFailed(evaluationTransportError));
    } finally {
      yield* call([channel, channel.close]);
    }
  });
}