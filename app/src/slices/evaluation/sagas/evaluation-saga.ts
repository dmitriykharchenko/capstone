import { call, put, take, takeLeading } from "typed-redux-saga";
import { END, eventChannel, type EventChannel } from "redux-saga";
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
  evaluationTransportFailed,
  runEvaluation,
} from "../evaluation-slice";

export interface EvaluationServices {
  runEvaluation: typeof api.runEvaluation;
}

const evaluationServices: EvaluationServices = {
  runEvaluation: api.runEvaluation,
};

const EVALUATION_TRANSPORT_ERROR =
  "Evaluation transport failed. Check the backend logs and retry.";

function createEvaluationChannel(
  testCases: EvaluationTestCase[],
  services: EvaluationServices,
): EventChannel<EvaluationStreamEvent> {
  return eventChannel<EvaluationStreamEvent>((emit) => {
    let terminalEventSeen = false;
    services
      .runEvaluation(testCases, (event) => {
        terminalEventSeen ||= event.event === "complete" || event.event === "error";
        emit(event);
      })
      .catch(() => {
        if (!terminalEventSeen) {
          terminalEventSeen = true;
          emit({ event: "error", message: EVALUATION_TRANSPORT_ERROR });
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
  });
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
      yield* put(evaluationTransportFailed(EVALUATION_TRANSPORT_ERROR));
    } finally {
      yield* call([channel, channel.close]);
    }
  });
}