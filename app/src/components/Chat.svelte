<script lang="ts">
  import { store } from "../store";
  import {
    selectMessages,
    selectActiveTurn,
    selectPending,
    selectStreaming,
    selectChatMode,
    selectChatError,
    selectSessionId,
  } from "../slices/chat/chat-selectors";
  import LoaderCircle from "@lucide/svelte/icons/loader-circle";
  import { selectSessions } from "../slices/sessions/sessions-selectors";
  import { createSession, selectSession, sendMessage } from "../slices/chat/chat-slice";
  import { Button } from "$lib/components/ui/button";
  import { Textarea } from "$lib/components/ui/textarea";
  import { Badge } from "$lib/components/ui/badge";
  import { Card } from "$lib/components/ui/card";
  import { ScrollArea } from "$lib/components/ui/scroll-area";
  import {
    Select,
    SelectContent,
    SelectItem,
    SelectTrigger,
  } from "$lib/components/ui/select";
  import { cn } from "$lib/utils";

  const messages = selectMessages();
  const activeTurn = selectActiveTurn();
  const pending = selectPending();
  const streaming = selectStreaming();
  const chatMode = selectChatMode();
  const error = selectChatError();
  const sessionId = selectSessionId();
  const sessions = selectSessions();

  let text = $state("");

  const sessionIds = $derived.by(() => {
    // Preserve the backend list order; only append the current session at the
    // end when it isn't persisted yet (e.g. a freshly created session).
    const ids = $sessions
      .map((s) => s.session_id)
      .filter((id): id is string => Boolean(id));
    if ($sessionId && !ids.includes($sessionId)) {
      ids.push($sessionId);
    }
    return Array.from(new Set(ids));
  });

  // Label sessions by the first few words of their first user message, falling
  // back to the raw id for sessions that have no messages yet.
  const sessionTitles = $derived(
    new Map($sessions.map((s) => [s.session_id, s.title])),
  );

  function sessionLabel(id: string): string {
    return sessionTitles.get(id) || id;
  }

  function submit(event: Event) {
    event.preventDefault();
    if (!text.trim() || $pending) {
      return;
    }
    store.dispatch(sendMessage(text));
    text = "";
  }

  function onKeydown(event: KeyboardEvent) {
    if (event.key === "Enter" && !event.shiftKey) {
      submit(event);
    }
  }

  function switchSession(value: string) {
    if (!value) {
      return;
    }
    store.dispatch(selectSession(value));
  }

  function newSession() {
    store.dispatch(createSession());
  }
</script>

