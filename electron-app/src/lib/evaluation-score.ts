const SCORE_MIN = 0;
const SCORE_MAX = 5;

export function clampEvaluationScore(score: number): number {
  if (!Number.isFinite(score)) return SCORE_MIN;
  return Math.min(SCORE_MAX, Math.max(SCORE_MIN, score));
}

export function evaluationScoreColor(score: number): string {
  const ratio = clampEvaluationScore(score) / SCORE_MAX;
  const red = Math.round(220 + (22 - 220) * ratio);
  const green = Math.round(38 + (163 - 38) * ratio);
  const blue = Math.round(38 + (74 - 38) * ratio);
  return `rgb(${red} ${green} ${blue})`;
}