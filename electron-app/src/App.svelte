<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import { store, appSagas } from "./store";
  import { loadSessions } from "./slices/sessions/sessions-slice";
  import { loadPapers } from "./slices/papers/papers-slice";
  import Chat from "./components/Chat.svelte";
  import HistorySearch from "./components/HistorySearch.svelte";
  import PapersList from "./components/PapersList.svelte";
  import GraphExplorer from "./components/GraphExplorer.svelte";
  import RagEvals from "./components/RagEvals.svelte";
  import { Tabs, TabsContent, TabsList, TabsTrigger } from "$lib/components/ui/tabs";
  import { Button } from "$lib/components/ui/button";
  import SunIcon from "@lucide/svelte/icons/sun";
  import MoonIcon from "@lucide/svelte/icons/moon";
  import { applyTheme, resolveTheme, type Theme } from "$lib/theme";
  import { selectActiveRoute } from "./slices/router/router-selectors";
  import { navigateTo } from "./slices/router/router-slice";
  import { api } from "./lib/api";
  import {
    routeForName,
    type RouteName,
  } from "./slices/router/router-types";

  const dispose = store.init();
  onDestroy(dispose);

  let theme = $state<Theme>("light");
  let availability = $state("");
  const activeRoute = selectActiveRoute();

  onMount(() => {
    theme = resolveTheme();
    applyTheme(theme);
    const cancels = appSagas.map((saga) => store.runSaga(saga));
    store.dispatch(loadSessions());
    store.dispatch(loadPapers());
    const health = api.health();
    let mounted = true;
    health.then((info) => {
      if (mounted && info.commands.some((command) => !command.available)) {
        availability = "Rust backend connected. Features not yet integrated in this build are unavailable.";
      }
    }).catch(() => { if (mounted) availability = "Rust backend unavailable. Build the native backend and restart."; });
    return () => { mounted = false; health.cancel?.(); cancels.forEach((cancel) => cancel()); };
  });

  function toggleTheme() {
    theme = theme === "dark" ? "light" : "dark";
    applyTheme(theme);
  }

  const tabs: { id: RouteName; label: string }[] = [
    { id: "chat", label: "Chat" },
    { id: "history", label: "History Search" },
    { id: "papers", label: "Papers" },
    { id: "graph", label: "Graph" },
    { id: "evals", label: "RAG Evals" },
  ];

  function navigate(name: string) {
    const routeName = name as RouteName;
    if ($activeRoute.name !== routeName) {
      store.dispatch(navigateTo(routeForName(routeName)));
    }
  }
</script>

<div data-backend-notice={availability ? "" : undefined} class="flex h-screen flex-col bg-background text-foreground">
  <Tabs
    value={$activeRoute.name}
    onValueChange={navigate}
    class="relative flex min-h-0 flex-1 flex-col gap-0"
  >
    <div
      data-electron-drag-region
      class="absolute inset-x-0 top-0 z-20 flex h-[38px] items-center justify-between gap-4 bg-transparent pr-3 pl-20 backdrop-blur-md"
    >
      <TabsList
        variant="line"
        class="translate-x-[10px] translate-y-[3px] gap-4 group-data-horizontal/tabs:h-6"
      >
        {#each tabs as t}
          <TabsTrigger
            value={t.id}
            class="font-mono text-xs pb-1 data-active:font-semibold data-active:text-primary data-active:bg-primary/10 data-active:after:hidden"
            >{t.label}</TabsTrigger
          >
        {/each}
      </TabsList>
      <Button
        onclick={() => window.research.toggleMaximize()}
        variant="ghost"
        size="icon-lg"
        aria-label="Toggle maximize"
        title="Toggle maximize"
      >↗</Button>
      <Button
        variant="ghost"
        size="icon-lg"
        onclick={toggleTheme}
        aria-label="Toggle theme"
        title="Toggle theme"
      >
        {#if theme === "dark"}
          <SunIcon class="size-4" />
        {:else}
          <MoonIcon class="size-4" />
        {/if}
      </Button>
    </div>

    {#if availability}
      <p role="status" class="absolute top-[38px] inset-x-0 z-30 bg-background/95 px-5 text-center text-xs text-muted-foreground">{availability}</p>
    {/if}

    {#if $activeRoute.name === "chat"}
      <TabsContent value="chat" class="relative flex min-h-0 flex-1 flex-col overflow-hidden px-5 pt-[38px] pb-0">
        <Chat />
      </TabsContent>
    {:else if $activeRoute.name === "history"}
      <TabsContent value="history" class="relative flex min-h-0 flex-1 flex-col overflow-hidden px-5 pt-[38px] pb-0">
        <HistorySearch />
      </TabsContent>
    {:else if $activeRoute.name === "papers"}
      <TabsContent value="papers" class="relative flex min-h-0 flex-1 flex-col overflow-hidden px-0 pt-[38px] pb-0">
        <PapersList />
      </TabsContent>
    {:else if $activeRoute.name === "graph"}
      <TabsContent value="graph" class="relative flex min-h-0 flex-1 flex-col overflow-hidden px-0 pt-[38px] pb-0">
        <GraphExplorer />
      </TabsContent>
    {:else}
      <TabsContent value="evals" class="relative flex min-h-0 flex-1 flex-col overflow-hidden px-5 pt-[38px] pb-0">
        <RagEvals />
      </TabsContent>
    {/if}
  </Tabs>
</div>
