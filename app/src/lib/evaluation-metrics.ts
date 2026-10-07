import type {
  EvaluationCaseResult,
  EvaluationMetrics,
  EvaluationPerformance,
  EvaluationPerformanceCheck,
  EvaluationPerformanceCheckName,
} from "./evaluation-types";

const tokenFields = ["known_input", "known_output", "known_total", "reported_calls"];
const costFields = ["known_usd", "priced_calls", "provider_reported_calls", "rate_estimated_calls"];
const contextFields = ["chunks", "unique_chunks", "duplicate_chunks", "characters", "utf8_bytes", "max_chunks", "max_characters", "prompt_characters", "max_prompt_characters"];
const stageLabels: Record<string, string> = {
  planner: "Planner", retrieval: "Retrieval", subanswer: "Subanswer", coverage: "Coverage",
  followup_retrieval: "Follow-up retrieval", followup_subanswer: "Follow-up subanswer",
  composer: "Composer", judge: "Judge",
};

function object(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown> : null;
}

function number(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value) && value >= 0;
}

function fields(value: unknown, required: string[], nullable: string[] = [], decimals: string[] = []) {
  const source = object(value);
  if (!source) return null;
  const entries: Array<[string, number | null]> = [];
  for (const key of [...required, ...nullable]) {
    const value = source[key];
    const validNumber = number(value) && (decimals.includes(key) || Number.isSafeInteger(value));
    if (!validNumber && !(nullable.includes(key) && value === null)) return null;
    entries.push([key, value as number | null]);
  }
  return Object.fromEntries(entries);
}

function usage(value: Record<string, unknown>) {
  const tokens = fields(value.tokens, tokenFields, ["input", "output", "total"]);
  const cost = fields(value.cost, costFields, ["estimated_usd"], ["known_usd", "estimated_usd"]);
  const context = fields(value.context, contextFields);
  return tokens && cost && context ? { tokens, cost, context } : null;
}

// Copy numeric contract fields only; legacy/unsupported metrics remain unavailable.
export function sanitizeEvaluationMetrics(value: unknown): EvaluationMetrics | null {
  const source = object(value);
  if (!source || source.version !== 1) return null;
  const common = usage(source);
  const latency_ms = fields(source.latency_ms, ["total", "rag", "judge"], [], ["total", "rag", "judge"]);
  const model_calls = fields(source.model_calls, ["total", "rag", "judge", "embedding"]);
  const workflow = fields(source.workflow, ["exchanges", "tool_calls", "reused_searches"]);
  const pricingFields = ["input_usd_per_million", "output_usd_per_million", "embedding_usd_per_million"];
  const pricing = fields(source.pricing, [], pricingFields, pricingFields);
  const rawStages = object(source.stages);
  if (!common || !latency_ms || !model_calls || !workflow || !pricing || !rawStages) return null;
  const stages: Record<string, unknown> = Object.create(null);
  for (const [name, value] of Object.entries(rawStages)) {
    if (!Object.hasOwn(stageLabels, name)) return null;
    const stage = object(value);
    if (!stage) return null;
    const commonStage = usage(stage);
    const counts = fields(stage, ["latency_ms", "model_calls", "exchanges", "tool_calls"], [], ["latency_ms"]);
    if (!commonStage || !counts) return null;
    stages[name] = { ...counts, ...commonStage };
  }
  return { version: 1, ...common, latency_ms, model_calls, workflow, pricing, stages } as unknown as EvaluationMetrics;
}

export interface EvaluationMetricTotal {
  value: number | null;
  known: number | null;
}

export function evaluationMetricTotals(results: ReadonlyArray<EvaluationCaseResult | null>) {
  // Replacement snapshots are cumulative. Never add an earlier result for the same case.
  const latest = new Map(results.filter((result) => result !== null).map((result) => [result.case_index, result]));
  const metrics = [...latest.values()].map((result) => sanitizeEvaluationMetrics(result.metrics));
  function sum(get: (metrics: EvaluationMetrics) => number | null, known = get): EvaluationMetricTotal {
    const values = metrics.map((metric) => metric ? get(metric) : null);
    const partial = metrics.map((metric) => metric ? known(metric) : null);
    const add = (values: Array<number | null>) => values.reduce<number>((total, value) => total + (value ?? 0), 0);
    return {
      value: values.length > 0 && values.every(number) ? add(values) : null,
      known: partial.some(number) ? add(partial) : null,
    };
  }
  return {
    cases: latest.size,
    measuredCases: metrics.filter((metric) => metric !== null).length,
    latency: sum((m) => m.latency_ms.total),
    modelCalls: sum((m) => m.model_calls.total),
    exchanges: sum((m) => m.workflow.exchanges),
    toolCalls: sum((m) => m.workflow.tool_calls),
    tokens: sum((m) => m.tokens.total, (m) => m.tokens.known_total),
    cost: sum((m) => m.cost.estimated_usd, (m) => m.cost.known_usd),
    chunks: sum((m) => m.context.chunks),
    characters: sum((m) => m.context.characters),
  };
}

