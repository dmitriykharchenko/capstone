<script lang="ts">
  import LoaderCircle from "@lucide/svelte/icons/loader-circle";
  import Upload from "@lucide/svelte/icons/upload";
  import { Button } from "$lib/components/ui/button";
  import { parseEvaluationTestCases } from "$lib/evaluation-upload";
  import { evaluationScoreColor } from "$lib/evaluation-score";
  import { failureSummary } from "$lib/evaluation-diagnostics";
  import {
    evaluationMetricTotals,
    evaluationReliabilityTotals,
    formatCost,
    formatLatency,
    formatMetricTotal,
    performanceCheckLabel,
    sanitizeEvaluationPerformance,
  } from "$lib/evaluation-metrics";
  import EvaluationRuntimeMetrics from "./EvaluationRuntimeMetrics.svelte";
  import type { EvaluationJudgeScores } from "$lib/evaluation-types";
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
    selectEvaluationRetryRound,
    selectEvaluationRetryPending,
    selectEvaluationRetryCompleted,
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
  const retryRound = selectEvaluationRetryRound();
  const retryPending = selectEvaluationRetryPending();
  const retryCompleted = selectEvaluationRetryCompleted();
  const metricTotals = $derived(evaluationMetricTotals($results));
  const metricSummary = $derived([
    ["Active latency", formatMetricTotal(metricTotals.latency, formatLatency)],
    ["Model calls (all)", formatMetricTotal(metricTotals.modelCalls)],
    ["RAG exchanges", formatMetricTotal(metricTotals.exchanges)],
    ["Retrieval tool calls", formatMetricTotal(metricTotals.toolCalls)],
    ["Total tokens", formatMetricTotal(metricTotals.tokens)],
    ["Cached input tokens", formatMetricTotal(metricTotals.cachedTokens)],
    ["Estimated cost (USD)", formatMetricTotal(metricTotals.cost, formatCost)],
    ["RAG chunk occurrences", formatMetricTotal(metricTotals.chunks)],
    ["RAG evidence characters", formatMetricTotal(metricTotals.characters)],
  ]);
  const reliability = $derived(evaluationReliabilityTotals($results));
  const ofChecked = (count: number) =>
    reliability.performanceChecked > 0 ? `${count} of ${reliability.performanceChecked}` : "Unavailable";
  const performanceSummary = $derived([
    ["Within all limits", ofChecked(reliability.performancePass)],
    ["Over a limit", ofChecked(reliability.performanceFail)],
    ["Incomplete checks", ofChecked(reliability.performanceIncomplete)],
    [
      "Limits exceeded",
      reliability.failedChecks.length === 0
        ? "None"
        : reliability.failedChecks.map(([name, count]) => `${performanceCheckLabel(name)} (${count})`).join(", "),
    ],
  ]);
  const reliabilitySummary = $derived([
    [
      "Success rate",
      reliability.cases > 0 ? `${Math.round((reliability.succeeded / reliability.cases) * 100)}%` : "Unavailable",
    ],
    ["Succeeded on first try", `${reliability.firstTry} of ${reliability.cases}`],
    ["Recovered after retry", `${reliability.recovered} of ${reliability.cases}`],
    ["Still failing", `${reliability.failed} of ${reliability.cases}`],
    ["Failed attempts (all rounds)", String(reliability.failedAttempts)],
  ]);

  let fileInput = $state<HTMLInputElement>();
  let uploadError = $state<string | null>(null);
  const progressPercent = $derived(
    $total > 0 ? Math.round(($completed / $total) * 100) : 0,
  );
  const idle = $derived(
    !$running && !$transportError && !$aggregates && $results.every((result) => result === null),
  );

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
    store.dispatch(runEvaluation(parsed.testCases));
  }

  function criterionDetails(scores: EvaluationJudgeScores, key: keyof EvaluationJudgeScores) {
    return scores[key];
  }

  function summaryCriterion(scores: EvaluationJudgeScores, key: keyof EvaluationJudgeScores) {
    const criterion = scores[key];
    return Number.isFinite(criterion?.score) ? criterion : null;
  }

  function summaryScoreAriaLabel(scores: EvaluationJudgeScores) {
    return `Judge scores: ${criteria
      .map(([key, label]) => {
        const criterion = summaryCriterion(scores, key);
        return `${label} ${criterion ? criterion.score.toFixed(2) : "not scored"}`;
      })
      .join(", ")}`;
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
  <div
    class={idle
      ? "flex flex-1 flex-col items-center justify-center gap-3"
      : "flex shrink-0 flex-col items-center gap-3 py-3"}
  >
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
    {#if idle}
      <p class="text-sm text-muted-foreground" role="status">
        Add a tests JSON file to start an evaluation.
      </p>
    {/if}
    {#if uploadError}
      <p class="rounded-lg bg-destructive/10 p-3 text-sm text-destructive" role="alert">
        {uploadError}
      </p>
    {/if}
  </div>

  {#if !idle}
  <div class="min-h-0 flex-1 overflow-y-auto pb-5">
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
            {#if $retryRound > 0}
              Retry round {$retryRound} of 2 · {$retryCompleted}/{$retryPending} failed tests retried
            {:else}
              {$completed}/{$total}
            {/if}{#if $currentCaseIndex} · test {$currentCaseIndex}{/if}
          </span>
        </div>
        <div
          class="h-2 overflow-hidden rounded-full bg-muted"
          role="progressbar"
          aria-label="Evaluation progress"
          aria-valuemin={0}
          aria-valuemax={$total}
          aria-valuenow={$completed}
          aria-valuetext={$retryRound > 0 ? `Initial pass complete. Retry round ${$retryRound} of 2, ${$retryCompleted} of ${$retryPending} failed tests retried` : `${$completed} of ${$total} tests completed`}
        >
          <div class="h-full bg-primary transition-all" style={`width: ${progressPercent}%`}></div>
        </div>
      </section>
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
      <section class="mb-4" aria-labelledby="runtime-totals-title">
        <h2 id="runtime-totals-title" class="text-lg font-semibold">Runtime totals</h2>
        <p class="my-2 text-xs text-muted-foreground">
          Metrics available for {metricTotals.measuredCases} of {metricTotals.cases} current case results.
          Totals use each case's latest cumulative snapshot once, including failed attempts; pending retries may change them.
          Active latency is summed work, not wall-clock duration. Missing values are unavailable; known subtotals are partial.
        </p>
        <dl class="grid gap-2 sm:grid-cols-2 lg:grid-cols-4">
          {#each metricSummary as [label, value]}
            <div class="rounded-lg border bg-card p-3">
              <dt class="text-xs text-muted-foreground">{label}</dt>
              <dd class="mt-1 font-mono text-sm">{value}</dd>
            </div>
          {/each}
        </dl>
      </section>
      <section class="mb-4" aria-labelledby="performance-totals-title">
        <h2 id="performance-totals-title" class="text-lg font-semibold">Performance</h2>
        <p class="my-2 text-xs text-muted-foreground">
          Cases compared against their latency, call, retrieval, follow-up, token and cost limits (judge work excluded).
          Cases with unreported values or unfinished work are incomplete, never passed.
        </p>
        <dl class="grid gap-2 sm:grid-cols-2 lg:grid-cols-4">
          {#each performanceSummary as [label, value]}
            <div class="rounded-lg border bg-card p-3">
              <dt class="text-xs text-muted-foreground">{label}</dt>
              <dd class="mt-1 font-mono text-sm">{value}</dd>
            </div>
          {/each}
        </dl>
      </section>
      <section class="mb-4" aria-labelledby="reliability-totals-title">
        <h2 id="reliability-totals-title" class="text-lg font-semibold">Reliability</h2>
        <p class="my-2 text-xs text-muted-foreground">
          Based on each case's latest result; pending retries may change these values.
        </p>
        <dl class="grid gap-2 sm:grid-cols-2 lg:grid-cols-4">
          {#each reliabilitySummary as [label, value]}
            <div class="rounded-lg border bg-card p-3">
              <dt class="text-xs text-muted-foreground">{label}</dt>
              <dd class="mt-1 font-mono text-sm">{value}</dd>
            </div>
          {/each}
        </dl>
      </section>
      <section aria-labelledby="case-results-title">
        <h2 id="case-results-title" class="mb-3 text-lg font-semibold">Test results</h2>
        <div class="space-y-2">
          {#each $results as result, index}
            {#if result}
              {@const performance = sanitizeEvaluationPerformance(result.performance)}
              <details class="rounded-lg border bg-card" data-case-index={result.case_index}>
                <summary class="flex items-center justify-between gap-3 px-4 py-3 text-sm font-medium">
                  <span class="min-w-0 flex-1">
                    <span class="block truncate">Test {index + 1}: {result.input.question}</span>
                    {#if result.error}
                      <span class="block truncate text-xs font-normal text-muted-foreground">
                        No judge scores available — evaluation failed.
                      </span>
                      <span class="block text-xs font-normal text-destructive">{failureSummary(result.error)}</span>
                    {:else if !result.judge_scores}
                      <span class="block truncate text-xs font-normal text-muted-foreground">
                        No judge scores available yet.
                      </span>
                    {:else}
                      <span
                        class="block max-w-full overflow-x-auto whitespace-nowrap text-xs font-normal"
                        aria-label={summaryScoreAriaLabel(result.judge_scores)}
                      >
                        {#each criteria as [key, label], scoreIndex}
                          {@const criterion = summaryCriterion(result.judge_scores, key)}
                          <span class="whitespace-nowrap">
                            {label}:
                            <span style={criterion ? `color: ${evaluationScoreColor(criterion.score)}` : undefined}>
                              {criterion ? criterion.score.toFixed(2) : "not scored"}
                            </span>{#if scoreIndex < criteria.length - 1} · {/if}
                          </span>
                        {/each}
                      </span>
                    {/if}
                  </span>
                  <span class="flex flex-col items-end text-right">
                    <span class={result.error ? "text-destructive" : "text-muted-foreground"}>
                      {result.error ? "Failed" : "Completed"}
                    </span>
                    {#if performance}
                      <span class="text-xs font-normal {performance.status === 'fail' ? 'text-destructive' : 'text-muted-foreground'}">
                        Performance: {performance.status === "pass" ? "within limits" : performance.status === "fail" ? "over limit" : "incomplete"}
                      </span>
                    {/if}
                  </span>
                </summary>
                <div class="space-y-4 border-t px-4 py-3 text-sm">
                  <EvaluationRuntimeMetrics metrics={result.metrics} performance={result.performance} />
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
                      <p class="mt-1">{failureSummary(result.error)}</p>
                      <p class="mt-1">{result.error.message}</p>
                    </section>
                  {/if}
                  {#if result.attempt_history.length > 0}
                    <details class="rounded-md border p-3">
                      <summary class="cursor-pointer font-medium">Failed attempt history ({result.attempt_history.length})</summary>
                      <ol class="mt-2 space-y-3" aria-label={`Failed attempts for test ${result.case_index}`}>
                        {#each result.attempt_history as attempt}
                          <li>
                            <p class="font-medium">{attempt.retry_round === 0 ? "Initial pass" : `Retry round ${attempt.retry_round}`} · stage attempt {attempt.stage_attempt}</p>
                            <p>{failureSummary(attempt)}</p>
                            <p class="text-muted-foreground">{attempt.message}</p>
                          </li>
                        {/each}
                      </ol>
                    </details>
                  {/if}
                </div>
              </details>
            {/if}
          {/each}
        </div>
      </section>
    {/if}
  </div>
  {/if}
</div>