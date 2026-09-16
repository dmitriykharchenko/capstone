export type GraphNodeKind = "paper" | "author" | "topic";
export type GraphEdgeKind = "authored_by" | "has_topic";

export interface GraphNode {
  id: string;
  kind: GraphNodeKind;
  label: string;
}

export interface GraphEdge {
  sourceId: string;
  targetId: string;
  kind: GraphEdgeKind;
}

export interface GraphNeighborhoodResponse {
  enabled: boolean;
  available: boolean;
  reason: string | null;
  nodes: GraphNode[];
  edges: GraphEdge[];
}

export class GraphPayloadError extends Error {
  constructor(message: string) {
    super(`Malformed graph payload: ${message}`);
    this.name = "GraphPayloadError";
  }
}

const NODE_KINDS = new Set<GraphNodeKind>(["paper", "author", "topic"]);
const EDGE_KINDS = new Set<GraphEdgeKind>(["authored_by", "has_topic"]);

function record(value: unknown, name: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new GraphPayloadError(`${name} must be an object`);
  }
  return value as Record<string, unknown>;
}

function text(value: unknown, name: string): string {
  if (typeof value !== "string" || value.trim() === "") {
    throw new GraphPayloadError(`${name} must be a non-empty string`);
  }
  return value;
}

export function parseGraphNeighborhoodPayload(value: unknown): GraphNeighborhoodResponse {
  const payload = record(value, "response");
  if (typeof payload.enabled !== "boolean" || typeof payload.available !== "boolean") {
    throw new GraphPayloadError("enabled and available must be booleans");
  }
  if (payload.query !== "neighborhood") {
    throw new GraphPayloadError("query must be neighborhood");
  }
  const reason = payload.reason === undefined ? null : text(payload.reason, "reason");
  const neighborhood = record(payload.neighborhood, "neighborhood");
  if (!Array.isArray(neighborhood.nodes) || !Array.isArray(neighborhood.edges)) {
    throw new GraphPayloadError("nodes and edges must be arrays");
  }

  const nodeIds = new Set<string>();
  const nodes = neighborhood.nodes.map((item, index): GraphNode => {
    const node = record(item, `nodes[${index}]`);
    const id = text(node.node_id, `nodes[${index}].node_id`);
    const kind = text(node.kind, `nodes[${index}].kind`) as GraphNodeKind;
    if (!NODE_KINDS.has(kind) || nodeIds.has(id)) {
      throw new GraphPayloadError(`nodes[${index}] has invalid or duplicate identity`);
    }
    nodeIds.add(id);
    return { id, kind, label: text(node.label, `nodes[${index}].label`) };
  });

  const edgeIds = new Set<string>();
  const edges = neighborhood.edges.map((item, index): GraphEdge => {
    const edge = record(item, `edges[${index}]`);
    const sourceId = text(edge.source_id, `edges[${index}].source_id`);
    const targetId = text(edge.target_id, `edges[${index}].target_id`);
    const kind = text(edge.kind, `edges[${index}].kind`) as GraphEdgeKind;
    const identity = `${kind}\u0000${sourceId}\u0000${targetId}`;
    if (!EDGE_KINDS.has(kind) || !nodeIds.has(sourceId) || !nodeIds.has(targetId)) {
      throw new GraphPayloadError(`edges[${index}] has an invalid kind or endpoint`);
    }
    if (edgeIds.has(identity)) {
      throw new GraphPayloadError(`edges[${index}] is duplicated`);
    }
    edgeIds.add(identity);
    return { sourceId, targetId, kind };
  });

  return {
    enabled: payload.enabled,
    available: payload.available,
    reason,
    nodes: nodes.sort(compareNodes),
    edges: edges.sort(compareEdges),
  };
}

export function compareNodes(left: GraphNode, right: GraphNode): number {
  return left.kind.localeCompare(right.kind) || left.label.localeCompare(right.label) || left.id.localeCompare(right.id);
}

export function compareEdges(left: GraphEdge, right: GraphEdge): number {
  return left.sourceId.localeCompare(right.sourceId) || left.targetId.localeCompare(right.targetId) || left.kind.localeCompare(right.kind);
}

export function mergeGraphNeighborhoods(
  responses: GraphNeighborhoodResponse[],
): { nodes: GraphNode[]; edges: GraphEdge[] } {
  const nodes = new Map<string, GraphNode>();
  const edges = new Map<string, GraphEdge>();
  for (const response of responses) {
    for (const node of response.nodes) nodes.set(node.id, node);
    for (const edge of response.edges) {
      edges.set(`${edge.kind}\u0000${edge.sourceId}\u0000${edge.targetId}`, edge);
    }
  }
  return {
    nodes: [...nodes.values()].sort(compareNodes),
    edges: [...edges.values()].sort(compareEdges),
  };
}