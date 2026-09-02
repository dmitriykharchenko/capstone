<script lang="ts">
  import { store } from "../store";
  import {
    selectMessages,
    selectPending,
    selectStreaming,
    selectChatMode,
    selectChatError,
    selectSessionId,
  } from "../slices/chat/chat-selectors";
  import LoaderCircle from "@lucide/svelte/icons/loader-circle";
  import { selectSessions } from "../slices/sessions/sessions-selectors";
  import { sendMessage, setSessionId, loadSession } from "../slices/chat/chat-slice";
  import { loadSessions } from "../slices/sessions/sessions-slice";
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
  const pending = selectPending();
  const streaming = selectStreaming();
  const chatMode = selectChatMode();
  const error = selectChatError();
  const sessionId = selectSessionId();
  const sessions = selectSessions();

  let text = $state("");

  const sessionIds = $derived.by(() => {
    const ids = [$sessionId, ...$sessions.map((s) => s.session_id)];
    return Array.from(new Set(ids.filter((id): id is string => Boolean(id))));
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
    store.dispatch(setSessionId(value));
    store.dispatch(loadSession(value));
    store.dispatch(loadSessions());
  }

  function newSession() {
    store.dispatch(setSessionId(`session-${crypto.randomUUID()}`));
  }
</script>

<div class="flex h-full flex-col gap-3">
  <div class="flex items-center gap-3">
    <Select type="single" value={$sessionId} onValueChange={switchSession}>
      <SelectTrigger class="w-[240px]">
        {$sessionId ? sessionLabel($sessionId) : "Select session"}
      </SelectTrigger>
      <SelectContent>
        {#each sessionIds as id}
          <SelectItem value={id} label={sessionLabel(id)}>{sessionLabel(id)}</SelectItem>
        {/each}
      </SelectContent>
    </Select>
    <Button type="button" variant="outline" onclick={newSession} disabled={$pending}>
      New session
    </Button>
    {#if $chatMode}
      <Badge variant="secondary" class="font-mono">mode: {$chatMode}</Badge>
    {/if}
  </div>

  <Card class="min-h-0 flex-1 gap-0 py-0">
    <ScrollArea class="h-full">
      <div class="flex flex-col gap-3 p-4">
        {#each $messages as m, i}
          <div
            class={cn(
              "flex max-w-[80%] flex-col gap-1 rounded-lg px-3 py-2",
              m.role === "user"
                ? "self-end bg-primary text-primary-foreground"
                : "self-start bg-muted",
            )}
          >
            <span class="font-mono text-[0.7rem] font-medium uppercase opacity-70">{m.role}</span>
            <p class="m-0 whitespace-pre-wrap text-sm">{m.content}{#if $streaming && i === $messages.length - 1 && m.role === "assistant"}<span
                  class="ml-0.5 inline-block h-3.5 w-1.5 translate-y-0.5 animate-pulse bg-current align-baseline"
                ></span>{/if}</p>
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
        {#if $pending && !$streaming}
          <div
            class="flex items-center gap-2 self-start rounded-lg bg-muted px-3 py-2 text-sm text-muted-foreground italic"
          >
            <LoaderCircle class="size-4 animate-spin" />
            <span>…thinking</span>
          </div>
        {/if}
        {#if $error}
          <p class="text-sm text-destructive">{$error}</p>
        {/if}
      </div>
    </ScrollArea>
  </Card>

  <form class="flex flex-col gap-2" onsubmit={submit}>
    <Textarea
      class="max-h-60 min-h-28 w-full resize-none"
      rows={3}
      placeholder="Ask a question… (Enter to send, Shift+Enter for a new line)"
      bind:value={text}
      disabled={$pending}
      onkeydown={onKeydown}
    />
    <Button type="submit" class="self-end" disabled={$pending || !text.trim()}>Send</Button>
  </form>
</div>
