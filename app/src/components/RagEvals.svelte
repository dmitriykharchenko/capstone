<script lang="ts">
  import LoaderCircle from "@lucide/svelte/icons/loader-circle";
  import Upload from "@lucide/svelte/icons/upload";
  import { Button } from "$lib/components/ui/button";
  import {
    Select,
    SelectContent,
    SelectItem,
    SelectTrigger,
  } from "$lib/components/ui/select";
  import { parseEvaluationTestCases } from "$lib/evaluation-upload";
  import type {
    EvaluationJudgeScores,
    EvaluationRetrievalType,
  } from "$lib/evaluation-types";
  import { store } from "../store";
  import {
    selectEvaluationAggregateScores,
    selectEvaluationCompleted,
    selectEvaluationCurrentCaseIndex,
    selectEvaluationError,
    selectEvaluationFailed,
    selectEvaluationResults,
    selectEvaluationRunning,
    selectEvaluationSucceeded,
    selectEvaluationTotal,
  } from "../slices/evaluation/evaluation-selectors";
  import { runEvaluation } from "../slices/evaluation/evaluation-slice";

  const criteria = [
    ["context_precision", "Context precision"],
    ["context_recall", "Context recall"],
    ["relevant_chunk_ranking", "Relevant chunk ranking"],
    ["context_utilization", "Context utilization"],
    ["semantic_similarity", "Semantic similarity"],
    ["completeness", "Completeness"],
    ["hallucination_rate", "Hallucination rate"],
  ] as const;

  const running = selectEvaluationRunning();
  const total = selectEvaluationTotal();
  const completed = selectEvaluationCompleted();
  const succeeded = selectEvaluationSucceeded();
  const failed = selectEvaluationFailed();
  const currentCaseIndex = selectEvaluationCurrentCaseIndex();
  const results = selectEvaluationResults();
  const aggregates = selectEvaluationAggregateScores();
  const transportError = selectEvaluationError();

  let retrievalType = $state<EvaluationRetrievalType>("vector");
  let fileInput = $state<HTMLInputElement>();
  let uploadError = $state<string | null>(null);
  const progressPercent = $derived(
    $total > 0 ? Math.round(($completed / $total) * 100) : 0,
  );

  function selectRetrieval(value: string) {
    if (value === "vector" || value === "bm25" || value === "hybrid") {
      retrievalType = value;
    }
  }

  async function addTests(event: Event) {
    const input = event.currentTarget as HTMLInputElement;
    const file = input.files?.[0];
    input.value = "";
    if (!file) return;
    uploadError = null;
    if (!file.name.toLowerCase().endsWith(".json")) {
      uploadError = "Choose a .json file.";
      return;
    }
    let text: string;
    try {
      text = await file.text();
    } catch {
      uploadError = "The selected file could not be read.";
      return;
    }
    const parsed = parseEvaluationTestCases(text);
    if (!parsed.ok) {
      uploadError = parsed.error;
      return;
    }
    store.dispatch(runEvaluation(retrievalType, parsed.testCases));
  }

  function criterionDetails(scores: EvaluationJudgeScores, key: keyof EvaluationJudgeScores) {
    return scores[key];
  }

  function criterionEvidence(
    criterion: EvaluationJudgeScores[keyof EvaluationJudgeScores],
  ): Array<[string, string]> {
    if ("relevant_chunk_ranks" in criterion) {
      return [["Relevant chunk ranks", criterion.relevant_chunk_ranks.join(", ") || "None"]];
    }
    if ("chunks_used" in criterion) {
      return [["Chunks used", criterion.chunks_used.join(", ") || "None"]];
    }
    if ("contradicting_claims" in criterion) {
      return [["Contradicting claims", criterion.contradicting_claims.join("; ") || "None"]];
    }
    if ("missing_elements" in criterion) {
      return [["Missing elements", criterion.missing_elements.join("; ") || "None"]];
    }
    if ("unsupported_claims" in criterion) {
      return [["Unsupported claims", criterion.unsupported_claims.join("; ") || "None"]];
    }
    return [];
  }
