<script lang="ts">
  import type { GraphFlowNodeData } from "$lib/graph-flow";

  let { data, selected = false }: { data: GraphFlowNodeData; selected?: boolean } = $props();

  const kindClasses = {
    paper: "border-blue-500/70 bg-blue-500/15 text-blue-950 dark:text-blue-100",
    author: "border-amber-500/70 bg-amber-500/15 text-amber-950 dark:text-amber-100",
    topic: "border-emerald-500/70 bg-emerald-500/15 text-emerald-950 dark:text-emerald-100",
  } as const;
</script>

<button
  type="button"
  class={`nodrag nopan flex w-44 flex-col items-start gap-1 rounded-md border px-3 py-2 text-left text-xs shadow-sm transition-colors hover:brightness-95 ${kindClasses[data.kind]}`}
  class:ring-2={selected}
  class:ring-primary={selected}
  aria-pressed={selected}
  aria-label={`${data.kind}: ${data.label}`}
  title={data.label}
  onclick={(event) => {
    event.stopPropagation();
    data.onActivate();
  }}
>
  <span class="font-mono text-[10px] font-semibold uppercase tracking-wide opacity-70">{data.kind}</span>
  <span class="w-full truncate font-medium">{data.label}</span>
</button>