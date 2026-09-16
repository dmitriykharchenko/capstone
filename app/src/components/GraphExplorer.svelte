<script lang="ts">
  import { onMount, untrack } from "svelte";
  import {
    Background,
    BackgroundVariant,
    Controls,
    SvelteFlow,
    type NodeEventWithPointer,
  } from "@xyflow/svelte";
  import "@xyflow/svelte/dist/style.css";
  import { connectedNodes } from "../lib/graph-layout";
  import { reconcileFlowNodes, toFlowEdges, type GraphFlowNode } from "$lib/graph-flow";
  import {
    selectGraphEdges,
    selectGraphMessage,
    selectGraphNodeId,
    selectGraphNodes,
    selectGraphStatus,
  } from "../slices/graph-explorer/graph-explorer-selectors";
  import { loadGraph, selectGraphNode } from "../slices/graph-explorer/graph-explorer-slice";
  import { openPaper } from "../slices/papers/papers-slice";
  import { navigateTo } from "../slices/router/router-slice";
  import { routeForName } from "../slices/router/router-types";
  import { store } from "../store";
  import GraphNode from "./GraphNode.svelte";
  import { Button } from "$lib/components/ui/button";
  import { Card } from "$lib/components/ui/card";
  import LoaderCircle from "@lucide/svelte/icons/loader-circle";

  const status = selectGraphStatus();
  const nodes = selectGraphNodes();
  const edges = selectGraphEdges();
  const selectedId = selectGraphNodeId();
  const message = selectGraphMessage();

  let flowNodes = $state.raw<GraphFlowNode[]>([]);
  let flowLayoutKey = $state<string | null>(null);
  const flowEdges = $derived(toFlowEdges($edges));
  const selected = $derived($nodes.find((node) => node.id === $selectedId) ?? null);
  const connected = $derived(connectedNodes($selectedId, $nodes, $edges));
  const nodeTypes = { graph: GraphNode };
  let colorMode = $state<"light" | "dark">("light");

  $effect(() => {
    const next = reconcileFlowNodes(flowLayoutKey, untrack(() => flowNodes), $nodes, $edges, activateNode);
    if (next.layoutKey !== flowLayoutKey) {
      flowLayoutKey = next.layoutKey;
      flowNodes = next.nodes;
    }
  });

  onMount(() => {
    const syncTheme = () => {
      colorMode = document.documentElement.classList.contains("dark") ? "dark" : "light";
    };
    syncTheme();
    const observer = new MutationObserver(syncTheme);
    observer.observe(document.documentElement, { attributes: true, attributeFilter: ["class"] });
    return () => observer.disconnect();
  });

  function retry() {
    store.dispatch(loadGraph());
  }

  function activateNode(node: { id: string; kind: string }) {
    store.dispatch(selectGraphNode(node.id));
    if (node.kind === "paper") {
      store.dispatch(openPaper(node.id));
      store.dispatch(navigateTo(routeForName("papers")));
    }
  }

  function select(nodeId: string) {
    store.dispatch(selectGraphNode(nodeId));
  }

  const handleNodeClick: NodeEventWithPointer<MouseEvent | TouchEvent, GraphFlowNode> = ({ node }) => {
    activateNode({ id: node.id, kind: node.data.kind });
  };
</script>

<Card
  class="flex h-full min-h-0 flex-col overflow-hidden rounded-none border-0 bg-background shadow-none"
>
  <header class="flex items-center justify-between border-b px-4 py-3">
    <div>
      <h2 id="paper-graph-title" class="font-semibold">Paper graph</h2>
      <p class="text-xs text-muted-foreground">Authors, papers, and topics</p>
      <p class="text-[10px] text-muted-foreground" aria-label="Graph edge legend">
        Edges: authored by · has topic
      </p>
    </div>
    <Button type="button" variant="outline" size="sm" onclick={retry} disabled={$status === "loading"}>
      Retry
    </Button>
  </header>

  <section aria-labelledby="paper-graph-title" class="min-h-0 flex-1 overflow-hidden">
    {#if $status === "loading"}
      <div class="flex items-center gap-2 p-4 text-sm" role="status" aria-live="polite">
        <LoaderCircle class="size-4 animate-spin" /> Loading paper graph…
      </div>
    {:else if $status === "idle"}
      <p class="p-4 text-sm text-muted-foreground">Graph data is ready to load.</p>
    {:else if $status === "empty"}
      <p class="p-4 text-sm text-muted-foreground" role="status">No graph data is available for these papers.</p>
    {:else if $status === "dependency-unavailable"}
      <div class="p-4 text-sm" role="alert">
        <strong>Graph dependency unavailable.</strong> {$message}
      </div>
    {:else if $status === "store-unavailable"}
      <div class="p-4 text-sm" role="alert">
        <strong>Graph store unavailable.</strong> {$message}
      </div>
    {:else if $status === "malformed-payload"}
      <div class="p-4 text-sm" role="alert">
        <strong>Malformed graph data.</strong> The response could not be displayed safely.
      </div>
    {:else if $status === "transport-error"}
      <div class="p-4 text-sm" role="alert">
        <strong>Graph transport error.</strong> {$message}
      </div>
    {:else}
      <div class="h-full min-h-[260px] w-full" aria-label="Interactive paper graph">
        <SvelteFlow
          {nodeTypes}
          bind:nodes={flowNodes}
          edges={flowEdges}
          fitView
          fitViewOptions={{ padding: 0.2, minZoom: 0.35, maxZoom: 1.5 }}
          colorMode={colorMode}
          nodesConnectable={false}
          nodesFocusable={false}
          panOnDrag
          zoomOnScroll
          onnodeclick={handleNodeClick}
        >
          <Background variant={BackgroundVariant.Dots} gap={24} size={1} />
          <Controls />
        </SvelteFlow>
      </div>
    {/if}
  </section>

  {#if selected}
    <aside class="max-h-48 overflow-auto border-t p-4" aria-label="Selected node connections">
      <div class="flex items-start justify-between gap-2">
        <div>
          <p class="text-xs uppercase text-muted-foreground">Selected {selected.kind}</p>
          <h3 class="font-medium">{selected.label}</h3>
        </div>
        <Button type="button" variant="ghost" size="sm" onclick={() => store.dispatch(selectGraphNode(null))}>
          Clear
        </Button>
      </div>
      {#if connected.length === 0}
        <p class="mt-2 text-sm text-muted-foreground">No directly connected nodes.</p>
      {:else}
        {#each ["paper", "author", "topic"] as kind}
          {@const matches = connected.filter((node) => node.kind === kind)}
          {#if matches.length > 0}
            <h4 class="mt-3 text-xs font-semibold uppercase text-muted-foreground">Connected {kind}s</h4>
            <div class="mt-1 flex flex-wrap gap-1">
              {#each matches as node (node.id)}
                <Button
                  type="button"
                  variant="secondary"
                  size="sm"
                  aria-pressed={$selectedId === node.id}
                  onclick={() => select(node.id)}
                >
                  {node.label}
                </Button>
              {/each}
            </div>
          {/if}
        {/each}
      {/if}
    </aside>
  {/if}
</Card>