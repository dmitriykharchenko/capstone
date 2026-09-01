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

  const dispose = store.init();
  onDestroy(dispose);

  let theme = $state<Theme>("light");

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

  let tab = $state("chat");

  const tabs: { id: string; label: string }[] = [
    { id: "chat", label: "Chat" },
    { id: "search", label: "History Search" },
    { id: "papers", label: "Papers" },
  ];
</script>

<div class="flex h-screen flex-col bg-background text-foreground">
  <Tabs bind:value={tab} class="flex min-h-0 flex-1 flex-col gap-0">
    <div
      data-tauri-drag-region
      class="flex items-center justify-between gap-4 border-b py-1.5 pr-3 pl-20"
    >
      <TabsList variant="line">
        {#each tabs as t}
          <TabsTrigger value={t.id} class="font-mono">{t.label}</TabsTrigger>
        {/each}
      </TabsList>
      <Button
        variant="ghost"
        size="icon"
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

    <TabsContent value="chat" class="min-h-0 overflow-hidden p-5">
      <Chat />
    </TabsContent>
    <TabsContent value="search" class="min-h-0 overflow-hidden p-5">
      <HistorySearch />
    </TabsContent>
    <TabsContent value="papers" class="min-h-0 overflow-hidden p-5">
      <PapersList />
    </TabsContent>
  </Tabs>
</div>
