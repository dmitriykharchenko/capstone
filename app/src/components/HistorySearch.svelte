<script lang="ts">
  import { store } from "../store";
  import {
    selectSearchMatches,
    selectSearching,
    selectSearchEnabled,
    selectSearchReason,
    selectSearchError,
    selectHasSearched,
  } from "../slices/search/search-selectors";
  import { runSearch } from "../slices/search/search-slice";
  import { selectActiveRoute } from "../slices/router/router-selectors";
  import { navigateTo } from "../slices/router/router-slice";
  import type { SearchMatch } from "../lib/api";
  import LoaderCircle from "@lucide/svelte/icons/loader-circle";
  import { Button } from "$lib/components/ui/button";
  import { Input } from "$lib/components/ui/input";
  import { Card, CardContent } from "$lib/components/ui/card";
  import { ScrollArea } from "$lib/components/ui/scroll-area";

  const matches = selectSearchMatches();
  const searching = selectSearching();
  const enabled = selectSearchEnabled();
  const reason = selectSearchReason();
  const error = selectSearchError();
  const hasSearched = selectHasSearched();
  const activeRoute = selectActiveRoute();

  let query = $state("");

  $effect(() => {
    query = $activeRoute.name === "history" ? ($activeRoute.query ?? "") : "";
  });

  function submit(event: Event) {
    event.preventDefault();
    if (!query.trim() || $searching) {
      return;
    }
    store.dispatch(runSearch(query));
  }

  function clearResults() {
    store.dispatch(runSearch(""));
  }

  function openMatch(match: SearchMatch) {
    store.dispatch(
      navigateTo({
        name: "chat",
        sessionId: match.session_id,
        exchangeId: String(match.exchange_id),
      }),
    );
  }

  function openMatchFromKeyboard(event: KeyboardEvent, match: SearchMatch) {
    if (event.key === "Enter" || event.key === " ") {
      event.preventDefault();
      openMatch(match);
    }
  }
</script>

<div class="flex h-full flex-col gap-3 bg-transparent px-4 backdrop-blur-md">
  <form class="flex items-center gap-2 bg-transparent backdrop-blur-md" onsubmit={submit}>
    <Input
      class="flex-1"
      placeholder="Search chat history…"
      bind:value={query}
      disabled={$searching}
    />
    <Button type="submit" disabled={$searching || !query.trim()}>
      {#if $searching}
        <LoaderCircle class="size-4 animate-spin" />
        Searching…
      {:else}
        Search
      {/if}
    </Button>
    {#if $activeRoute.name === "history" && $activeRoute.query}
      <Button type="button" variant="ghost" onclick={clearResults}>Clear</Button>
    {/if}
  </form>

  {#if $error}
    <p class="text-sm text-destructive">{$error}</p>
  {/if}

  {#if $hasSearched && !$enabled}
    <p class="text-sm text-muted-foreground">
      Semantic search is unavailable{$reason ? `: ${$reason}` : ""}.
    </p>
  {/if}

  <ScrollArea class="min-h-0 flex-1">
    <div class="flex flex-col gap-3">
      {#if $searching}
        <div
          class="flex items-center gap-2 text-sm text-muted-foreground"
          aria-live="polite"
        >
          <LoaderCircle class="size-4 animate-spin" />
          <span>Searching chat history…</span>
        </div>
      {/if}
      {#each $matches as match}
        <Card
          class="cursor-pointer focus-visible:ring-2 focus-visible:ring-ring"
          role="link"
          tabindex={0}
          onclick={() => openMatch(match)}
          onkeydown={(event) => openMatchFromKeyboard(event, match)}
        >
          <CardContent>
            <div class="font-mono text-xs uppercase text-muted-foreground">
              session {match.session_id} · exchange {match.exchange_id}
            </div>
            <p class="mt-1.5 mb-1 font-medium">{match.user_message}</p>
            <p class="m-0 whitespace-pre-wrap text-muted-foreground">
              {match.assistant_message}
            </p>
          </CardContent>
        </Card>
      {/each}
      {#if $hasSearched && $enabled && $matches.length === 0 && !$searching}
        <p class="text-sm text-muted-foreground">No matching exchanges.</p>
      {/if}
    </div>
  </ScrollArea>
</div>
