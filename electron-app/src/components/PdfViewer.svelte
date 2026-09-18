<script lang="ts">
  import {
    selectPdfUrl,
    selectPdfLoading,
    selectSelectedPaperId,
  } from "../slices/papers/papers-selectors";

  const pdfUrl = selectPdfUrl();
  const pdfLoading = selectPdfLoading();
  const selectedId = selectSelectedPaperId();
</script>

<div class="flex h-full w-full overflow-hidden">
  {#if $pdfLoading}
    <p class="m-auto text-sm text-muted-foreground">Loading PDF…</p>
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
