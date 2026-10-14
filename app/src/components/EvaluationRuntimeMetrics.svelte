<script lang="ts">
  import type { EvaluationMetrics, EvaluationPerformance, EvaluationPerformanceCheckName } from "$lib/evaluation-types";
  import { formatCost, formatLatency, formatMetric, formatMetricTotal, formatPerformanceValue, metricStageLabel, performanceCheckLabel, sanitizeEvaluationMetrics, sanitizeEvaluationPerformance } from "$lib/evaluation-metrics";

  let { metrics, performance }: { metrics?: EvaluationMetrics | null; performance?: EvaluationPerformance | null } = $props();
  const measured = $derived(sanitizeEvaluationMetrics(metrics));
  const checked = $derived(sanitizeEvaluationPerformance(performance));
  const checkRows = $derived(checked
    ? Object.entries(checked.checks).map(([name, check]) => [name as EvaluationPerformanceCheckName, check] as const)
    : []);
  const runtime = $derived(measured ? [
    ["Active latency", formatLatency(measured.latency_ms.total)],
    ["RAG latency", formatLatency(measured.latency_ms.rag)],
    ["Judge latency", formatLatency(measured.latency_ms.judge)],
    ["Model calls (all)", formatMetric(measured.model_calls.total)],
    ["RAG / judge / embedding calls", [measured.model_calls.rag, measured.model_calls.judge, measured.model_calls.embedding].map(formatMetric).join(" / ")],
    ["RAG exchanges", formatMetric(measured.workflow.exchanges)],
    ["Retrieval tool calls", formatMetric(measured.workflow.tool_calls)],
    ...(measured.workflow.reused_searches === undefined ? [] : [["Reused searches", formatMetric(measured.workflow.reused_searches)]]),
  ] : []);
  const context = $derived(measured ? [
    ["Chunk occurrences", measured.context.chunks], ["Unique chunks", measured.context.unique_chunks],
    ["Evidence characters", measured.context.characters], ["Evidence UTF-8 bytes", measured.context.utf8_bytes],
    ["Max chunks per call", measured.context.max_chunks], ["Max evidence characters per call", measured.context.max_characters],
    ["Full RAG prompt characters", measured.context.prompt_characters], ["Max prompt characters per call", measured.context.max_prompt_characters],
  ] as const : []);
</script>

