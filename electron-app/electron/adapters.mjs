import { publicError } from './backend.mjs';
import { requireValid, object, array, nonempty, text, uint, positive, nullable, oneOf,
  sources, chatResult, indexResult, evaluationResult, caseResult, testCase, progress } from './validation.mjs';

export const MAX_PDF_BYTES = 64 * 1024 * 1024;
export const MAX_PDF_CHUNK = 64 * 1024;
export class PdfAssembler {
  constructor(id) { this.id = id; this.chunks = []; this.size = 0; }
  event(name, value) {
    requireValid(name === 'pdf_chunk');
    const chunk = object({ offset: uint, bytes: array((v) => {
      uint(v); requireValid(v <= 255); return v;
    }, MAX_PDF_CHUNK) })(value);
    requireValid(chunk.offset === this.size && chunk.bytes.length > 0 && this.size + chunk.bytes.length <= MAX_PDF_BYTES);
    this.chunks.push(Buffer.from(chunk.bytes));
    this.size += chunk.bytes.length;
  }
  result(value) {
    const info = object({ id: nonempty, filename: nonempty, mime_type: oneOf('application/pdf'), total_bytes: positive })(value);
    requireValid(info.id === this.id && info.total_bytes === this.size && this.size <= MAX_PDF_BYTES);
    return { id: info.id, filename: info.filename, mime_type: info.mime_type,
      bytes: [...Buffer.concat(this.chunks, this.size)] };
  }
  clear() { this.chunks = []; this.size = 0; }
}

const subqueries = array(object({ id: nonempty, index: (v) => { positive(v); requireValid(v <= 7); return v; }, query: nonempty }), 7);
const subquery = { id: nonempty, index: positive, total: positive, query: nonempty };
const chatEvents = {
  plan: object({ subqueries }),
  subquery: object(subquery),
  subanswer: object({ ...subquery, answer: text, sources }),
  coverage: object({ status: oneOf('reviewing', 'complete', 'followup'), total: positive, subqueries }),
  delta: object({ delta: text }),
};
const indexEvents = {
  start: object(progress),
  started: object({ ...progress, paper_id: nonempty, filename: nonempty }),
  progress: object({ ...progress, paper_id: nonempty, filename: nonempty,
    status: nonempty, chunk_count: uint, error: nullable(text) }),
};
const evalEvents = {
  run_start: object({ retrieval_type: oneOf('hybrid'), total: positive }),
  case_start: object({ case_index: positive, total: positive, retrieval_type: oneOf('hybrid'), input: testCase }),
  case_success: object({ ...progress, result: caseResult }),
  case_failure: object({ ...progress, result: caseResult }),
};
const camel = (name) => name.replace(/_([a-z])/g, (_, c) => c.toUpperCase());
function uiEvent(name, data) {
  // Only envelope fields change case. Nested evaluation/provenance DTOs stay snake_case.
  return { event: camel(name), ...Object.fromEntries(Object.entries(data).map(([key, value]) => [camel(key), value])) };
}
export function parseEvent(command, name, value) {
  const shape = command === 'chat_stream' ? chatEvents : command === 'index_papers_stream' ? indexEvents :
    command === 'evaluate_stream' ? evalEvents : {};
  requireValid(Object.hasOwn(shape, name));
  const result = shape[name](value);
  if (command === 'chat_stream') {
    if (result.total !== undefined) requireValid(result.total <= 7);
    if (result.index !== undefined) requireValid(result.index <= result.total && result.index <= 7);
    if (name === 'plan') requireValid(result.subqueries.length >= 2 && result.subqueries.length <= 5);
    return { event: name, ...result };
  }
  if (result.completed !== undefined) requireValid(result.completed <= result.total);
  if (result.case_index !== undefined) requireValid(result.case_index <= result.total);
  if (result.error !== undefined && result.error !== null) result.error = 'Paper indexing failed.';
  if (name === 'case_success' || name === 'case_failure') {
    requireValid(result.result.case_index === result.completed);
    requireValid((result.result.error === null) === (name === 'case_success'));
    if (name === 'case_success') requireValid(result.result.generated_answer !== null && result.result.judge_scores !== null);
  }
  return uiEvent(name, result);
}

export function terminalEvent(command, value) {
  if (command === 'chat_stream') {
    const result = chatResult(value);
    return { event: 'done', reply: result.reply, chatMode: result.chat_mode, indexed: result.indexed, sources: result.sources };
  }
  if (command === 'index_papers_stream') {
    const result = indexResult(value);
    requireValid(result.completed === result.total && result.indexed + result.empty + result.failed + result.skipped === result.total);
    return { event: 'done', ...result };
  }
  if (command === 'evaluate_stream') {
    const result = evaluationResult(value);
    requireValid(result.completed === result.total && result.succeeded + result.failed === result.total);
    if (result.succeeded === 0) requireValid(Object.values(result.aggregate_scores).every((v) => v === null));
    return uiEvent('complete', result);
  }
  throw publicError('protocol');
}