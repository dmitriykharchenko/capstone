<script lang="ts">
  import { Controls, SvelteFlow } from "@xyflow/svelte";
  import GraphNode from "../../src/components/GraphNode.svelte";
  import { filterGraphByQuery, toFlowEdges, toFlowNodes } from "../../src/lib/graph-flow";
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
  let query = $state("");
  let selectedId = $state<string | null>(null);
  const visibleGraph = $derived(filterGraphByQuery(allGraphNodes, allGraphEdges, query));
  const nodes = $derived(toFlowNodes(visibleGraph.nodes, visibleGraph.edges, (node) => { selectedId = node.id; })
    .map((node) => ({ ...node, selected: node.id === selectedId })));
  const edges = $derived(toFlowEdges(visibleGraph.edges));

  function filterGraph() {
    query = "Retrieval";
    selectedId = null;
  }
</script>

<label for="graph-search">Search graph nodes</label>
<input id="graph-search" type="search" bind:value={query} aria-label="Search graph nodes" />
{#if query.trim() && visibleGraph.nodes.length === 0}
  <p role="status">No graph nodes match “{query.trim()}”.</p>
{/if}
<button id="filter-graph" type="button" onclick={filterGraph}>Filter graph</button>
<output id="selected-node">{selectedId ?? "none"}</output>
<div style="width: 900px; height: 500px">
  <SvelteFlow {nodes} {edges} {nodeTypes} nodesConnectable={false} nodesDraggable={false} nodesFocusable={false}>
    <Controls />
  </SvelteFlow>
</div>