<section aria-label="Runtime and workflow metrics">
  <h3 class="font-medium">Runtime and workflow</h3>
  {#if measured}
    <p class="mt-1 text-xs text-muted-foreground">
      Cumulative active work, including failed attempts and retries; excludes deferred-round waits and UI pauses.
      Model calls are application-level attempts (including judge and embeddings); SDK-internal retries are not observable.
      Exchanges are returned RAG model responses; tool calls are retrieval attempts.
    </p>
    <dl class="mt-2 grid gap-2 sm:grid-cols-2 lg:grid-cols-4">
      {#each runtime as [label, value]}
        <div class="rounded-md bg-muted/60 p-3"><dt class="text-xs text-muted-foreground">{label}</dt><dd class="mt-1 font-mono">{value}</dd></div>
      {/each}
    </dl>
    <h4 class="mt-3 font-medium">Tokens and cost</h4>
    <dl class="mt-2 grid gap-2 sm:grid-cols-2 lg:grid-cols-4">
      {#each ["input", "output", "total"] as kind}
        {@const key = kind as "input" | "output" | "total"}
        <div class="rounded-md bg-muted/60 p-3">
          <dt class="text-xs capitalize text-muted-foreground">{kind} tokens</dt>
          <dd class="mt-1 font-mono">{formatMetricTotal({ value: measured.tokens[key], known: measured.tokens[`known_${key}`] })}</dd>
        </div>
      {/each}
      <div class="rounded-md bg-muted/60 p-3">
        <dt class="text-xs text-muted-foreground">Cached input tokens</dt>
        <dd class="mt-1 font-mono">{formatMetric(measured.tokens.cached_input)}</dd>
      </div>
      <div class="rounded-md bg-muted/60 p-3">
        <dt class="text-xs text-muted-foreground">Estimated cost (USD)</dt>
        <dd class="mt-1 font-mono">{formatMetricTotal({ value: measured.cost.estimated_usd, known: measured.cost.known_usd }, formatCost)}</dd>
      </div>
    </dl>
    <p class="mt-2 text-xs text-muted-foreground">
      Token usage reported for {formatMetric(measured.tokens.reported_calls)} of {formatMetric(measured.model_calls.total)} model calls.
      Cost available for {formatMetric(measured.cost.priced_calls)} calls:
      {formatMetric(measured.cost.provider_reported_calls)} provider-reported, {formatMetric(measured.cost.rate_estimated_calls)} rate-estimated.
      Known subtotals are partial, not complete totals. Tokens and cost include judge and embedding calls.
      Cached input is provider-reported; an unavailable value does not mean zero.
    </p>
    <h4 class="mt-3 font-medium">RAG context</h4>
    <p class="mt-1 text-xs text-muted-foreground">
      Raw evidence actually sent to RAG subanswers, cumulative across retries; unique by paper and chunk index.
      Evidence sizes include formatting. Full prompts are measured separately. Judge context is only in the stage breakdown.
    </p>
    <dl class="mt-2 grid gap-2 sm:grid-cols-2 lg:grid-cols-4">
      {#each context as [label, value]}
        <div class="rounded-md bg-muted/60 p-3"><dt class="text-xs text-muted-foreground">{label}</dt><dd class="mt-1 font-mono">{formatMetric(value)}</dd></div>
      {/each}
    </dl>
    {#if Object.keys(measured.stages).length > 0}
      <details class="mt-3 rounded-md border p-3">
        <summary class="cursor-pointer font-medium">Stage breakdown</summary>
        <div class="mt-2 overflow-x-auto">
          <table class="w-full text-left text-xs">
            <caption class="sr-only">Runtime, usage, and context by stage, including separate judge context</caption>
            <thead><tr>{#each ["Stage", "Active latency", "Model calls", "Exchanges", "Tool calls", "Tokens", "Cached input", "Estimated USD", "Chunks / unique", "Evidence chars / bytes", "Max chunks / chars", "Prompt chars / max"] as label}<th scope="col" class="whitespace-nowrap p-2">{label}</th>{/each}</tr></thead>
            <tbody>
              {#each Object.entries(measured.stages) as [name, stage]}
                <tr class="border-t">
                  <th scope="row" class="p-2">{metricStageLabel(name)}</th>
                  <td class="p-2">{formatLatency(stage.latency_ms)}</td><td class="p-2">{formatMetric(stage.model_calls)}</td>
                  <td class="p-2">{formatMetric(stage.exchanges)}</td><td class="p-2">{formatMetric(stage.tool_calls)}</td>
                  <td class="p-2">{formatMetricTotal({ value: stage.tokens.total, known: stage.tokens.known_total })}</td>
                  <td class="p-2">{formatMetric(stage.tokens.cached_input)}</td>
                  <td class="p-2">{formatMetricTotal({ value: stage.cost.estimated_usd, known: stage.cost.known_usd }, formatCost)}</td>
                  <td class="p-2">{formatMetric(stage.context.chunks)} / {formatMetric(stage.context.unique_chunks)}</td>
                  <td class="p-2">{formatMetric(stage.context.characters)} / {formatMetric(stage.context.utf8_bytes)}</td>
                  <td class="p-2">{formatMetric(stage.context.max_chunks)} / {formatMetric(stage.context.max_characters)}</td>
                  <td class="p-2">{formatMetric(stage.context.prompt_characters)} / {formatMetric(stage.context.max_prompt_characters)}</td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
      </details>
    {/if}
  {:else}
    <p class="mt-1 text-muted-foreground">Unavailable — this result has no supported runtime metrics.</p>
  {/if}
  <h4 class="mt-3 font-medium">Performance checks</h4>
  {#if checked}
    <p class="mt-1 text-xs text-muted-foreground">
      Overall: {checked.status === "pass" ? "within all limits" : checked.status === "fail" ? "over at least one limit" : "incomplete — some values are unreported or the case did not finish"}.
      Judge work is excluded; retried work counts again.
    </p>
    <div class="mt-2 overflow-x-auto">
      <table class="w-full text-left text-xs">
        <caption class="sr-only">Performance checks against limits</caption>
        <thead><tr>{#each ["Check", "Value", "Limit", "Result"] as label}<th scope="col" class="whitespace-nowrap p-2">{label}</th>{/each}</tr></thead>
        <tbody>
          {#each checkRows as [name, check]}
            <tr class="border-t">
              <th scope="row" class="p-2">{performanceCheckLabel(name)}</th>
              <td class="p-2 font-mono">{formatPerformanceValue(name, check.value)}</td>
              <td class="p-2 font-mono">{formatPerformanceValue(name, check.limit)}</td>
              <td class="p-2 {check.status === 'fail' ? 'text-destructive' : ''}">{check.status === "pass" ? "Pass" : check.status === "fail" ? "Fail" : "Unknown"}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
  {:else}
    <p class="mt-1 text-muted-foreground">Unavailable — this result has no performance checks.</p>
  {/if}
</section>