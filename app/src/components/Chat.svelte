<script lang="ts">
  import { store } from "../store";
  import {
    selectMessages,
    selectPending,
    selectChatMode,
    selectChatError,
    selectSessionId,
  } from "../slices/chat/chat-selectors";
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
  const chatMode = selectChatMode();
  const error = selectChatError();
  const sessionId = selectSessionId();
  const sessions = selectSessions();

  let text = $state("");

  const sessionIds = $derived.by(() => {
    const ids = [$sessionId, ...$sessions.map((s) => s.session_id)];
    return Array.from(new Set(ids.filter((id): id is string => Boolean(id))));
  });

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
</script>

<div class="flex h-full flex-col gap-3">
  <div class="flex items-center gap-3">
    <Select type="single" value={$sessionId} onValueChange={switchSession}>
      <SelectTrigger class="w-[240px]">{$sessionId || "Select session"}</SelectTrigger>
      <SelectContent>
        {#each sessionIds as id}
          <SelectItem value={id} label={id}>{id}</SelectItem>
        {/each}
      </SelectContent>
    </Select>
    {#if $chatMode}
      <Badge variant="secondary">mode: {$chatMode}</Badge>
    {/if}
  </div>

  <Card class="min-h-0 flex-1 gap-0 py-0">
    <ScrollArea class="h-full">
      <div class="flex flex-col gap-3 p-4">
        {#each $messages as m}
          <div
            class={cn(
              "flex max-w-[80%] flex-col gap-1 rounded-lg px-3 py-2",
              m.role === "user"
                ? "self-end bg-primary text-primary-foreground"
                : "self-start bg-muted",
            )}
          >
            <span class="text-[0.7rem] font-medium uppercase opacity-70">{m.role}</span>
            <p class="m-0 whitespace-pre-wrap text-sm">{m.content}</p>
          </div>
        {/each}
        {#if $pending}
          <div
            class="self-start rounded-lg bg-muted px-3 py-2 text-sm text-muted-foreground italic"
          >
            …thinking
          </div>
        {/if}
        {#if $error}
          <p class="text-sm text-destructive">{$error}</p>
        {/if}
      </div>
    </ScrollArea>
  </Card>

  <form class="flex items-end gap-2" onsubmit={submit}>
    <Textarea
      class="max-h-40 min-h-10 flex-1 resize-none"
      rows={1}
      placeholder="Ask a question… (Enter to send, Shift+Enter for a new line)"
      bind:value={text}
      disabled={$pending}
      onkeydown={onKeydown}
    />
    <Button type="submit" disabled={$pending || !text.trim()}>Send</Button>
  </form>
</div>
