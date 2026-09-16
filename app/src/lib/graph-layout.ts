import { compareEdges, compareNodes, type GraphEdge, type GraphNode } from "./graph-types";

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
const COLLISION_GAP = 24;
const PADDING = 48;
const MIN_WIDTH = 520;
const MIN_HEIGHT = 320;
const MAX_SIZE = 2400;
const ITERATIONS = 120;

interface Body {
  node: GraphNode;
  x: number;
  y: number;
  vx: number;
  vy: number;
}

function stableHash(value: string): number {
  let hash = 2166136261;
  for (let index = 0; index < value.length; index += 1) {
    hash ^= value.charCodeAt(index);
    hash = Math.imul(hash, 16777619);
  }
  return hash >>> 0;
}

function clamp(value: number, minimum: number, maximum: number): number {
  return Math.min(maximum, Math.max(minimum, value));
}

function layoutDimensions(count: number): { width: number; height: number } {
  const columns = Math.max(1, Math.ceil(Math.sqrt(count * 1.4)));
  const rows = Math.max(1, Math.ceil(count / columns));
  return {
    width: clamp(PADDING * 2 + NODE_WIDTH + (columns - 1) * (NODE_WIDTH + COLLISION_GAP), MIN_WIDTH, MAX_SIZE),
    height: clamp(PADDING * 2 + NODE_HEIGHT + (rows - 1) * (NODE_HEIGHT + COLLISION_GAP), MIN_HEIGHT, MAX_SIZE),
  };
}

function separateCollisions(bodies: Body[], width: number, height: number): void {
  const maxX = width - PADDING - NODE_WIDTH;
  const maxY = height - PADDING - NODE_HEIGHT;
  const slots: { x: number; y: number }[] = [];
  for (let y = PADDING; y <= maxY; y += NODE_HEIGHT + COLLISION_GAP) {
    for (let x = PADDING; x <= maxX; x += NODE_WIDTH + COLLISION_GAP) slots.push({ x, y });
  }
  for (const body of bodies) {
    if (slots.length === 0) break;
    slots.sort((left, right) => {
      const leftDistance = (left.x - body.x) ** 2 + (left.y - body.y) ** 2;
      const rightDistance = (right.x - body.x) ** 2 + (right.y - body.y) ** 2;
      return leftDistance - rightDistance || left.y - right.y || left.x - right.x;
    });
    const nearest = slots.shift()!;
    body.x = nearest.x;
    body.y = nearest.y;
  }
}

export function layoutGraph(nodes: GraphNode[], edges: GraphEdge[] = []): GraphLayout {
  const orderedNodes = [...nodes].sort(compareNodes);
  const { width, height } = layoutDimensions(orderedNodes.length);
  if (orderedNodes.length === 0) return { width, height, nodes: [] };

  const centerX = (width - NODE_WIDTH) / 2;
  const centerY = (height - NODE_HEIGHT) / 2;
  if (orderedNodes.length === 1) {
    return { width, height, nodes: [{ ...orderedNodes[0], x: centerX, y: centerY }] };
  }
  const radiusX = Math.max(1, centerX - PADDING);
  const radiusY = Math.max(1, centerY - PADDING);
  const bodies: Body[] = orderedNodes.map((node) => {
    const angle = (stableHash(node.id) / 0x100000000) * Math.PI * 2;
    const radius = Math.sqrt((stableHash(`${node.id}\0radius`) + 1) / 0x100000001);
    return {
      node,
      x: centerX + Math.cos(angle) * radiusX * radius,
      y: centerY + Math.sin(angle) * radiusY * radius,
      vx: 0,
      vy: 0,
    };
  });
  const bodyIndexes = new Map(bodies.map((body, index) => [body.node.id, index]));
  const orderedEdges = [...edges].sort(compareEdges);
  const maxX = width - PADDING - NODE_WIDTH;
  const maxY = height - PADDING - NODE_HEIGHT;

  for (let iteration = 0; iteration < ITERATIONS; iteration += 1) {
    const alpha = 1 - iteration / ITERATIONS;
    const forces = bodies.map(() => ({ x: 0, y: 0 }));
    for (let left = 0; left < bodies.length; left += 1) {
      for (let right = left + 1; right < bodies.length; right += 1) {
        let dx = bodies[right].x - bodies[left].x;
        let dy = bodies[right].y - bodies[left].y;
        if (dx === 0 && dy === 0) {
          const angle = (stableHash(`${bodies[left].node.id}\0${bodies[right].node.id}`) / 0x100000000) * Math.PI * 2;
          dx = Math.cos(angle);
          dy = Math.sin(angle);
        }
        const distance = Math.max(1, Math.hypot(dx, dy));
        const repulsion = 9000 / Math.max(576, distance * distance);
        const unitX = dx / distance;
        const unitY = dy / distance;
        forces[left].x -= unitX * repulsion;
        forces[left].y -= unitY * repulsion;
        forces[right].x += unitX * repulsion;
        forces[right].y += unitY * repulsion;

        const overlapX = NODE_WIDTH + COLLISION_GAP - Math.abs(dx);
        const overlapY = NODE_HEIGHT + COLLISION_GAP - Math.abs(dy);
        if (overlapX > 0 && overlapY > 0) {
          if (overlapX < overlapY) {
            const push = Math.sign(dx || 1) * overlapX * 0.18;
            forces[left].x -= push;
            forces[right].x += push;
          } else {
            const push = Math.sign(dy || 1) * overlapY * 0.18;
            forces[left].y -= push;
            forces[right].y += push;
          }
        }
      }
    }

    for (const edge of orderedEdges) {
      const sourceIndex = bodyIndexes.get(edge.sourceId);
      const targetIndex = bodyIndexes.get(edge.targetId);
      if (sourceIndex === undefined || targetIndex === undefined || sourceIndex === targetIndex) continue;
      const source = bodies[sourceIndex];
      const target = bodies[targetIndex];
      const dx = target.x - source.x;
      const dy = target.y - source.y;
      const distance = Math.max(1, Math.hypot(dx, dy));
      const spring = (distance - 210) * 0.018;
      forces[sourceIndex].x += (dx / distance) * spring;
      forces[sourceIndex].y += (dy / distance) * spring;
      forces[targetIndex].x -= (dx / distance) * spring;
      forces[targetIndex].y -= (dy / distance) * spring;
    }

    for (let index = 0; index < bodies.length; index += 1) {
      const body = bodies[index];
      forces[index].x += (centerX - body.x) * 0.006;
      forces[index].y += (centerY - body.y) * 0.006;
      body.vx = clamp((body.vx + forces[index].x * alpha) * 0.78, -18, 18);
      body.vy = clamp((body.vy + forces[index].y * alpha) * 0.78, -18, 18);
      body.x = clamp(body.x + body.vx, PADDING, maxX);
      body.y = clamp(body.y + body.vy, PADDING, maxY);
    }
  }

  separateCollisions(bodies, width, height);
  return {
    width,
    height,
    nodes: bodies.map(({ node, x, y }) => ({ ...node, x: Math.round(x * 1000) / 1000, y: Math.round(y * 1000) / 1000 })),
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