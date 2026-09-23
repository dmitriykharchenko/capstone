<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import { store } from "../store";
  import { closePaper } from "../slices/papers/papers-slice";
  import { selectSelectedPaperId } from "../slices/papers/papers-selectors";
  import { createPdfReader, initialReaderState } from "../lib/pdf-reader";
  import { PDF_LIMITS } from "../lib/pdf-page";
  import type { PdfPasswordPrompt } from "../lib/pdf-runtime";
  import "../lib/pdf-reader.css";

  const selectedId = selectSelectedPaperId();
  let viewState = $state(initialReaderState());
  let prompt = $state.raw<PdfPasswordPrompt>();
  let passwordInput = $state.raw<HTMLInputElement>();
  let pageInput = $state(1);
  let surface: HTMLDivElement;
  let scroll: HTMLDivElement;
  let reader: ReturnType<typeof createPdfReader> | undefined;
  const controlsEnabled = $derived(viewState.pages > 0 && (viewState.phase === 'ready' || viewState.phase === 'rendering'));

  function clearPassword(next?: PdfPasswordPrompt) {
    if (passwordInput) passwordInput.value = '';
    prompt = next;
  }
  function unlock(event: SubmitEvent) {
    event.preventDefault();
    if (!passwordInput || !prompt) return;
    let value = passwordInput.value;
    passwordInput.value = '';
    prompt.submit(value);
    value = '';
  }
  function cancelPassword() {
    const pending = prompt;
    clearPassword();
    void pending?.cancel();
  }
  function close() {
    void reader?.open(null);
    store.dispatch(closePaper());
  }
  onMount(() => {
    reader = createPdfReader(surface, next => {
      viewState = next;
      pageInput = next.page;
    }, clearPassword);
    const unsubscribe = selectedId.subscribe(id => { scroll.scrollTo(0, 0); void reader?.open(id); });
    const resize = new ResizeObserver(() => reader?.resize(scroll.clientWidth, scroll.clientHeight));
    resize.observe(scroll);
    reader.resize(scroll.clientWidth, scroll.clientHeight);
    const wheel = (event: WheelEvent) => {
      if (event.ctrlKey || event.shiftKey) return;
      const unit = event.deltaMode === 1 ? 16 : event.deltaMode === 2 ? scroll.clientHeight : 1;
      if (reader?.scrollBy(event.deltaY * unit)) {
        event.preventDefault();
        scroll.scrollLeft += event.deltaX * unit;
      }
    };
    scroll.addEventListener('wheel', wheel, { passive: false });
    const shutdown = () => { clearPassword(); void reader?.destroy(); };
    window.addEventListener('beforeunload', shutdown);
    return () => {
      unsubscribe(); resize.disconnect();
      scroll.removeEventListener('wheel', wheel);
      window.removeEventListener('beforeunload', shutdown);
      shutdown(); reader = undefined;
    };
  });
  onDestroy(() => store.dispatch(closePaper()));
</script>

<div class="pdf-reader flex h-full min-w-0 flex-col overflow-hidden">
  {#if $selectedId}
    <div class="flex flex-wrap items-center gap-2 border-b bg-background p-2 text-sm" role="group" aria-label="PDF controls">
      <button type="button" disabled={!controlsEnabled || viewState.page <= 1} onclick={() => reader?.page(viewState.page - 1)}>Previous page</button>
      <form class="flex items-center gap-1" onsubmit={event => { event.preventDefault(); reader?.page(pageInput); pageInput = viewState.page; }}>
        <label for="pdf-page-number">Page</label>
        <input id="pdf-page-number" class="w-16" type="number" min="1" max={viewState.pages || 1} step="1" bind:value={pageInput} disabled={!controlsEnabled} />
        <span>of {viewState.pages || '—'}</span>
        <button type="submit" disabled={!controlsEnabled}>Go</button>
      </form>
      <button type="button" disabled={!controlsEnabled || viewState.page >= viewState.pages} onclick={() => reader?.page(viewState.page + 1)}>Next page</button>
      <button type="button" aria-label="Zoom out" disabled={!controlsEnabled || viewState.zoom <= PDF_LIMITS.minZoom} onclick={() => reader?.zoom(viewState.zoom - 0.25)}>−</button>
      <output aria-label="PDF zoom">{Math.round(viewState.zoom * 100)}%</output>
      <button type="button" aria-label="Zoom in" disabled={!controlsEnabled || viewState.zoom >= PDF_LIMITS.maxZoom} onclick={() => reader?.zoom(viewState.zoom + 0.25)}>+</button>
      <button type="button" disabled={!controlsEnabled} aria-pressed={viewState.fit} onclick={() => reader?.fit()}>Fit width</button>
      <button type="button" class="ml-auto" onclick={close}>Close PDF</button>
    </div>
  {/if}
  {#if viewState.phase === 'loading' || viewState.phase === 'rendering' || viewState.phase === 'ready'}
    <!-- Keep viewport height stable when a lazy window finishes rendering. -->
    <p role="status" class="shrink-0 p-2 text-center text-sm" class:invisible={viewState.phase === 'ready'} aria-hidden={viewState.phase === 'ready'}>{viewState.phase === 'loading' ? 'Loading PDF…' : 'Rendering visible PDF pages…'}</p>
  {:else if viewState.phase === 'error' || viewState.phase === 'cancelled'}
    <div class="p-4 text-center text-sm">
      <p role="alert">{viewState.message}</p>
      <button type="button" class="mt-2" onclick={() => { void reader?.open($selectedId); }}>Retry PDF</button>
    </div>
  {:else if viewState.phase === 'idle'}
    <p class="m-auto p-4 text-sm text-muted-foreground">Select a paper to preview its PDF.</p>
  {/if}
  {#if prompt}
    <form class="mx-auto my-4 flex max-w-sm flex-col gap-3 p-4" aria-label="Unlock PDF" onsubmit={unlock}>
      <p role="status">{prompt.reason === 'incorrect' ? 'Incorrect password. Try again.' : 'This PDF requires a password.'}</p>
      <label for="pdf-password">PDF password</label>
      <input id="pdf-password" type="password" autocomplete="off" spellcheck={false} bind:this={passwordInput} />
      <p class="text-xs text-muted-foreground">Used only in this local reader, never saved or sent to the backend.</p>
      <div class="flex gap-2"><button type="submit">Unlock</button><button type="button" onclick={cancelPassword}>Cancel</button></div>
    </form>
  {/if}
  {#if viewState.notice}<p role="status" class="p-2 text-center text-xs">{viewState.notice}</p>{/if}
  <!-- svelte-ignore a11y_no_noninteractive_tabindex, a11y_no_noninteractive_element_interactions (The focusable document region preserves keyboard scrolling across virtual coordinates.) -->
  <div bind:this={scroll} onscroll={() => reader?.scrolled(scroll.scrollTop)} onkeydown={event => { if (!event.shiftKey && !event.altKey && !event.metaKey && reader?.scrollKey(event.key)) event.preventDefault(); }} class="relative min-h-0 flex-1 overflow-auto bg-muted/40" role="region" aria-label="PDF document, scroll to read" tabindex="0" aria-busy={viewState.phase === 'rendering'}>
    <div bind:this={surface} class="pdf-surface p-4"></div>
  </div>
</div>
