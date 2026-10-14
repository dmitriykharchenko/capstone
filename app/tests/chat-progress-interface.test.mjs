import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

async function readChatComponent() {
  return readFile(new URL("../src/components/Chat.svelte", import.meta.url), "utf8");
}

test("subquery evidence is collapsed by default", async () => {
  const component = await readChatComponent();
  const detailsTag = component.match(/<details[\s\S]*?>/)?.[0];
  assert.ok(detailsTag, "subquery details control");
  assert.doesNotMatch(detailsTag, /\sopen(?:\s|=|>)/);
  assert.match(component, /<details[\s\S]*?<summary[\s\S]*?Subquery answer[\s\S]*?Retrieved chunks[\s\S]*?<\/details>/);
});

test("each planner-ordered subquery owns an independent native toggle", async () => {
  const component = await readChatComponent();
  assert.match(component, /\{#each \$activeTurn\.subqueries as subquery \(subquery\.id\)\}[\s\S]*?<details/);
  assert.ok(component.includes("data-subquery-id={subquery.id}"));
  assert.doesNotMatch(component, /expandedSubquer|toggleSubquer|bind:open/);
});

test("Chat renders accessible ordered multihop and prominent final-answer progress", async () => {
  const component = await readChatComponent();
  for (const contract of [
    'aria-label="Multihop answer progress"',
    'aria-live="polite"',
    "Retrieval progress",
    "completedHops",
    "$activeTurn.total",
    'aria-label="Evidence coverage review status"',
    'role="status"',
    "Reviewing evidence coverage…",
    "Coverage review requested additional evidence.",
    "subquery.status",
    "subquery.answer",
    "Retrieved chunks",
    'aria-label="Final composed answer"',
    "$activeTurn.finalAnswer",
    'role="alert"',
    "Failed:",
  ]) assert.ok(component.includes(contract), contract);
  assert.match(component, /<Select[^>]+disabled=\{\$pending\}/);
  assert.ok((component.match(/disabled=\{\$pending/g) ?? []).length >= 3);
});
