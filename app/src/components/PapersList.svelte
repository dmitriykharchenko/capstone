<script lang="ts">
  import { store } from "../store";
  import {
    selectPapers,
    selectPapersLoading,
    selectPapersError,
    selectSelectedPaperId,
  } from "../slices/papers/papers-selectors";
  import { openPaper } from "../slices/papers/papers-slice";
  import PdfViewer from "./PdfViewer.svelte";
  import { Button } from "$lib/components/ui/button";
  import { Card } from "$lib/components/ui/card";
  import { ScrollArea } from "$lib/components/ui/scroll-area";

  const papers = selectPapers();
  const loading = selectPapersLoading();
  const error = selectPapersError();
  const selectedId = selectSelectedPaperId();

  function formatSize(bytes: number): string {
    if (bytes < 1024) return `${bytes} B`;
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`;
    return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  }

  function open(id: string) {
    store.dispatch(openPaper(id));
  }
</script>

<div class="grid h-full grid-cols-[320px_1fr] gap-4">
  <Card class="min-h-0 gap-0 overflow-hidden py-0">
    <ScrollArea class="h-full">
      <div class="flex flex-col gap-1 p-2">
        {#if $loading}
          <p class="p-2 text-sm text-muted-foreground">Loading papers…</p>
        {/if}
        {#if $error}
          <p class="p-2 text-sm text-destructive">{$error}</p>
        {/if}
        {#each $papers as paper}
          <Button
            variant={$selectedId === paper.id ? "secondary" : "ghost"}
            class="h-auto w-full flex-col items-start gap-0.5 whitespace-normal px-3 py-2 text-left"
            onclick={() => open(paper.id)}
          >
            <span class="font-medium">{paper.title}</span>
            <span class="text-xs text-muted-foreground">
              {paper.filename} · {formatSize(paper.size_bytes)}
            </span>
          </Button>
        {/each}
        {#if !$loading && $papers.length === 0}
          <p class="p-2 text-sm text-muted-foreground">No papers found.</p>
        {/if}
      </div>
    </ScrollArea>
  </Card>

  <div class="min-h-0">
    <PdfViewer />
  </div>
</div>
