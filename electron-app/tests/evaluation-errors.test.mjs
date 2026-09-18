import assert from 'node:assert/strict';
import test from 'node:test';
import { parseEvent } from '../electron/adapters.mjs';

// Independent expectations from the legacy CLI contract, not the implementation map.
const messages = {
  planner: 'Question planning failed for this case. Check backend configuration and retry.',
  retrieval: 'Retrieval failed for this case. Check the paper index and retry.',
  subanswer: 'Subanswer generation failed for this case. Check backend configuration and retry.',
  coverage: 'Coverage review failed for this case. Check backend configuration and retry.',
  followup_retrieval: 'Follow-up retrieval failed for this case. Check the paper index and retry.',
  followup_subanswer: 'Follow-up subanswer generation failed for this case. Check backend configuration and retry.',
  composer: 'Final answer composition failed for this case. Check backend configuration and retry.',
  model: 'Answer generation failed for this case. Check backend configuration and retry.',
  judge: 'Judge evaluation failed for this case. Check backend configuration and retry.',
  evaluation: 'Evaluation failed for this case. Check backend configuration and retry.',
  generation: 'Answer generation failed for this case. Check backend configuration and retry.',
  composition: 'Final answer composition failed for this case. Check backend configuration and retry.',
};
const sentinel = 'FABRICATED_NONSECRET_EXCEPTION_BODY_AND_PRIVATE_REASONING';
const protocolError = {
  code: 'protocol',
  message: 'The backend returned an invalid response. The operation outcome is unknown; check its state before retrying.',
};
function failure(error, caseIndex = 1) {
  return { completed: caseIndex, total: 2, result: {
    case_index: caseIndex,
    input: { question: `Question ${caseIndex}`, grading_notes: 'Reference', sources: ['paper.pdf'] },
    retrieval_strategy: 'hybrid',
    retrieved_chunks: [{ rank: 1, text: 'Full evidence', score: 0.75, paper_id: 'paper',
      chunk_index: 2, filename: 'paper.pdf', title: 'Paper title', page: 3 }],
    generated_answer: 'Answer with evidence', judge_scores: null, error,
  } };
}
function parseFailure(value) { return parseEvent('evaluate_stream', 'case_failure', value); }

for (const [stage, message] of Object.entries(messages)) {
  test(`evaluation failure preserves ${stage} with its fixed safe message`, () => {
    for (const raw of [sentinel, '', 'Already safe-looking but fabricated message']) {
      const value = failure({ stage, message: raw });
      const before = structuredClone(value);
      const parsed = parseFailure(value);
      assert.deepEqual(parsed, { event: 'caseFailure', ...value,
        result: { ...value.result, error: { stage, message } } });
      assert.equal(parsed.result.error.stage, stage);
      assert.equal(parsed.result.error.message, message);
      assert.equal(JSON.stringify(parsed).includes(sentinel), false);
      assert.deepEqual(value, before);
    }
  });
}

test('unknown stages including object prototype names use only the fixed evaluation fallback', () => {
  for (const stage of [sentinel, '__proto__', 'constructor', 'toString', 'Judge', ' judge ', 'unknown']) {
    const parsed = parseFailure(failure({ stage, message: sentinel }));
    assert.deepEqual(parsed.result.error, { stage: 'evaluation', message: messages.evaluation });
    assert.equal(JSON.stringify(parsed).includes(sentinel), false);
  }
});

test('malformed case errors are rejected with the fixed outer protocol error', () => {
  for (const error of [
    null, undefined, false, 1, sentinel, [], ['judge', sentinel], {},
    { stage: 'judge' }, { message: sentinel },
    ...[null, false, 1, [], {}, '', ' \n', 'x'.repeat(1_048_577)]
      .map((stage) => ({ stage, message: sentinel })),
    ...[null, undefined, false, 1, [], {}, 'x'.repeat(1_048_577)]
      .map((message) => ({ stage: 'judge', message })),
    { stage: 'judge', message: sentinel, private_reasoning: sentinel },
    { stage: 'unknown', message: sentinel, body: sentinel },
  ]) {
    assert.throws(() => parseFailure(failure(error)), protocolError);
  }
});

test('case ordering and strict event/result envelopes remain enforced', () => {
  const first = failure({ stage: 'planner', message: sentinel });
  const second = failure({ stage: 'judge', message: sentinel }, 2);
  const parsed = [first, second].map(parseFailure);
  assert.deepEqual(parsed.map((event) => [event.completed, event.result.case_index]), [[1, 1], [2, 2]]);
  assert.throws(() => parseFailure({ ...first, completed: 2 }), protocolError);
  assert.throws(() => parseFailure({ ...second, total: 1 }), protocolError);
  assert.throws(() => parseFailure({ ...first, error: sentinel }), protocolError);
  assert.throws(() => parseFailure({ ...first, result: { ...first.result, body: sentinel } }), protocolError);
  assert.throws(() => parseEvent('evaluate_stream', 'case_success', first), protocolError);
});

test('successful nested provenance and judge fields stay unchanged', () => {
  const value = failure(null);
  const criterion = { score: 4, confidence: 'high', reasoning: 'Public rubric assessment', flags: [] };
  value.result.judge_scores = {
    context_precision: { ...criterion }, context_recall: { ...criterion },
    relevant_chunk_ranking: { ...criterion, relevant_chunk_ranks: [1] },
    context_utilization: { ...criterion, chunks_used: [1, 'chunk two'] },
    semantic_similarity: { ...criterion, contradicting_claims: ['Example claim'] },
    completeness: { ...criterion, missing_elements: ['Example element'] },
    hallucination_rate: { ...criterion, unsupported_claims: ['Example unsupported claim'] },
  };
  const before = structuredClone(value);
  assert.deepEqual(parseEvent('evaluate_stream', 'case_success', value), { event: 'caseSuccess', ...before });
  assert.deepEqual(value, before);
});