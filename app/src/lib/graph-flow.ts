import type { Edge, Node } from "@xyflow/svelte";
import { layoutGraph } from "./graph-layout";
import type { GraphEdge, GraphNode, GraphNodeKind } from "./graph-types";

export interface GraphFlowNodeData extends Record<string, unknown> {
  kind: GraphNodeKind;
  label: string;
  onActivate: () => void;
}

export type GraphFlowNode = Node<GraphFlowNodeData, "graph">;
export type GraphFlowEdge = Edge<{ kind: GraphEdge["kind"] }, "straight">;

export interface VisibleGraph {
  nodes: GraphNode[];
  edges: GraphEdge[];
}

export function filterGraphByQuery(
  nodes: GraphNode[],
  edges: GraphEdge[],
  query: string,
): VisibleGraph {
  const normalizedQuery = query.trim().toLowerCase();
  if (!normalizedQuery) return { nodes: [...nodes], edges: [...edges] };

  const matchingIds = new Set(
    nodes
      .filter((node) => node.label.trim().toLowerCase().includes(normalizedQuery))
      .map((node) => node.id),
  );
  if (matchingIds.size === 0) return { nodes: [], edges: [] };

  const visibleIds = new Set(matchingIds);
  for (const edge of edges) {
    if (matchingIds.has(edge.sourceId)) visibleIds.add(edge.targetId);
    if (matchingIds.has(edge.targetId)) visibleIds.add(edge.sourceId);
  }
  return {
    nodes: nodes.filter((node) => visibleIds.has(node.id)),
    edges: edges.filter((edge) => visibleIds.has(edge.sourceId) && visibleIds.has(edge.targetId)),
  };
}

export function toFlowNodes(
  nodes: GraphNode[],
  edges: GraphEdge[],
  onActivate: (node: GraphNode) => void,
): GraphFlowNode[] {
  return layoutGraph(nodes, edges).nodes.map((node) => ({
    id: node.id,
    type: "graph",
    position: { x: node.x, y: node.y },
    data: {
      kind: node.kind,
      label: node.label,
      onActivate: () => onActivate(node),
    },
    class: `graph-node-${node.kind}`,
    selectable: true,
    draggable: false,
    focusable: false,
    ariaLabel: `${node.kind}: ${node.label}`,
  }));
}

export function toFlowEdges(edges: GraphEdge[]): GraphFlowEdge[] {
  return edges.map((edge) => ({
    id: `${edge.kind}:${edge.sourceId}:${edge.targetId}`,
    source: edge.sourceId,
    target: edge.targetId,
    sourceHandle: edge.kind,
    targetHandle: edge.kind,
    type: "straight",
    label: edge.kind === "authored_by" ? "authored by" : "has topic",
    ariaLabel: edge.kind === "authored_by" ? "authored by" : "has topic",
    data: { kind: edge.kind },
    focusable: false,
  }));
}