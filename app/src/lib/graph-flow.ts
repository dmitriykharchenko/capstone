import type { Edge, Node } from "@xyflow/svelte";
import { layoutGraph } from "./graph-layout";
import { compareEdges, type GraphEdge, type GraphNode, type GraphNodeKind } from "./graph-types";

export interface GraphFlowNodeData extends Record<string, unknown> {
  kind: GraphNodeKind;
  label: string;
  onActivate: () => void;
}

export type GraphFlowNode = Node<GraphFlowNodeData, "graph">;
export type GraphFlowEdge = Edge<{ kind: GraphEdge["kind"] }, "straight">;

export interface GraphFlowLayoutState {
  layoutKey: string;
  nodes: GraphFlowNode[];
}

export function graphLayoutKey(nodes: GraphNode[], edges: GraphEdge[]): string {
  return JSON.stringify({
    nodes: nodes.map(({ id, kind }) => [id, kind]).sort((left, right) =>
      left[0].localeCompare(right[0]) || left[1].localeCompare(right[1])),
    edges: [...edges].sort(compareEdges).map(({ sourceId, targetId, kind }) => [sourceId, targetId, kind]),
  });
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
    draggable: true,
    focusable: false,
    ariaLabel: `${node.kind}: ${node.label}`,
  }));
}

export function reconcileFlowNodes(
  currentLayoutKey: string | null,
  currentNodes: GraphFlowNode[],
  nodes: GraphNode[],
  edges: GraphEdge[],
  onActivate: (node: GraphNode) => void,
): GraphFlowLayoutState {
  const layoutKey = graphLayoutKey(nodes, edges);
  return layoutKey === currentLayoutKey
    ? { layoutKey, nodes: currentNodes }
    : { layoutKey, nodes: toFlowNodes(nodes, edges, onActivate) };
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