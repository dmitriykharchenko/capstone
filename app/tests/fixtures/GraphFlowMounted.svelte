<script lang="ts">
  import { Controls, SvelteFlow } from "@xyflow/svelte";
  import GraphNode from "../../src/components/GraphNode.svelte";
  import { toFlowEdges, toFlowNodes } from "../../src/lib/graph-flow";

  const nodeTypes = { graph: GraphNode };
  let nodes = $state.raw(
    toFlowNodes(
      [
        { id: "author-a", kind: "author", label: "Ada" },
        { id: "paper-a", kind: "paper", label: "Paper A" },
        { id: "topic-a", kind: "topic", label: "Retrieval" },
      ],
      () => {},
    ),
  );
  const edges = toFlowEdges([
    { sourceId: "paper-a", targetId: "author-a", kind: "authored_by" },
    { sourceId: "paper-a", targetId: "topic-a", kind: "has_topic" },
  ]);

  function moveTopic() {
    nodes = nodes.map((node) =>
      node.id === "topic-a"
        ? { ...node, position: { x: node.position.x + 120, y: node.position.y + 80 } }
        : node,
    );
  }
</script>

<button id="move-topic" type="button" onclick={moveTopic}>Move topic</button>
<div style="width: 900px; height: 500px">
  <SvelteFlow bind:nodes {edges} {nodeTypes} nodesConnectable={false} nodesFocusable={false}>
    <Controls />
  </SvelteFlow>
</div>