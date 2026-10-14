<script lang="ts">
  import { store } from "../store";
  import {
    selectPapers,
    selectPapersLoading,
    selectPapersError,
    selectSelectedPaperId,
    selectPaperIndexing,
    selectPaperIndexedCount,
    selectPaperIndexError,
    selectPaperIndexCompleted,
    selectPaperIndexTotal,
    selectPaperIndexCurrentFile,
    selectPaperIndexCurrentStatus,
    selectPaperIndexCurrentWarning,
    selectPaperIndexFailedCount,
    selectPaperIndexEmptyCount,
    selectPaperIndexSkippedCount,
    selectPaperIndexStatuses,
    selectPaperIndexWarnings,
    selectBm25PaperIds,
    selectPapersResetting,
  } from "../slices/papers/papers-selectors";
  import type { PaperIndexStatus } from "../slices/papers/papers-types";
  import {
    openPaper,
    resetIndices,
    runPaperIndex,
  } from "../slices/papers/papers-slice";
  import {
    selectPaperSearchMatches,
    selectPaperSearching,
    selectPaperSearchEnabled,
    selectPaperSearchReason,
    selectPaperSearchError,
    selectPaperHasSearched,
  } from "../slices/paper-search/paper-search-selectors";
  import { runPaperSearch } from "../slices/paper-search/paper-search-slice";
  import { selectActiveRoute } from "../slices/router/router-selectors";
  import { filterPaperList, paperListEntries } from "../lib/paper-listing";
  import PdfViewer from "./PdfViewer.svelte";
  import LibraryChooser from "./LibraryChooser.svelte";
  import LoaderCircle from "@lucide/svelte/icons/loader-circle";
  import PanelLeftClose from "@lucide/svelte/icons/panel-left-close";
  import PanelLeftOpen from "@lucide/svelte/icons/panel-left-open";
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
  const indexCompleted = selectPaperIndexCompleted();
  const indexTotal = selectPaperIndexTotal();
  const indexCurrentFile = selectPaperIndexCurrentFile();
  const indexCurrentStatus = selectPaperIndexCurrentStatus();
  const indexCurrentWarning = selectPaperIndexCurrentWarning();
  const indexFailedCount = selectPaperIndexFailedCount();
  const indexEmptyCount = selectPaperIndexEmptyCount();
  const indexSkippedCount = selectPaperIndexSkippedCount();
  const indexStatuses = selectPaperIndexStatuses();
  const indexWarnings = selectPaperIndexWarnings();
  const bm25PaperIds = selectBm25PaperIds();
  const resetting = selectPapersResetting();
  const activeRoute = selectActiveRoute();

  // A membership set for O(1) per-paper BM25 lookups while rendering the list.
  const bm25Set = $derived(new Set($bm25PaperIds));

  let query = $state("");
  let paperFilter = $state("");
  let sidebarOpen = $state(true);

  const paperEntries = $derived(paperListEntries($papers));
  const filteredPapers = $derived(filterPaperList(paperEntries, paperFilter));

  $effect(() => {
    query = $activeRoute.name === "papers" ? ($activeRoute.query ?? "") : "";
  });

  // Determinate progress percentage while indexing (0 when total unknown).
  const indexPercent = $derived(
    $indexTotal > 0
      ? Math.min(
          100,
          Math.max(0, Math.round(($indexCompleted / $indexTotal) * 100)),
        )
      : 0,
  );

  // Resolve the indicator to show for one paper. Papers without a per-paper
  // entry read as "queued" while a run is active and "not-indexed" otherwise,
  // so an in-flight run visibly forms a queue that drains one paper at a time.
  type PaperIndicator = PaperIndexStatus | "not-indexed";
  function paperStatus(id: string): PaperIndicator {
    const status = $indexStatuses[id];
    if (status) {
      return status;
    }
    return $indexing ? "queued" : "not-indexed";
  }

  const INDICATOR_LABELS: Record<PaperIndicator, string> = {
    "not-indexed": "Not indexed",
    queued: "Queued",
    indexing: "Indexing…",
    indexed: "Indexed",
    empty: "No text",
    failed: "Failed",
    skipped: "Already indexed",
  };

  // A single colored dot conveys the status; the label is exposed only as a
  // tooltip (native title) and accessible aria-label, never inline text.
  const INDICATOR_DOT_CLASSES: Record<PaperIndicator, string> = {
    "not-indexed": "bg-muted-foreground/40",
    queued: "bg-yellow-500",
    indexing: "bg-blue-500",
    indexed: "bg-green-500",
    empty: "bg-muted-foreground/40",
    failed: "bg-red-500",
    skipped: "bg-green-500",
  };

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
    store.dispatch(runPaperSearch(""));
  }

  function indexPapers() {
    if ($indexing) {
      return;
    }
    store.dispatch(runPaperIndex());
  }

  function startReset() {
    if ($indexing || $resetting) {
      return;
    }
    store.dispatch(resetIndices());
  }

  function toggleSidebar() {
    sidebarOpen = !sidebarOpen;
  }

  $effect(() => {
    // The PDF renders inside an <iframe>, so clicks on its content never bubble
    // to this document. When focus moves into that iframe the window blurs and
    // the iframe becomes the active element, letting us collapse the sidebar as
    // soon as the user clicks the PDF content.
    function handleWindowBlur() {
      if (document.activeElement instanceof HTMLIFrameElement) {
        sidebarOpen = false;
      }
    }
    window.addEventListener("blur", handleWindowBlur);
    return () => window.removeEventListener("blur", handleWindowBlur);
  });
