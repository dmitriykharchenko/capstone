<script lang="ts">
  import { Controls, SvelteFlow } from "@xyflow/svelte";
  import GraphNode from "../../src/components/GraphNode.svelte";
  import { reconcileFlowNodes, toFlowEdges, type GraphFlowNode } from "../../src/lib/graph-flow";
  import type { GraphEdge, GraphNode as InputGraphNode } from "../../src/lib/graph-types";

  const nodeTypes = { graph: GraphNode };
  const graphNodes: InputGraphNode[] = [
    { id: "author-a", kind: "author", label: "Ada" },
    { id: "paper-a", kind: "paper", label: "Paper A" },
    { id: "topic-a", kind: "topic", label: "Retrieval" },
  ];
  const graphEdges: GraphEdge[] = [
    { sourceId: "paper-a", targetId: "author-a", kind: "authored_by" },
    { sourceId: "paper-a", targetId: "topic-a", kind: "has_topic" },
  ];
  const initial = reconcileFlowNodes(null, [], graphNodes, graphEdges, () => {});
  let layoutKey = initial.layoutKey;
  let nodes = $state.raw<GraphFlowNode[]>(initial.nodes);
  let edges = $state.raw(toFlowEdges(graphEdges));

  function moveTopic() {
    nodes = nodes.map((node) =>
      node.id === "topic-a"
        ? { ...node, position: { x: node.position.x + 120, y: node.position.y + 80 } }
        : node,
    );
  }

  function refreshUnchanged() {
    const next = reconcileFlowNodes(layoutKey, nodes, [...graphNodes].reverse(), [...graphEdges].reverse(), () => {});
    layoutKey = next.layoutKey;
    nodes = next.nodes;
  }

  function filterGraph() {
    const filteredNodes = graphNodes.filter((node) => node.id !== "author-a");
    const filteredEdges = graphEdges.filter((edge) => edge.kind === "has_topic");
    const next = reconcileFlowNodes(layoutKey, nodes, filteredNodes, filteredEdges, () => {});
    layoutKey = next.layoutKey;
    nodes = next.nodes;
    edges = toFlowEdges(filteredEdges);
  }
</script>

<button id="move-topic" type="button" onclick={moveTopic}>Move topic</button>
<button id="refresh-unchanged" type="button" onclick={refreshUnchanged}>Refresh unchanged graph</button>
<button id="filter-graph" type="button" onclick={filterGraph}>Filter graph</button>
<div style="width: 900px; height: 500px">
  <SvelteFlow bind:nodes {edges} {nodeTypes} nodesConnectable={false} nodesFocusable={false}>
    <Controls />
  </SvelteFlow>
</div>