<script lang="ts">
  import FolderOpen from "@lucide/svelte/icons/folder-open";
  import LoaderCircle from "@lucide/svelte/icons/loader-circle";
  import { Button } from "$lib/components/ui/button";
  import { store } from "../store";
  import {
    selectLibrary,
    selectLibraryChoosing,
    selectLibraryError,
    selectLibraryWorkActive,
  } from "../slices/library/library-selectors";
  import { chooseLibrary } from "../slices/library/library-slice";

  const library = selectLibrary();
  const choosing = selectLibraryChoosing();
  const error = selectLibraryError();
  const workActive = selectLibraryWorkActive();
  const displayPath = $derived(
    $library?.path ?? "Default papers folder (no custom folder selected)",
  );

  function openFolder() {
    if (!$choosing && !$workActive) store.dispatch(chooseLibrary());
  }
</script>

<div class="flex min-w-0 max-w-72 items-center gap-2">
  <div class="min-w-0 flex-1 leading-tight" aria-live="polite">
    <span class="block text-[10px] font-medium uppercase tracking-wide text-muted-foreground">
      Papers folder
    </span>
    <span class="block truncate text-xs" title={displayPath}>{displayPath}</span>
    {#if $error}
      <span class="block truncate text-[11px] text-destructive" role="alert" title={$error}>
        {$error}
      </span>
    {/if}
  </div>
  <Button
    type="button"
    variant="outline"
    size="lg"
    class="shrink-0"
    onclick={openFolder}
    disabled={$choosing || $workActive}
    aria-label="Open Papers folder"
    title={$workActive
      ? "Finish active work before changing the Papers folder"
      : "Choose a research-paper folder"}
  >
    {#if $choosing}
      <LoaderCircle class="size-4 animate-spin" />
      Opening…
    {:else}
      <FolderOpen class="size-4" />
      Open folder
    {/if}
  </Button>
</div>