import type { GraphEdge, GraphNode } from "../../lib/graph-types";

export type GraphExplorerStatus =
  | "idle"
  | "loading"
  | "success"
  | "empty"
  | "dependency-unavailable"
  | "store-unavailable"
  | "malformed-payload"
  | "transport-error";

export interface GraphExplorerState {
  status: GraphExplorerStatus;
  nodes: GraphNode[];
  edges: GraphEdge[];
  selectedNodeId: string | null;
  message: string | null;
}

export const initialGraphExplorerState: GraphExplorerState = {
  status: "idle",
  nodes: [],
  edges: [],
  selectedNodeId: null,
  message: null,
};

export function beginGraphLoad(state: GraphExplorerState): GraphExplorerState {
  return { ...state, status: "loading", message: null };
}

export function completeGraphLoad(
  state: GraphExplorerState,
  nodes: GraphNode[],
  edges: GraphEdge[],
): GraphExplorerState {
  const selectedNodeId = nodes.some((node) => node.id === state.selectedNodeId)
    ? state.selectedNodeId
    : null;
  return {
    status: nodes.length === 0 ? "empty" : "success",
    nodes,
    edges,
    selectedNodeId,
    message: null,
  };
}

export function failGraphLoad(
  state: GraphExplorerState,
  status: Exclude<GraphExplorerStatus, "idle" | "loading" | "success" | "empty">,
  message: string,
): GraphExplorerState {
  return { ...state, status, nodes: [], edges: [], selectedNodeId: null, message };
}