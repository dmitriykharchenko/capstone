<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import { store, appSagas } from "./store";
  import { loadSessions } from "./slices/sessions/sessions-slice";
  import { loadPapers } from "./slices/papers/papers-slice";
  import Chat from "./components/Chat.svelte";
  import HistorySearch from "./components/HistorySearch.svelte";
  import PapersList from "./components/PapersList.svelte";
  import { Tabs, TabsContent, TabsList, TabsTrigger } from "$lib/components/ui/tabs";

  const dispose = store.init();
  onDestroy(dispose);

  onMount(() => {
    const cancels = appSagas.map((saga) => store.runSaga(saga));
    store.dispatch(loadSessions());
    store.dispatch(loadPapers());
    return () => cancels.forEach((cancel) => cancel());
  });

  let tab = $state("chat");

  const tabs: { id: string; label: string }[] = [
    { id: "chat", label: "Chat" },
    { id: "search", label: "History Search" },
    { id: "papers", label: "Papers" },
  ];
</script>

<div class="flex h-screen flex-col bg-background text-foreground">
  <header class="border-b px-5 py-3">
    <h1 class="text-base font-semibold tracking-tight">Capstone Research Assistant</h1>
  </header>

  <Tabs bind:value={tab} class="flex min-h-0 flex-1 flex-col gap-0">
    <div class="border-b px-5 py-2">
      <TabsList>
        {#each tabs as t}
          <TabsTrigger value={t.id}>{t.label}</TabsTrigger>
        {/each}
      </TabsList>
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
