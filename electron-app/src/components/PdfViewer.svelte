<script lang="ts">
  import { onDestroy } from "svelte";
  import { store } from "../store";
  import { closePaper } from "../slices/papers/papers-slice";
  import {
    selectPdfError,
    selectPdfUrl,
    selectPdfLoading,
    selectSelectedPaperId,
  } from "../slices/papers/papers-selectors";

  const pdfUrl = selectPdfUrl();
  const pdfLoading = selectPdfLoading();
  const selectedId = selectSelectedPaperId();
  const pdfError = selectPdfError();
  onDestroy(() => store.dispatch(closePaper()));
</script>

<div class="flex h-full w-full overflow-hidden">
  {#if $pdfLoading}
    <p class="m-auto text-sm text-muted-foreground">Loading PDF…</p>
  {:else if $pdfError}
    <p role="alert" class="m-auto max-w-md p-4 text-sm text-destructive">{$pdfError}</p>
  {:else if $pdfUrl}
    <iframe
      class="h-full w-full flex-1 border-0"
      title={$selectedId ?? "paper"}
      src={$pdfUrl}
    ></iframe>
  {:else}
    <p class="m-auto text-sm text-muted-foreground">
      Select a paper to preview its PDF.
    </p>
  {/if}
</div>
