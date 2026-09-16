<script lang="ts">
  import { Controls, SvelteFlow } from "@xyflow/svelte";
  import GraphNode from "../../src/components/GraphNode.svelte";
  import { toFlowEdges, toFlowNodes } from "../../src/lib/graph-flow";
  import type { GraphEdge, GraphNode as InputGraphNode } from "../../src/lib/graph-types";

  const nodeTypes = { graph: GraphNode };
  const allGraphNodes: InputGraphNode[] = [
    { id: "author-a", kind: "author", label: "Ada" },
    { id: "paper-a", kind: "paper", label: "Paper A" },
    { id: "topic-a", kind: "topic", label: "Retrieval" },
  ];
  const allGraphEdges: GraphEdge[] = [
    { sourceId: "paper-a", targetId: "author-a", kind: "authored_by" },
    { sourceId: "paper-a", targetId: "topic-a", kind: "has_topic" },
  ];
  let graphNodes = $state.raw(allGraphNodes);
  let graphEdges = $state.raw(allGraphEdges);
  let selectedId = $state<string | null>(null);
  const nodes = $derived(toFlowNodes(graphNodes, graphEdges, (node) => { selectedId = node.id; })
    .map((node) => ({ ...node, selected: node.id === selectedId })));
  const edges = $derived(toFlowEdges(graphEdges));

  function filterGraph() {
    graphNodes = allGraphNodes.filter((node) => node.id !== "author-a");
    graphEdges = allGraphEdges.filter((edge) => edge.kind === "has_topic");
    selectedId = null;
  }
</script>

<button id="filter-graph" type="button" onclick={filterGraph}>Filter graph</button>
<output id="selected-node">{selectedId ?? "none"}</output>
<div style="width: 900px; height: 500px">
  <SvelteFlow {nodes} {edges} {nodeTypes} nodesConnectable={false} nodesDraggable={false} nodesFocusable={false}>
    <Controls />
  </SvelteFlow>
</div>