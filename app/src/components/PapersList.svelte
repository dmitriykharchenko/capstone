<script lang="ts">
  import { store } from "../store";
  import {
    selectPapers,
    selectPapersLoading,
    selectPapersError,
    selectSelectedPaperId,
  } from "../slices/papers/papers-selectors";
  import { openPaper } from "../slices/papers/papers-slice";
  import {
    selectPaperSearchMatches,
    selectPaperSearching,
    selectPaperSearchEnabled,
    selectPaperSearchReason,
    selectPaperSearchError,
    selectPaperHasSearched,
    selectPaperIndexing,
    selectPaperIndexedCount,
    selectPaperIndexError,
  } from "../slices/paper-search/paper-search-selectors";
  import {
    runPaperSearch,
    clearPaperSearch,
    runPaperIndex,
  } from "../slices/paper-search/paper-search-slice";
  import PdfViewer from "./PdfViewer.svelte";
  import { Button } from "$lib/components/ui/button";
  import { Card } from "$lib/components/ui/card";
  import { Input } from "$lib/components/ui/input";
  import { ScrollArea } from "$lib/components/ui/scroll-area";

  const papers = selectPapers();
  const loading = selectPapersLoading();
  const error = selectPapersError();
  const selectedId = selectSelectedPaperId();

  const matches = selectPaperSearchMatches();
  const searching = selectPaperSearching();
  const searchEnabled = selectPaperSearchEnabled();
  const searchReason = selectPaperSearchReason();
  const searchError = selectPaperSearchError();
  const hasSearched = selectPaperHasSearched();
  const indexing = selectPaperIndexing();
  const indexedCount = selectPaperIndexedCount();
  const indexError = selectPaperIndexError();

  let query = $state("");

  function formatSize(bytes: number): string {
    if (bytes < 1024) return `${bytes} B`;
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`;
    return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  }

  function open(id: string) {
    store.dispatch(openPaper(id));
  }

  function submitSearch(event: Event) {
    event.preventDefault();
    if (!query.trim() || $searching) {
      return;
    }
    store.dispatch(runPaperSearch(query));
  }

  function clearResults() {
    query = "";
    store.dispatch(clearPaperSearch());
  }

  function indexPapers() {
    if ($indexing) {
      return;
    }
    store.dispatch(runPaperIndex());
  }
</script>

<div class="grid h-full grid-cols-[320px_1fr] gap-4">
  <Card class="min-h-0 gap-0 overflow-hidden py-0">
    <form class="flex gap-2 border-b p-2" onsubmit={submitSearch}>
      <Input
        class="flex-1"
        placeholder="Search papers…"
        bind:value={query}
        disabled={$searching}
      />
      <Button type="submit" size="sm" disabled={$searching || !query.trim()}>
        Search
      </Button>
      {#if $hasSearched}
        <Button type="button" variant="ghost" size="sm" onclick={clearResults}>
          Clear
        </Button>
      {/if}
    </form>

    <div
      class="flex items-center justify-between gap-2 border-b px-2 py-1.5 text-xs text-muted-foreground"
    >
      <span>
        {#if $indexing}
          Indexing papers…
        {:else if $indexError}
          <span class="text-destructive">Indexing failed: {$indexError}</span>
        {:else if $indexedCount !== null}
          Indexed {$indexedCount} paper{$indexedCount === 1 ? "" : "s"}.
        {:else}
          Index papers to enable semantic search.
        {/if}
      </span>
      <Button
        type="button"
        variant="outline"
        size="sm"
        onclick={indexPapers}
        disabled={$indexing}
      >
        {$indexing ? "Indexing…" : "Index"}
      </Button>
    </div>

    <ScrollArea class="h-full">
      <div class="flex flex-col gap-1 p-2">
        {#if $searchError}
          <p class="p-2 text-sm text-destructive">{$searchError}</p>
        {/if}
        {#if $hasSearched && !$searchEnabled}
          <p class="p-2 text-sm text-muted-foreground">
            Semantic search is unavailable{$searchReason
              ? `: ${$searchReason}`
              : ""}.
          </p>
        {/if}

        {#if $hasSearched && $searchEnabled}
          {#each $matches as match}
            <Button
              variant={$selectedId === match.paper_id ? "secondary" : "ghost"}
              class="h-auto w-full flex-col items-start gap-0.5 whitespace-normal px-3 py-2 text-left"
              onclick={() => open(match.paper_id)}
            >
              <span class="font-medium">{match.title}</span>
              <span class="text-xs text-muted-foreground">
                {match.filename}{match.page ? ` · p.${match.page}` : ""} · score {match.score.toFixed(
                  3,
                )}
              </span>
              {#if match.snippet}
                <span class="mt-1 line-clamp-3 text-xs text-muted-foreground">
                  {match.snippet}
                </span>
              {/if}
            </Button>
          {/each}
          {#if !$searching && $matches.length === 0}
            <p class="p-2 text-sm text-muted-foreground">No matching papers.</p>
          {/if}
        {:else}
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
        {/if}
      </div>
    </ScrollArea>
  </Card>

  <div class="min-h-0">
    <PdfViewer />
  </div>
</div>
