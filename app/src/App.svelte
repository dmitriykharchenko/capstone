<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import { store, appSagas } from "./store";
  import { loadSessions } from "./slices/sessions/sessions-slice";
  import { loadPapers } from "./slices/papers/papers-slice";
  import Chat from "./components/Chat.svelte";
  import HistorySearch from "./components/HistorySearch.svelte";
  import PapersList from "./components/PapersList.svelte";
  import RagEvals from "./components/RagEvals.svelte";
  import { Tabs, TabsContent, TabsList, TabsTrigger } from "$lib/components/ui/tabs";
  import { selectActiveRoute } from "./slices/router/router-selectors";
  import { navigateTo } from "./slices/router/router-slice";
  import { activateChat } from "./slices/chat/chat-slice";
  import {
    routeForName,
    type RouteName,
  } from "./slices/router/router-types";

  const dispose = store.init();
  onDestroy(dispose);

  const activeRoute = selectActiveRoute();

  onMount(() => {
    const cancels = appSagas.map((saga) => store.runSaga(saga));
    store.dispatch(loadSessions());
    store.dispatch(loadPapers());
    return () => cancels.forEach((cancel) => cancel());
  });

  const tabs: { id: RouteName; label: string }[] = [
    { id: "chat", label: "Chat" },
    { id: "history", label: "History Search" },
    { id: "papers", label: "Papers" },
  ];
  const triggerClass =
    "font-mono text-xs pb-1 data-active:font-semibold data-active:text-primary data-active:bg-primary/10 data-active:after:hidden";

  function navigate(name: string) {
    const routeName = name as RouteName;
    if ($activeRoute.name !== routeName) {
      store.dispatch(
        routeName === "chat"
          ? activateChat()
          : navigateTo(routeForName(routeName)),
      );
    }
  }
</script>

<div class="flex h-screen flex-col bg-background text-foreground">
  <Tabs
    value={$activeRoute.name}
    onValueChange={navigate}
    class="relative flex min-h-0 flex-1 flex-col gap-0"
  >
    <div
      data-tauri-drag-region
      class="absolute inset-x-0 top-0 z-20 flex h-[38px] items-center justify-between gap-4 bg-transparent pr-3 pl-20 backdrop-blur-md"
    >
      <TabsList
        variant="line"
        class="translate-x-[10px] translate-y-[3px] gap-4 group-data-horizontal/tabs:h-6"
      >
        {#each tabs as t}
          <TabsTrigger value={t.id} class={triggerClass}>{t.label}</TabsTrigger>
        {/each}
      </TabsList>
      <TabsList variant="line" class="translate-y-[3px] group-data-horizontal/tabs:h-6">
        <TabsTrigger
          value="evals"
          class="{triggerClass} opacity-60 hover:opacity-100 data-active:opacity-100"
          >RAG Evals</TabsTrigger
        >
      </TabsList>
    </div>

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
    {:else}
      <TabsContent value="evals" class="relative flex min-h-0 flex-1 flex-col overflow-hidden px-5 pt-[38px] pb-0">
        <RagEvals />
      </TabsContent>
    {/if}
  </Tabs>
</div>
