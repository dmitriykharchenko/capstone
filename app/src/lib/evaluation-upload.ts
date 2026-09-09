import type { EvaluationTestCase } from "./evaluation-types";

export type EvaluationUploadResult =
  | { ok: true; testCases: EvaluationTestCase[] }
  | { ok: false; error: string };

function isNonEmptyString(value: unknown): value is string {
  return typeof value === "string" && value.trim().length > 0;
}

export function parseEvaluationTestCases(text: string): EvaluationUploadResult {
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch {
    return { ok: false, error: "The selected file is not valid JSON." };
  }
  if (!Array.isArray(value) || value.length === 0) {
    return { ok: false, error: "The JSON file must contain a non-empty array of tests." };
  }
  for (let index = 0; index < value.length; index += 1) {
    const item = value[index];
    if (item === null || typeof item !== "object" || Array.isArray(item)) {
      return { ok: false, error: `Test ${index + 1} must be an object.` };
    }
    const candidate = item as Record<string, unknown>;
    if (!isNonEmptyString(candidate.question)) {
      return { ok: false, error: `Test ${index + 1} must have a non-empty question.` };
    }
    if (!isNonEmptyString(candidate.grading_notes)) {
      return { ok: false, error: `Test ${index + 1} must have non-empty grading_notes.` };
    }
    if (
      !Array.isArray(candidate.sources) ||
      !candidate.sources.every(isNonEmptyString)
    ) {
      return {
        ok: false,
        error: `Test ${index + 1} sources must be an array containing only non-empty strings.`,
      };
    }
  }
  return { ok: true, testCases: value as EvaluationTestCase[] };
}