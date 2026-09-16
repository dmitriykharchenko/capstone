<script lang="ts">
  import { connectedNodes, layoutGraph } from "../lib/graph-layout";
  import type { GraphNodeKind } from "../lib/graph-types";
  import {
    selectGraphEdges,
    selectGraphMessage,
    selectGraphNodeId,
    selectGraphNodes,
    selectGraphStatus,
  } from "../slices/graph-explorer/graph-explorer-selectors";
  import { loadGraph, selectGraphNode } from "../slices/graph-explorer/graph-explorer-slice";
  import { store } from "../store";
  import { Button } from "$lib/components/ui/button";
  import { Card } from "$lib/components/ui/card";
  import LoaderCircle from "@lucide/svelte/icons/loader-circle";

  const status = selectGraphStatus();
  const nodes = selectGraphNodes();
  const edges = selectGraphEdges();
  const selectedId = selectGraphNodeId();
  const message = selectGraphMessage();
  let graphOpen = $state(true);

  const layout = $derived(layoutGraph($nodes));
  const positions = $derived(new Map(layout.nodes.map((node) => [node.id, node])));
  const selected = $derived($nodes.find((node) => node.id === $selectedId) ?? null);
  const connected = $derived(connectedNodes($selectedId, $nodes, $edges));

  const nodeClasses: Record<GraphNodeKind, string> = {
    paper: "border-blue-500/60 bg-blue-500/15",
    author: "border-amber-500/60 bg-amber-500/15",
    topic: "border-emerald-500/60 bg-emerald-500/15",
  };

  function retry() {
    store.dispatch(loadGraph());
  }

  function select(nodeId: string) {
    store.dispatch(selectGraphNode(nodeId));
  }
</script>

{#if graphOpen}
<Card
  class="absolute top-16 right-4 bottom-4 z-20 flex w-[420px] max-w-[calc(100%-2rem)] flex-col overflow-hidden bg-background/90 shadow-lg backdrop-blur-md"
>
  <header class="flex items-center justify-between border-b px-4 py-3">
    <div>
      <h2 id="paper-graph-title" class="font-semibold">Paper graph</h2>
      <p class="text-xs text-muted-foreground">Authors, papers, and topics</p>
    </div>
    <div class="flex gap-1">
      <Button type="button" variant="outline" size="sm" onclick={retry} disabled={$status === "loading"}>
        Retry
      </Button>
      <Button type="button" variant="ghost" size="sm" onclick={() => (graphOpen = false)}>
        Hide graph
      </Button>
    </div>
  </header>

  <section aria-labelledby="paper-graph-title" class="min-h-0 flex-1 overflow-auto">
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
      <div class="relative" style={`width: ${layout.width}px; height: ${layout.height}px`}>
        <svg
          class="absolute inset-0"
          width={layout.width}
          height={layout.height}
          viewBox={`0 0 ${layout.width} ${layout.height}`}
          aria-hidden="true"
        >
          {#each $edges as edge (`${edge.kind}:${edge.sourceId}:${edge.targetId}`)}
            {@const source = positions.get(edge.sourceId)}
            {@const target = positions.get(edge.targetId)}
            {#if source && target}
              <line
                x1={source.x}
                y1={source.y}
                x2={target.x}
                y2={target.y}
                class="stroke-border"
                stroke-width="2"
              />
              <text
                x={(source.x + target.x) / 2}
                y={(source.y + target.y) / 2 - 4}
                class="fill-muted-foreground text-[9px]"
                text-anchor="middle"
              >
                {edge.kind === "authored_by" ? "authored by" : "has topic"}
              </text>
            {/if}
          {/each}
        </svg>
        {#each layout.nodes as node (node.id)}
          <button
            type="button"
            class={`absolute w-40 -translate-x-1/2 -translate-y-1/2 truncate rounded-md border px-2 py-1.5 text-xs shadow-sm ${nodeClasses[node.kind]}`}
            class:ring-2={$selectedId === node.id}
            style={`left: ${node.x}px; top: ${node.y}px`}
            aria-pressed={$selectedId === node.id}
            aria-label={`${node.kind}: ${node.label}`}
            title={node.label}
            onclick={() => select(node.id)}
          >
            {node.label}
          </button>
        {/each}
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
                <Button type="button" variant="secondary" size="sm" onclick={() => select(node.id)}>
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
{:else}
  <Button
    type="button"
    variant="outline"
    class="absolute top-16 right-4 z-20 bg-background/90 backdrop-blur-md"
    onclick={() => (graphOpen = true)}
    aria-controls="paper-graph-title"
  >
    Show graph
  </Button>
{/if}