<div class="relative flex h-full min-h-0 flex-col">
  <div class="absolute inset-x-0 top-0 z-20 flex items-center gap-3 bg-transparent px-4 py-3 backdrop-blur-md">
    <Select type="single" value={$sessionId} onValueChange={switchSession} disabled={$pending}>
      <SelectTrigger
        class="w-[320px] max-w-full"
        aria-label={$sessionId ? sessionLabel($sessionId) : "Select session"}
        title={$sessionId ? sessionLabel($sessionId) : "Select session"}
      >
        <span class="min-w-0 truncate">
          {$sessionId ? sessionLabel($sessionId) : "Select session"}
        </span>
      </SelectTrigger>
      <SelectContent>
        {#each sessionIds as id}
          <SelectItem value={id} label={sessionLabel(id)}>{sessionLabel(id)}</SelectItem>
        {/each}
      </SelectContent>
    </Select>
    <Button type="button" variant="outline" size="lg" onclick={newSession} disabled={$pending}>
      New session
    </Button>
    {#if $chatMode}
      <Badge variant="secondary" class="font-mono">mode: {$chatMode}</Badge>
    {/if}
  </div>

  <Card class="min-h-0 flex-1 gap-0 py-0">
    <ScrollArea class="h-full">
      <div class="flex flex-col gap-3 p-4 pt-14 pb-72">
        {#each $messages as m, i}
          <div
            data-exchange-id={m.exchange_id}
            class={cn(
              "flex max-w-[80%] flex-col gap-1 rounded-lg px-3 py-2",
              m.role === "user"
                ? "self-end bg-primary text-primary-foreground"
                : "self-start bg-muted",
            )}
          >
            <span class="font-mono text-[0.7rem] font-medium uppercase opacity-70">{m.role}</span>
            <p class="m-0 whitespace-pre-wrap text-sm">{m.content}</p>
            {#if m.role === "assistant" && m.sources?.length}
              <section
                class="mt-2 w-full border-t border-border/70 pt-2"
                aria-label={`Retrieved document metadata for assistant message ${i + 1}`}
              >
                <div class="mb-2 flex items-center gap-2">
                  <span class="text-xs font-semibold">Retrieved sources</span>
                  <Badge variant="secondary">{m.sources.length}</Badge>
                </div>
                <ol class="m-0 flex list-none flex-col gap-2 p-0" aria-label="Retrieved documents">
                  {#each m.sources as source}
                    <li>
                      <Card class="gap-1.5 p-3">
                        <div class="flex flex-wrap items-center gap-2">
                          <Badge variant="outline">Rank {source.rank}</Badge>
                          <span class="text-xs font-semibold">{source.title}</span>
                        </div>
                        <span class="text-xs text-muted-foreground">{source.filename}</span>
                        <span class="text-xs text-muted-foreground">
                          Page {source.page} · Chunk {source.chunk_index} · Score {source.score.toFixed(4)}
                        </span>
                        <p class="m-0 text-xs leading-relaxed">{source.snippet}</p>
                      </Card>
                    </li>
                  {/each}
                </ol>
              </section>
            {/if}
          </div>
        {/each}
        {#if $activeTurn}
          <section
            class="flex w-full max-w-[90%] flex-col gap-3 self-start rounded-lg bg-muted px-3 py-3"
            aria-label="Multihop answer progress"
            aria-live="polite"
            aria-busy={$pending}
          >
            <span class="font-mono text-[0.7rem] font-medium uppercase opacity-70">assistant</span>
            {#if $activeTurn.subqueries.length}
              <div class="flex flex-col gap-2">
                <h2 class="m-0 text-sm font-semibold">Retrieval plan</h2>
                <ol class="m-0 flex flex-col gap-2 pl-5">
                  {#each $activeTurn.subqueries as subquery (subquery.id)}
                    <li aria-label={`Subquery ${subquery.index}: ${subquery.status}`}>
                      <div class="flex flex-wrap items-center gap-2">
                        <span class="text-sm font-medium">{subquery.query}</span>
                        <Badge variant={subquery.status === "complete" ? "secondary" : "outline"}>
                          {subquery.status}
                        </Badge>
                        {#if subquery.status === "active"}
                          <LoaderCircle class="size-3.5 animate-spin" aria-hidden="true" />
                        {/if}
                      </div>
                      {#if subquery.answer}
                        <p class="mt-1 mb-0 whitespace-pre-wrap text-sm">{subquery.answer}</p>
                        <span class="text-xs text-muted-foreground">
                          {subquery.sources.length} retrieved {subquery.sources.length === 1 ? "source" : "sources"}
                        </span>
                      {/if}
                    </li>
                  {/each}
                </ol>
              </div>
            {/if}
            {#if $activeTurn.finalAnswer || $streaming}
              <section class="border-t border-border/70 pt-2" aria-label="Final composed answer">
                <div class="mb-1 flex items-center gap-2">
                  <h2 class="m-0 text-sm font-semibold">Final composed answer</h2>
                  {#if $streaming}<Badge variant="outline">streaming</Badge>{/if}
                </div>
                <p class="m-0 whitespace-pre-wrap text-sm">{$activeTurn.finalAnswer}{#if $streaming}<span
                      class="ml-0.5 inline-block h-3.5 w-1.5 translate-y-0.5 animate-pulse bg-current align-baseline"
                      aria-hidden="true"
                    ></span>{/if}</p>
              </section>
            {:else if !$activeTurn.subqueries.length}
              <div class="flex items-center gap-2 text-sm text-muted-foreground italic">
                <LoaderCircle class="size-4 animate-spin" aria-hidden="true" />
                <span>Planning answer…</span>
              </div>
            {/if}
          </section>
        {:else if $pending && !$streaming}
          <div
            class="flex items-center gap-2 self-start rounded-lg bg-muted px-3 py-2 text-sm text-muted-foreground italic"
          >
            <LoaderCircle class="size-4 animate-spin" />
            <span>…thinking</span>
          </div>
        {/if}
        {#if $error}
          <p class="text-sm text-destructive" role="alert">Failed: {$error}</p>
        {/if}
      </div>
    </ScrollArea>
  </Card>

  <form
    class="absolute inset-x-0 bottom-0 z-10 bg-transparent p-4"
    onsubmit={submit}
  >
    <Textarea
      class="max-h-60 min-h-28 w-full resize-none pr-20 pb-16"
      rows={3}
      placeholder="Ask a question… (Enter to send, Shift+Enter for a new line)"
      bind:value={text}
      disabled={$pending}
      onkeydown={onKeydown}
    />
    <Button
      type="submit"
      size="lg"
      class="absolute right-7 bottom-7 z-10"
      disabled={$pending || !text.trim()}
    >Send</Button>
  </form>
</div>
