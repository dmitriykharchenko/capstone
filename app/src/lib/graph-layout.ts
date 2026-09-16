import { compareNodes, type GraphEdge, type GraphNode } from "./graph-types";

export interface PositionedGraphNode extends GraphNode {
  x: number;
  y: number;
}

export interface GraphLayout {
  width: number;
  height: number;
  nodes: PositionedGraphNode[];
}

const NODE_WIDTH = 176;
const NODE_HEIGHT = 56;
const ROW_STEP = NODE_HEIGHT + 24;
const COLUMN_STEP = NODE_WIDTH + 72;
const NEIGHBORHOOD_GAP = 128;
const PADDING = 48;
const MIN_WIDTH = 520;
const MIN_HEIGHT = 320;
const MAX_ROW_WIDTH = 2400;

interface Neighborhood {
  columns: GraphNode[][];
  width: number;
  height: number;
}

function addConnection(connections: Map<string, Set<string>>, from: string, to: string): void {
  const connected = connections.get(from) ?? new Set<string>();
  connected.add(to);
  connections.set(from, connected);
}

function connectedById(
  connections: Map<string, Set<string>>,
  id: string,
  nodesById: Map<string, GraphNode>,
): GraphNode[] {
  return [...(connections.get(id) ?? [])]
    .map((connectedId) => nodesById.get(connectedId))
    .filter((node): node is GraphNode => node !== undefined)
    .sort(compareNodes);
}

function neighborhood(columns: GraphNode[][]): Neighborhood {
  const lastColumn = columns.reduce((last, column, index) => column.length > 0 ? index : last, 0);
  const rows = Math.max(1, ...columns.map((column) => column.length));
  return {
    columns,
    width: NODE_WIDTH + lastColumn * COLUMN_STEP,
    height: NODE_HEIGHT + (rows - 1) * ROW_STEP,
  };
}

function graphNeighborhoods(nodes: GraphNode[], edges: GraphEdge[]): Neighborhood[] {
  const orderedNodes = [...nodes].sort(compareNodes);
  const nodesById = new Map(orderedNodes.map((node) => [node.id, node]));
  const authorPapers = new Map<string, Set<string>>();
  const paperTopics = new Map<string, Set<string>>();
  for (const edge of edges) {
    const source = nodesById.get(edge.sourceId);
    const target = nodesById.get(edge.targetId);
    if (!source || !target) continue;
    const pair = [source, target];
    const author = pair.find((node) => node.kind === "author");
    const paper = pair.find((node) => node.kind === "paper");
    const topic = pair.find((node) => node.kind === "topic");
    if (edge.kind === "authored_by" && author && paper) addConnection(authorPapers, author.id, paper.id);
    if (edge.kind === "has_topic" && paper && topic) addConnection(paperTopics, paper.id, topic.id);
  }

  const placed = new Set<string>();
  const groups: Neighborhood[] = [];
  for (const author of orderedNodes.filter((node) => node.kind === "author")) {
    placed.add(author.id);
    const connectedPapers = connectedById(authorPapers, author.id, nodesById);
    const papers = connectedPapers.filter((paper) => !placed.has(paper.id));
    papers.forEach((paper) => placed.add(paper.id));
    const topics: GraphNode[] = [];
    for (const paper of connectedPapers) {
      for (const topic of connectedById(paperTopics, paper.id, nodesById)) {
        if (!placed.has(topic.id)) {
          placed.add(topic.id);
          topics.push(topic);
        }
      }
    }
    groups.push(neighborhood([[author], papers, topics]));
  }
  for (const paper of orderedNodes.filter((node) => node.kind === "paper" && !placed.has(node.id))) {
    placed.add(paper.id);
    const topics = connectedById(paperTopics, paper.id, nodesById).filter((topic) => !placed.has(topic.id));
    topics.forEach((topic) => placed.add(topic.id));
    groups.push(neighborhood([[paper], topics]));
  }
  const remaining = orderedNodes.filter((node) => !placed.has(node.id));
  if (remaining.length > 0) groups.push(neighborhood([remaining]));
  return groups;
}

function place(group: Neighborhood, left: number, top: number): PositionedGraphNode[] {
  const rows = Math.max(1, ...group.columns.map((column) => column.length));
  return group.columns.flatMap((column, columnIndex) => {
    const offsetY = (rows - column.length) * ROW_STEP / 2;
    return column.map((node, rowIndex) => ({
      ...node,
      x: left + columnIndex * COLUMN_STEP,
      y: top + offsetY + rowIndex * ROW_STEP,
    }));
  });
}

export function layoutGraph(nodes: GraphNode[], edges: GraphEdge[] = []): GraphLayout {
  if (nodes.length === 0) return { width: MIN_WIDTH, height: MIN_HEIGHT, nodes: [] };
  const positioned: PositionedGraphNode[] = [];
  let left = PADDING;
  let top = PADDING;
  let rowHeight = 0;
  let right = PADDING;
  let bottom = PADDING;
  for (const group of graphNeighborhoods(nodes, edges)) {
    if (left > PADDING && left + group.width > MAX_ROW_WIDTH - PADDING) {
      left = PADDING;
      top += rowHeight + NEIGHBORHOOD_GAP;
      rowHeight = 0;
    }
    positioned.push(...place(group, left, top));
    right = Math.max(right, left + group.width);
    bottom = Math.max(bottom, top + group.height);
    left += group.width + NEIGHBORHOOD_GAP;
    rowHeight = Math.max(rowHeight, group.height);
  }
  return {
    width: Math.max(MIN_WIDTH, right + PADDING),
    height: Math.max(MIN_HEIGHT, bottom + PADDING),
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