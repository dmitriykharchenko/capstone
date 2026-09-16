import type { StoreAction, StoreReducerFunction } from "@augmentcode/themis/types";
import { createAction } from "@augmentcode/themis/utils/store/create-action";
import { createReducer } from "@augmentcode/themis/utils/store/create-reducer";
import type { GraphEdge, GraphNode } from "../../lib/graph-types";
import {
  beginGraphLoad,
  completeGraphLoad,
  failGraphLoad,
  initialGraphExplorerState,
  type GraphExplorerState,
  type GraphExplorerStatus,
} from "./graph-explorer-types";

type GraphFailureStatus = Exclude<
  GraphExplorerStatus,
  "idle" | "loading" | "success" | "empty"
>;

export const loadGraph = createAction("graphExplorer/load");
export const graphLoading = createAction("graphExplorer/loading");
export const graphLoaded = createAction<[nodes: GraphNode[], edges: GraphEdge[]]>(
  "graphExplorer/loaded",
);
export const graphFailed = createAction<[status: GraphFailureStatus, message: string]>(
  "graphExplorer/failed",
);
export const selectGraphNode = createAction<[nodeId: string | null]>(
  "graphExplorer/selectNode",
);

export const graphExplorerReducer: StoreReducerFunction<GraphExplorerState> =
  createReducer<GraphExplorerState>(initialGraphExplorerState)
    .with(graphLoading, (state: GraphExplorerState) => beginGraphLoad(state))
    .with(
      graphLoaded,
      (state: GraphExplorerState, action: StoreAction<[GraphNode[], GraphEdge[]]>) =>
        completeGraphLoad(state, ...action.payload),
    )
    .with(
      graphFailed,
      (state: GraphExplorerState, action: StoreAction<[GraphFailureStatus, string]>) =>
        failGraphLoad(state, ...action.payload),
    )
    .with(
      selectGraphNode,
      (state: GraphExplorerState, action: StoreAction<[string | null]>) => ({
        ...state,
        selectedNodeId: state.nodes.some((node) => node.id === action.payload[0])
          ? action.payload[0]
          : null,
      }),
    );