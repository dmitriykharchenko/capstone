<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import { store, appSagas } from "./store";
  import { loadSessions } from "./slices/sessions/sessions-slice";
  import { loadPapers } from "./slices/papers/papers-slice";
  import Chat from "./components/Chat.svelte";
  import HistorySearch from "./components/HistorySearch.svelte";
  import PapersList from "./components/PapersList.svelte";
  import { Tabs, TabsContent, TabsList, TabsTrigger } from "$lib/components/ui/tabs";
  import { Button } from "$lib/components/ui/button";
  import SunIcon from "@lucide/svelte/icons/sun";
  import MoonIcon from "@lucide/svelte/icons/moon";
  import { applyTheme, resolveTheme, type Theme } from "$lib/theme";
  import { selectActiveRoute } from "./slices/router/router-selectors";
  import { navigateTo } from "./slices/router/router-slice";
  import {
    routeForName,
    type RouteName,
  } from "./slices/router/router-types";

  const dispose = store.init();
  onDestroy(dispose);

  let theme = $state<Theme>("light");
  const activeRoute = selectActiveRoute();

  onMount(() => {
    theme = resolveTheme();
    applyTheme(theme);
    const cancels = appSagas.map((saga) => store.runSaga(saga));
    store.dispatch(loadSessions());
    store.dispatch(loadPapers());
    return () => cancels.forEach((cancel) => cancel());
  });

  function toggleTheme() {
    theme = theme === "dark" ? "light" : "dark";
    applyTheme(theme);
  }

  const tabs: { id: RouteName; label: string }[] = [
    { id: "chat", label: "Chat" },
    { id: "history", label: "History Search" },
    { id: "papers", label: "Papers" },
  ];

  function navigate(name: string) {
    const routeName = name as RouteName;
    if ($activeRoute.name !== routeName) {
      store.dispatch(navigateTo(routeForName(routeName)));
    }
  }
</script>

<div class="flex h-screen flex-col bg-background text-foreground">
  <Tabs
    value={$activeRoute.name}
    onValueChange={navigate}
    class="flex min-h-0 flex-1 flex-col gap-0"
  >
    <div
      data-tauri-drag-region
      class="flex h-[30px] items-center justify-between gap-4 border-b pr-3 pl-20"
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
        variant="ghost"
        size="icon-sm"
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

    {#if $activeRoute.name === "chat"}
      <TabsContent value="chat" class="flex min-h-0 flex-1 flex-col overflow-hidden p-5">
        <Chat />
      </TabsContent>
    {:else if $activeRoute.name === "history"}
      <TabsContent value="history" class="min-h-0 overflow-hidden p-5">
        <HistorySearch />
      </TabsContent>
    {:else}
      <TabsContent value="papers" class="min-h-0 overflow-hidden p-5">
        <PapersList />
      </TabsContent>
    {/if}
  </Tabs>
</div>
