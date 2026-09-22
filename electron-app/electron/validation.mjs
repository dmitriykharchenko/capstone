import { publicError } from './backend.mjs';

export function requireValid(condition) { if (!condition) throw publicError('protocol'); }
export const text = (v) => { requireValid(typeof v === 'string' && v.length <= 1_048_576); return v; };
export const nonempty = (v) => { text(v); requireValid(v.trim().length > 0); return v; };
export const bool = (v) => { requireValid(typeof v === 'boolean'); return v; };
export const number = (v) => { requireValid(typeof v === 'number' && Number.isFinite(v)); return v; };
export const uint = (v) => { requireValid(Number.isSafeInteger(v) && v >= 0); return v; };
export const positive = (v) => { uint(v); requireValid(v > 0); return v; };
export const oneOf = (...values) => (v) => { requireValid(values.includes(v)); return v; };
export const optional = (parse) => (v) => v === undefined ? undefined : parse(v);
export const nullable = (parse) => (v) => v === null ? null : parse(v);
export const array = (parse, max = 10000) => (v) => {
  requireValid(Array.isArray(v) && v.length <= max); return v.map(parse);
};
export const object = (shape, strict = true) => (v) => {
  requireValid(v !== null && typeof v === 'object' && !Array.isArray(v));
  if (strict) requireValid(Object.keys(v).every((key) => Object.hasOwn(shape, key)));
  return Object.fromEntries(Object.entries(shape).map(([key, parse]) => [key, parse(v[key])])
    .filter(([, value]) => value !== undefined));
};
export const empty = object({});
export const identifier = (v) => {
  nonempty(v); requireValid(v.length <= 256 && !/[\x00-\x1f\x7f]/.test(v)); return v;
};
export const paperId = (v) => {
  identifier(v); requireValid(v !== '.' && v !== '..' && !/[/\\:]/.test(v)); return v;
};
const limit = (v) => { positive(v); requireValid(v <= 100); return v; };
export const testCase = object({ question: nonempty, grading_notes: nonempty, sources: array(nonempty, 1000) });
const cases = (v) => { const result = array(testCase, 1000)(v); requireValid(result.length > 0); return result; };
const chatArgs = object({ session_id: identifier, message: nonempty });

// Renderer methods are a closed allowlist. There is no raw command/path/env API.
export const methods = Object.freeze({
  health: ['health', empty], listPapers: ['list_papers', empty],
  getPaper: ['get_paper', object({ paper_id: paperId })],
  readPdf: ['read_pdf', object({ paper_id: paperId })],
  chat: ['chat', chatArgs], chatStream: ['chat_stream', chatArgs],
  listSessions: ['list_sessions', empty],
  getMessages: ['get_messages', object({ session_id: identifier })],
  search: ['search', object({ query: nonempty, limit: optional(limit), session_id: optional(identifier) })],
  searchPapers: ['search_papers', object({ query: nonempty, limit: optional(limit) })],
  graphNeighborhood: ['graph_neighborhood', object({ node_id: identifier, max_depth: oneOf(1, 2) })],
  indexPapers: ['index_papers', object({ reindex: optional(bool) })],
  indexPapersStream: ['index_papers_stream', object({ reindex: optional(bool) })],
  paperStatuses: ['paper_statuses', empty], bm25Status: ['bm25_status', empty],
  resetAllIndices: ['reset_all_indices', empty],
  runEvaluation: ['evaluate_stream', object({ retrieval_type: oneOf('hybrid'), test_cases: cases })],
});

export const source = object({ rank: positive, paper_id: nonempty, chunk_index: uint,
  filename: nonempty, title: text, page: uint, score: number, snippet: text });
export const sources = array(source, 35);
export const paper = object({ id: paperId, filename: nonempty, title: text, size_bytes: uint });
export const chatResult = object({ session_id: identifier, reply: text, chat_mode: nonempty, indexed: bool, sources });
export const progress = { completed: uint, total: uint };
export const indexResult = object({ ...progress, indexed: uint, empty: uint, failed: uint, skipped: uint });
export const score = (v) => { number(v); requireValid(v >= 0 && v <= 5); return v; };
const criterion = { score, confidence: oneOf('high', 'medium', 'low'), reasoning: text, flags: array(text) };
const scoreNames = ['context_precision', 'context_recall', 'relevant_chunk_ranking', 'context_utilization',
  'semantic_similarity', 'completeness', 'hallucination_rate'];