</script>

<div class="flex h-full min-h-0 flex-col">
  <div class="flex shrink-0 items-center gap-2 py-3">
    <Select type="single" value={retrievalType} onValueChange={selectRetrieval} disabled={$running}>
      <SelectTrigger class="w-40" aria-label="Retrieval strategy" title="Retrieval strategy">
        <span class="capitalize">{retrievalType}</span>
      </SelectTrigger>
      <SelectContent>
        <SelectItem value="vector" label="vector">vector</SelectItem>
        <SelectItem value="bm25" label="bm25">bm25</SelectItem>
        <SelectItem value="hybrid" label="hybrid">hybrid</SelectItem>
      </SelectContent>
    </Select>
    <input
      bind:this={fileInput}
      class="sr-only"
      type="file"
      accept=".json"
      aria-label="Evaluation tests JSON file"
      disabled={$running}
      onchange={addTests}
    />
    <Button type="button" size="lg" onclick={() => fileInput?.click()} disabled={$running}>
      {#if $running}
        <LoaderCircle class="size-4 animate-spin" />
        Running…
      {:else}
        <Upload class="size-4" />
        Add tests JSON
      {/if}
    </Button>
  </div>

  <div class="min-h-0 flex-1 overflow-y-auto pb-5">
    {#if uploadError}
      <p class="mb-3 rounded-lg bg-destructive/10 p-3 text-sm text-destructive" role="alert">
        {uploadError}
      </p>
    {/if}

    {#if $transportError}
      <p class="mb-3 rounded-lg bg-destructive/10 p-3 text-sm text-destructive" role="alert">
        Evaluation stopped: {$transportError}
      </p>
    {/if}

    {#if $running}
      <section class="mb-4 rounded-xl border bg-card p-4" aria-labelledby="evaluation-progress-title">
        <div class="mb-2 flex items-center justify-between gap-3 text-sm">
          <h2 id="evaluation-progress-title" class="font-medium">Evaluation in progress</h2>
          <span aria-live="polite">
            {$completed}/{$total}{#if $currentCaseIndex} · test {$currentCaseIndex}{/if}
          </span>
        </div>
        <div
          class="h-2 overflow-hidden rounded-full bg-muted"
          role="progressbar"
          aria-label="Evaluation progress"
          aria-valuemin={0}
          aria-valuemax={$total}
          aria-valuenow={$completed}
          aria-valuetext={`${$completed} of ${$total} tests completed`}
        >
          <div class="h-full bg-primary transition-all" style={`width: ${progressPercent}%`}></div>
        </div>
      </section>
    {:else if !$transportError && !$aggregates && $results.every((result) => result === null)}
      <p class="rounded-xl border border-dashed p-8 text-center text-sm text-muted-foreground" role="status">
        Add a tests JSON file to start an evaluation.
      </p>
    {/if}

    {#if $aggregates}
      <section class="mb-4" aria-labelledby="aggregate-results-title">
        <div class="mb-3 flex flex-wrap items-baseline justify-between gap-2">
          <h2 id="aggregate-results-title" class="text-lg font-semibold">Aggregate means</h2>
          <p class="text-sm text-muted-foreground" role="status" aria-live="polite">
            Evaluation completed: {$succeeded} succeeded, {$failed} failed.
          </p>
        </div>
        {#if $failed > 0}
          <p class="mb-3 text-sm text-destructive" role="status">
            Partial failure: {$failed} of {$total} tests could not be fully evaluated.
          </p>
        {/if}
        <dl class="grid gap-2 sm:grid-cols-2 lg:grid-cols-4">
          {#each criteria as [key, label]}
            <div class="rounded-lg border bg-card p-3">
              <dt class="text-xs text-muted-foreground">{label}</dt>
              <dd class="mt-1 font-mono text-lg font-semibold">
                {$aggregates[key] === null ? "—" : $aggregates[key].toFixed(2)}
              </dd>
            </div>
          {/each}
        </dl>
      </section>
    {/if}

    {#if $results.some((result) => result !== null)}
      <section aria-labelledby="case-results-title">
        <h2 id="case-results-title" class="mb-3 text-lg font-semibold">Test results</h2>
        <div class="space-y-2">
          {#each $results as result, index}
            {#if result}
              <details class="rounded-lg border bg-card" data-case-index={result.case_index}>
                <summary class="flex items-center justify-between gap-3 px-4 py-3 text-sm font-medium">
                  <span>Test {index + 1}: {result.input.question}</span>
                  <span class={result.error ? "text-destructive" : "text-muted-foreground"}>
                    {result.error ? "Failed" : "Completed"}
                  </span>
                </summary>
                <div class="space-y-4 border-t px-4 py-3 text-sm">
                  <section aria-label="Question">
                    <h3 class="font-medium">Question</h3>
                    <p class="mt-1 whitespace-pre-wrap text-muted-foreground">{result.input.question}</p>
                  </section>
                  <section aria-label="Generated answer">
                    <h3 class="font-medium">Generated answer</h3>
                    <p class="mt-1 whitespace-pre-wrap text-muted-foreground">
                      {result.generated_answer ?? "No answer was generated."}
                    </p>
                  </section>
                  <section aria-label="Retrieved source provenance">
                    <h3 class="font-medium">Retrieved source provenance</h3>
                    {#if result.retrieved_chunks.length === 0}
                      <p class="mt-1 text-muted-foreground">No sources were retrieved.</p>
                    {:else}
                      <ol class="mt-2 space-y-2">
                        {#each result.retrieved_chunks as chunk}
                          <li class="rounded-md bg-muted/60 p-3">
                            <p class="font-medium">#{chunk.rank} {chunk.title || chunk.filename}</p>
                            <p class="text-xs text-muted-foreground">
                              {chunk.filename} · page {chunk.page} · score {chunk.score.toFixed(3)} ·
                              paper {chunk.paper_id} · chunk {chunk.chunk_index}
                            </p>
                            <p class="mt-2 whitespace-pre-wrap">{chunk.text}</p>
                          </li>
                        {/each}
                      </ol>
                    {/if}
                  </section>
                  <section aria-label="Judge results">
                    <h3 class="font-medium">Judge results</h3>
                    {#if result.judge_scores}
                      <dl class="mt-2 space-y-2">
                        {#each criteria as [key, label]}
                          {@const criterion = criterionDetails(result.judge_scores, key)}
                          <div class="rounded-md bg-muted/60 p-3">
                            <dt class="font-medium">{label}: {criterion.score.toFixed(2)}</dt>
                            <dd class="mt-1 text-muted-foreground">
                              Confidence: {criterion.confidence}. {criterion.reasoning}
                            </dd>
                            {#if criterion.flags.length > 0}
                              <dd class="mt-1 text-xs text-muted-foreground">
                                Flags: {criterion.flags.join(", ")}
                              </dd>
                            {/if}
                            {#each criterionEvidence(criterion) as [evidenceLabel, evidence]}
                              <dd class="mt-1 text-xs text-muted-foreground">
                                {evidenceLabel}: {evidence}
                              </dd>
                            {/each}
                          </div>
                        {/each}
                      </dl>
                    {:else}
                      <p class="mt-1 text-muted-foreground">No judge results are available.</p>
                    {/if}
                  </section>
                  {#if result.error}
                    <section class="rounded-md bg-destructive/10 p-3 text-destructive" aria-label="Evaluation error">
                      <h3 class="font-medium">Error during {result.error.stage}</h3>
                      <p class="mt-1">{result.error.message}</p>
                    </section>
                  {/if}
                </div>
              </details>
            {/if}
          {/each}
        </div>
      </section>
    {/if}
  </div>
</div>