</script>

<div class="relative flex h-full min-h-0 flex-col gap-3">
  <div class="absolute inset-x-0 top-0 z-30 flex items-center gap-3 bg-transparent px-4 py-3 backdrop-blur-md">
    <Button
      type="button"
      variant="ghost"
      size="icon-lg"
      onclick={toggleSidebar}
      aria-controls="papers-sidebar"
      aria-expanded={sidebarOpen}
      aria-label={sidebarOpen ? "Hide papers sidebar" : "Show papers sidebar"}
      title={sidebarOpen ? "Hide papers sidebar" : "Show papers sidebar"}
    >
      {#if sidebarOpen}
        <PanelLeftClose class="size-4" />
      {:else}
        <PanelLeftOpen class="size-4" />
      {/if}
    </Button>
    <LibraryChooser />
    <form class="flex min-w-0 flex-1 items-center gap-2" onsubmit={submitSearch}>
      <Input
        class="flex-1"
        placeholder="Search papers…"
        bind:value={query}
        disabled={$searching}
      />
      <Button type="submit" size="lg" disabled={$searching || !query.trim()}>
        {#if $searching}
          <LoaderCircle class="size-4 animate-spin" />
          Searching…
        {:else}
          Search
        {/if}
      </Button>
      {#if $hasSearched}
        <Button type="button" variant="ghost" size="lg" onclick={clearResults}>
          Clear
        </Button>
      {/if}
    </form>
    <Button
      type="button"
      variant="outline"
      size="lg"
      onclick={indexPapers}
      disabled={$indexing || $resetting}
    >
      {$indexing ? "Indexing…" : "Index papers"}
    </Button>
    <Button
      type="button"
      variant="outline"
      size="lg"
      onclick={startReset}
      disabled={$indexing || $resetting}
      title="Delete the vector, BM25, and per-paper status indexes"
    >
      {$resetting ? "Resetting…" : "Reset indices"}
    </Button>
  </div>

  <div class="absolute inset-0 min-h-0">
  {#if sidebarOpen}
  <aside
    id="papers-sidebar"
    class="absolute top-16 bottom-4 left-4 z-20 flex w-[320px] min-h-0 flex-col overflow-hidden rounded-xl bg-transparent shadow-lg shadow-black/5 backdrop-blur-md"
  >
    <div
      class="flex flex-col gap-1 bg-transparent px-4 py-1.5 text-xs backdrop-blur-md"
      aria-live="polite"
      aria-atomic="true"
    >
      <div
        class="flex items-center justify-between gap-2 text-muted-foreground"
      >
        <span>
          {#if $indexing}
            {#if $indexTotal > 0}
              Indexing {$indexCompleted}/{$indexTotal} ({indexPercent}%)…
            {:else}
              Indexing papers…
            {/if}
          {:else if $indexError}
            <span class="text-destructive">Indexing failed: {$indexError}</span>
          {:else if $indexedCount !== null}
            <span class={$indexFailedCount > 0 ? "text-destructive" : ""}>
              Indexing finished: {$indexedCount} indexed, {$indexSkippedCount} skipped,
              {$indexEmptyCount} empty, {$indexFailedCount} failed.
            </span>
          {/if}
        </span>
      </div>
      {#if $indexing}
        <div
          class="h-1 w-full overflow-hidden rounded-full bg-muted"
          role="progressbar"
          aria-label="Paper indexing progress"
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={indexPercent}
          aria-valuetext={$indexTotal > 0
            ? `${$indexCompleted} of ${$indexTotal} papers (${indexPercent}%)`
            : "Preparing paper index (0%)"}
        >
          <div
            class="h-full bg-primary transition-all"
            style={`width: ${$indexTotal > 0 ? indexPercent : 0}%`}
          ></div>
        </div>
        {#if $indexCurrentFile}
          <span class="truncate text-muted-foreground">
            {$indexCurrentFile} — {$indexCurrentStatus ?? "processing"}
          </span>
          {#if $indexCurrentWarning}
            <span class="text-amber-600 dark:text-amber-400">
              {$indexCurrentWarning}
            </span>
          {/if}
        {/if}
      {/if}
    </div>

    <div class="px-4 pb-2">
      <label class="sr-only" for="paper-list-filter">Filter papers by title, author, or topic</label>
      <Input
        id="paper-list-filter"
        type="search"
        placeholder="Filter papers…"
        bind:value={paperFilter}
        aria-label="Filter papers by title, author, or topic"
      />
    </div>

    <ScrollArea class="min-h-0 flex-1">
      <div class="flex flex-col gap-1 p-4">
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
        {#if $searching}
          <div
            class="flex items-center gap-2 p-2 text-sm text-muted-foreground"
            aria-live="polite"
          >
            <LoaderCircle class="size-4 animate-spin" />
            <span>Searching papers…</span>
          </div>
        {/if}

        {#if $hasSearched && $searchEnabled && !paperFilter.trim()}
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
          {#each filteredPapers as entry (entry.paper.id)}
            {@const paper = entry.paper}
            {@const status = paperStatus(paper.id)}
            {@const inBm25 = bm25Set.has(paper.id)}
            <Button
              variant={$selectedId === paper.id ? "secondary" : "ghost"}
              class="h-auto w-full flex-col items-start gap-0.5 whitespace-normal px-3 py-2 text-left"
              onclick={() => open(paper.id)}
            >
              <span class="flex w-full items-center gap-1.5">
                <span class="min-w-0 flex-1 font-medium">{paper.title ?? paper.filename}</span>
                <span
                  class={`size-1.5 shrink-0 rounded-full ${INDICATOR_DOT_CLASSES[status]}`}
                  class:animate-pulse={status === "indexing"}
                  role="img"
                  title={`Vector index: ${INDICATOR_LABELS[status]}`}
                  aria-label={`Vector index status: ${INDICATOR_LABELS[status]}`}
                ></span>
                {#if status === "indexed" && $indexWarnings[paper.id]}
                  <span
                    class="size-1.5 shrink-0 rounded-full bg-amber-500"
                    role="img"
                    title={`Indexing warning: ${$indexWarnings[paper.id]}`}
                    aria-label={`Indexing warning: ${$indexWarnings[paper.id]}`}
                  ></span>
                {/if}
                <span
                  class={`size-1.5 shrink-0 rounded-full ${inBm25 ? "bg-violet-500" : "bg-muted-foreground/40"}`}
                  role="img"
                  title={inBm25 ? "In BM25 index" : "Not in BM25 index"}
                  aria-label={`BM25 index: ${inBm25 ? "indexed" : "not indexed"}`}
                ></span>
              </span>
              <span class="text-xs text-muted-foreground">
                {entry.authors.length > 0
                  ? entry.authors.join(", ")
                  : paper.metadata_status === "missing"
                    ? "Authors not indexed"
                    : "Authors unavailable"}
              </span>
              <span class="text-[11px] font-light text-muted-foreground">
                {entry.topics.length > 0
                  ? entry.topics.join(" · ")
                  : paper.metadata_status === "missing"
                    ? "Topics not indexed"
                    : "Topics unavailable"}
              </span>
            </Button>
          {/each}
          {#if !$loading && filteredPapers.length === 0}
            <p class="p-2 text-sm text-muted-foreground">
              {paperFilter.trim() ? "No papers match the filter." : "No papers found."}
            </p>
          {/if}
        {/if}
      </div>
    </ScrollArea>
  </aside>
  {/if}

  <div class="absolute inset-0 min-h-0">
    <PdfViewer />
  </div>
  </div>
</div>