export function formatMetric(value: number | null | undefined): string {
  return number(value) ? value.toLocaleString("en-US", { maximumFractionDigits: 2 }) : "Unavailable";
}

export function formatLatency(value: number | null | undefined): string {
  return number(value) ? `${(value / 1000).toLocaleString("en-US", { minimumFractionDigits: 2, maximumFractionDigits: 2 })} s` : "Unavailable";
}

export function formatCost(value: number | null | undefined): string {
  if (!number(value)) return "Unavailable";
  return value > 0 && value < 0.000001 ? "< $0.000001" : `$${value.toFixed(6)}`;
}

export function formatMetricTotal(total: EvaluationMetricTotal, format = formatMetric): string {
  return total.value !== null ? format(total.value)
    : total.known !== null ? `Unavailable (known subtotal: ${format(total.known)})` : "Unavailable";
}

export function metricStageLabel(stage: string): string {
  return Object.hasOwn(stageLabels, stage) ? stageLabels[stage] : "Other stage";
}

const performanceChecks: Record<EvaluationPerformanceCheckName, readonly [string, (value: number) => string]> = {
  rag_latency_ms: ["RAG latency", formatLatency],
  rag_model_calls: ["RAG model calls", formatMetric],
  retrievals: ["Retrievals", formatMetric],
  followup_queries: ["Follow-up queries", formatMetric],
  rag_tokens: ["RAG tokens", formatMetric],
  rag_cost_usd: ["RAG cost (USD)", formatCost],
  duplicate_chunks: ["Duplicate excerpts", formatMetric],
};

export function performanceCheckLabel(name: EvaluationPerformanceCheckName): string {
  return performanceChecks[name][0];
}

export function formatPerformanceValue(name: EvaluationPerformanceCheckName, value: number | null): string {
  return value === null ? "Unavailable" : performanceChecks[name][1](value);
}

// Mirrors PerformanceChecks::validate in app/src-tauri/src/evaluation_metrics.rs.
export function sanitizeEvaluationPerformance(value: unknown): EvaluationPerformance | null {
  const source = object(value);
  const rawChecks = object(source?.checks);
  if (!source || source.version !== 1 || !rawChecks) return null;
  const checks: Partial<Record<EvaluationPerformanceCheckName, EvaluationPerformanceCheck>> = {};
  for (const [name, raw] of Object.entries(rawChecks)) {
    const check = object(raw);
    if (!Object.hasOwn(performanceChecks, name) || !check || !number(check.limit)) return null;
    const measured = number(check.value) ? check.value : check.value === null ? null : undefined;
    if (measured === undefined) return null;
    const status = measured === null ? "unknown" : measured <= check.limit ? "pass" : "fail";
    if (check.status !== status) return null;
    checks[name as EvaluationPerformanceCheckName] = { value: measured, limit: check.limit, status };
  }
  const statuses = Object.values(checks).map((check) => check.status);
  const expected = statuses.includes("fail") ? ["fail"] : statuses.includes("unknown") ? ["incomplete"] : ["pass", "incomplete"];
  if (typeof source.status !== "string" || !expected.includes(source.status)) return null;
  return { version: 1, status: source.status as EvaluationPerformance["status"], checks };
}

export function evaluationReliabilityTotals(results: ReadonlyArray<EvaluationCaseResult | null>) {
  const latest = [...new Map(results.filter((result) => result !== null).map((result) => [result.case_index, result])).values()];
  const succeeded = latest.filter((result) => !result.error && result.judge_scores);
  const performance = latest.map((result) => sanitizeEvaluationPerformance(result.performance)).filter((item) => item !== null);
  const failedChecks = new Map<EvaluationPerformanceCheckName, number>();
  for (const item of performance) {
    for (const [name, check] of Object.entries(item.checks)) {
      if (check.status === "fail") {
        const key = name as EvaluationPerformanceCheckName;
        failedChecks.set(key, (failedChecks.get(key) ?? 0) + 1);
      }
    }
  }
  return {
    cases: latest.length,
    succeeded: succeeded.length,
    failed: latest.filter((result) => result.error).length,
    firstTry: succeeded.filter((result) => result.attempt_history.length === 0).length,
    recovered: succeeded.filter((result) => result.attempt_history.length > 0).length,
    failedAttempts: latest.reduce((total, result) => total + result.attempt_history.length, 0),
    performanceChecked: performance.length,
    performancePass: performance.filter((item) => item.status === "pass").length,
    performanceFail: performance.filter((item) => item.status === "fail").length,
    performanceIncomplete: performance.filter((item) => item.status === "incomplete").length,
    failedChecks: [...failedChecks.entries()].sort((a, b) => b[1] - a[1]),
  };
}