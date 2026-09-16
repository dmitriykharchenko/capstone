import { compareNodes, type GraphEdge, type GraphNode, type GraphNodeKind } from "./graph-types";

export interface PositionedGraphNode extends GraphNode {
  x: number;
  y: number;
}

export interface GraphLayout {
  width: number;
  height: number;
  nodes: PositionedGraphNode[];
}

const COLUMNS: Record<GraphNodeKind, number> = { author: 92, paper: 322, topic: 552 };

export function layoutGraph(nodes: GraphNode[]): GraphLayout {
  const counts: Record<GraphNodeKind, number> = { author: 0, paper: 0, topic: 0 };
  const positioned = [...nodes].sort(compareNodes).map((node) => ({
    ...node,
    x: COLUMNS[node.kind],
    y: 42 + counts[node.kind]++ * 64,
  }));
  return {
    width: 644,
    height: Math.max(260, 84 + Math.max(...Object.values(counts)) * 64),
    nodes: positioned,
  };
}

export function connectedNodes(
  selectedId: string | null,
  nodes: GraphNode[],
  edges: GraphEdge[],
): GraphNode[] {
  if (!selectedId) return [];
  const ids = new Set<string>();
  for (const edge of edges) {
    if (edge.sourceId === selectedId) ids.add(edge.targetId);
    if (edge.targetId === selectedId) ids.add(edge.sourceId);
  }
  return nodes.filter((node) => ids.has(node.id)).sort(compareNodes);
}