export const aggregateScores = object(Object.fromEntries(scoreNames.map((name) => [name, nullable(score)])));
const judges = object({
  context_precision: object(criterion), context_recall: object(criterion),
  relevant_chunk_ranking: object({ ...criterion, relevant_chunk_ranks: array(positive, 35) }),
  context_utilization: object({ ...criterion, chunks_used: array((v) => typeof v === 'string' ? text(v) : positive(v), 35) }),
  semantic_similarity: object({ ...criterion, contradicting_claims: array(text) }),
  completeness: object({ ...criterion, missing_elements: array(text) }),
  hallucination_rate: object({ ...criterion, unsupported_claims: array(text) }),
});
const caseErrorMessages = Object.freeze({
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
  // Retain compatibility stage labels with the model/composer safe messages.
  generation: 'Answer generation failed for this case. Check backend configuration and retry.',
  composition: 'Final answer composition failed for this case. Check backend configuration and retry.',
});
const caseError = (v) => {
  const parsed = object({ stage: nonempty, message: text })(v);
  const stage = Object.hasOwn(caseErrorMessages, parsed.stage) ? parsed.stage : 'evaluation';
  return { stage, message: caseErrorMessages[stage] };
};
export const caseResult = object({ case_index: positive, input: testCase, retrieval_strategy: oneOf('hybrid'),
  retrieved_chunks: array(object({ rank: positive, text, score: number, paper_id: nonempty, chunk_index: uint,
    filename: nonempty, title: text, page: uint }), 35), generated_answer: nullable(text),
  judge_scores: nullable(judges), error: nullable(caseError) });
export const evaluationResult = object({ retrieval_type: oneOf('hybrid'), ...progress,
  succeeded: uint, failed: uint, aggregate_scores: aggregateScores });

export const indexReadiness = oneOf('ready', 'recovering', 'recovery_required', 'rebuild_required', 'unavailable');
export const indexFailure = oneOf('PDF text could not be extracted.',
  'PDF changed during indexing. Run indexing again.', 'Embedding or metadata preparation failed.',
  'Paper exceeds the indexing safety limits. Prior publication was preserved.');
export const indexOutcome = oneOf('indexed', 'empty', 'skipped', 'failed');
const status = object({ paper_id: paperId, filename: nonempty,
  status: oneOf('indexed', 'empty', 'stale', 'not_indexed'), chunk_count: uint,
  version_id: nullable(nonempty), attempt_scope: oneOf('session'),
  attempt: nullable(object({ status: oneOf('preparing', 'interrupted', 'indexed', 'empty', 'skipped', 'failed'),
    error: nullable(indexFailure), observed_at: nonempty })) });
const graph = object({ enabled: bool, available: bool, query: oneOf('neighborhood'), reason: optional(nonempty),
  neighborhood: object({ nodes: array(object({ node_id: nonempty, kind: oneOf('paper', 'author', 'topic'), label: nonempty })),
    edges: array(object({ source_id: nonempty, target_id: nonempty, kind: oneOf('authored_by', 'has_topic') })) }) });
const responses = {
  list_papers: array(paper), get_paper: paper, chat: chatResult,
  list_sessions: object({ sessions: array(object({ session_id: identifier, title: text, message_count: uint, exchange_count: uint })) }),
  get_messages: object({ session_id: identifier, messages: array(object({ role: oneOf('user', 'assistant', 'system'),
    content: text, exchange_id: optional(uint), sources: optional(sources) })) }),
  search: object({ enabled: bool, reason: optional(text), matches: array(object({ exchange_id: uint,
    session_id: identifier, user_message: text, assistant_message: text })) }),
  search_papers: object({ enabled: bool, reason: optional(text), matches: array(object({ paper_id: paperId,
    title: text, filename: nonempty, page: nullable(uint), snippet: text, score: number })) }),
  graph_neighborhood: graph,
  index_papers: object({ enabled: oneOf(true), indexed: uint, summary: indexResult }),
  paper_statuses: object({ readiness: indexReadiness, statuses: array(status) }),
  bm25_status: object({ readiness: indexReadiness, built: bool, chunk_count: uint, paper_ids: array(paperId) }),
  reset_all_indices: object({ ok: oneOf(true), reset: object({ vector: oneOf(true), bm25: oneOf(true), statuses: oneOf(true), graph: oneOf(true) }),
    preserved: object({ pdfs: oneOf(true), history: oneOf(true) }) }),
  health: object({ ok: bool, protocol_version: oneOf(1), backend: oneOf('rust'), stage: nonempty,
    chat_mode: nonempty, search_enabled: bool, commands: array(object({ command: nonempty, owner: nonempty,
      streaming: bool, available: bool }), 64) }, false),
};

export function parseResponse(command, value) {
  requireValid(Object.hasOwn(responses, command));
  const result = responses[command](value);
  if (command === 'paper_statuses') {
    requireValid(result.readiness === 'ready' || result.statuses.length === 0);
    requireValid(new Set(result.statuses.map(p => p.paper_id)).size === result.statuses.length);
    result.statuses.forEach((item) => {
      requireValid((item.status === 'not_indexed') === (item.version_id === null));
      if (['empty', 'not_indexed'].includes(item.status)) requireValid(item.chunk_count === 0);
      if (item.status === 'indexed') requireValid(item.chunk_count > 0);
      if (item.attempt) requireValid((item.attempt.status === 'failed') === (item.attempt.error !== null));
    });
  }
  if (command === 'bm25_status') {
    requireValid(result.built === (result.readiness === 'ready'));
    requireValid(result.built || (result.chunk_count === 0 && result.paper_ids.length === 0));
    requireValid(new Set(result.paper_ids).size === result.paper_ids.length);
    requireValid(result.chunk_count >= result.paper_ids.length);
  }
  if (command === 'index_papers') {
    const s = result.summary;
    requireValid(s.completed === s.total && s.indexed + s.empty + s.failed + s.skipped === s.total && result.indexed === s.indexed);
  }
  return result;
}