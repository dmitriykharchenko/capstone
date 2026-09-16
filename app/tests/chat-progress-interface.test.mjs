import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

test("Chat renders accessible ordered multihop and final-answer progress", async () => {
  const component = await readFile(
    new URL("../src/components/Chat.svelte", import.meta.url),
    "utf8",
  );
  for (const contract of [
    'aria-label="Multihop answer progress"',
    'aria-live="polite"',
    "Retrieval plan",
    "subquery.status",
    "subquery.answer",
    "subquery.sources.length",
    'aria-label="Final composed answer"',
    "$activeTurn.finalAnswer",
    'role="alert"',
    "Failed:",
  ]) assert.ok(component.includes(contract), contract);
  assert.match(component, /<Select[^>]+disabled=\{\$pending\}/);
  assert.ok((component.match(/disabled=\{\$pending/g) ?? []).length >= 3);
});