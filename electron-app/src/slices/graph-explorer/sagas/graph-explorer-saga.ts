import { call, put, select, takeLatest } from "typed-redux-saga";
import type { PaperInfo } from "../../../lib/api";
import { api } from "../../../lib/api";
import {
  GraphPayloadError,
  mergeGraphNeighborhoods,
  type GraphNeighborhoodResponse,
} from "../../../lib/graph-types";
import { paperIndexSucceeded, setPapers } from "../../papers/papers-slice";
import { graphFailed, graphLoaded, graphLoading, loadGraph } from "../graph-explorer-slice";

export interface GraphExplorerServices {
  graphNeighborhood: typeof api.graphNeighborhood;
}

const services: GraphExplorerServices = { graphNeighborhood: api.graphNeighborhood };

export function* loadGraphForPapers(
  papers: PaperInfo[],
  graphServices: GraphExplorerServices = services,
) {
  yield* put(graphLoading());
  try {
    const responses: GraphNeighborhoodResponse[] = [];
    const paperIds = [...new Set(papers.map((paper) => paper.id))].sort();
    for (const paperId of paperIds) {
      const response = yield* call(graphServices.graphNeighborhood, paperId, 1);
      if (!response.enabled) {
        yield* put(
          graphFailed(
            "dependency-unavailable",
            response.reason ?? "Embedded graph support is unavailable.",
          ),
        );
        return;
      }
      if (!response.available) {
        yield* put(
          graphFailed("store-unavailable", response.reason ?? "Paper graph store is unavailable."),
        );
        return;
      }
      responses.push(response);
    }
    const graph = mergeGraphNeighborhoods(responses);
    yield* put(graphLoaded(graph.nodes, graph.edges));
  } catch (error) {
    if (error instanceof GraphPayloadError) {
      yield* put(graphFailed("malformed-payload", "Graph data was malformed."));
    } else {
      yield* put(
        graphFailed("transport-error", "Graph transport failed. Check the backend and retry."),
      );
    }
  }
}

export function* graphExplorerSaga(graphServices: GraphExplorerServices = services) {
  yield* takeLatest(setPapers, function* (action) {
    yield* call(loadGraphForPapers, action.payload[0], graphServices);
  });
  yield* takeLatest(loadGraph, function* () {
    const papers = (yield* select((state) => state.papers.items)) as PaperInfo[];
    yield* call(loadGraphForPapers, papers, graphServices);
  });
  yield* takeLatest(paperIndexSucceeded, function* () {
    const papers = (yield* select((state) => state.papers.items)) as PaperInfo[];
    yield* call(loadGraphForPapers, papers, graphServices);
  });
}