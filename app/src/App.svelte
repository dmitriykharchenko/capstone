<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import { store, appSaga } from "./store";
  import Chat from "./components/Chat.svelte";
  import HistorySearch from "./components/HistorySearch.svelte";
  import LibraryChooser from "./components/LibraryChooser.svelte";
  import PapersList from "./components/PapersList.svelte";
  import RagEvals from "./components/RagEvals.svelte";
  import LoaderCircle from "@lucide/svelte/icons/loader-circle";
  import { Card } from "$lib/components/ui/card";
  import { Tabs, TabsContent, TabsList, TabsTrigger } from "$lib/components/ui/tabs";
  import { selectActiveRoute } from "./slices/router/router-selectors";
  import { navigateTo } from "./slices/router/router-slice";
  import { activateChat } from "./slices/chat/chat-slice";
  import {
    routeForName,
    type RouteName,
  } from "./slices/router/router-types";
  import {
    selectLibraryError,
    selectLibraryStatus,
  } from "./slices/library/library-selectors";

  const dispose = store.init();
  onDestroy(dispose);

  const activeRoute = selectActiveRoute();
  const libraryStatus = selectLibraryStatus();
  const libraryError = selectLibraryError();

  onMount(() => {
    const cancel = store.runSaga(appSaga);
    return cancel;
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
  {#if $libraryStatus === "idle" || $libraryStatus === "loading"}
    <div class="flex h-full items-center justify-center" aria-live="polite">
      <div class="flex items-center gap-2 text-sm text-muted-foreground">
        <LoaderCircle class="size-4 animate-spin" />
        Opening the Papers folder…
      </div>
    </div>
  {:else if $libraryStatus === "error"}
    <div class="flex h-full items-center justify-center p-6">
      <Card class="flex w-full max-w-lg flex-col gap-4 p-6">
        <div>
          <h1 class="text-lg font-semibold">Papers folder unavailable</h1>
          <p class="mt-1 text-sm text-muted-foreground">
            The saved folder could not be opened. Choose another folder to continue.
          </p>
        </div>
        <p class="text-sm text-destructive" role="alert">{$libraryError}</p>
        <LibraryChooser />
      </Card>
    </div>
  {:else}
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
  {/if}
</div>
