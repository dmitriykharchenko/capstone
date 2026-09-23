import { createServer } from 'node:http';

// Loopback-only disposable provider. Never logs requests, headers or credentials.
export async function fakeProvider() {
  const counts = { embedding: 0, model: 0, stream: 0 };
  const server = createServer(async (request, response) => {
    try {
      let body = '';
      for await (const chunk of request) {
        body += chunk;
        if (body.length > 1_048_576) throw new Error('Fixture request too large');
      }
      const input = JSON.parse(body);
      response.setHeader('content-type', 'application/json');
      if (request.url === '/v1/embeddings') {
        counts.embedding++;
        response.end(JSON.stringify({ data: input.input.map((_, index) => ({ index, embedding: [0.25, 0.75] })) }));
        return;
      }
      if (request.url !== '/v1/chat/completions') { response.writeHead(404).end(); return; }
      counts.model++;
      const system = input.messages.filter((m) => m.role === 'system').map((m) => m.content).join('\n');
      let text;
      if (system.includes('Extract bibliographic metadata')) {
        text = JSON.stringify({ title: 'Synthetic PDF sample', authors: ['Fixture Author'], topics: ['testing', 'systems', 'retrieval'] });
      } else if (system.includes('MULTIHOP_PLANNER')) {
        text = JSON.stringify({ subqueries: ['What does the synthetic sample say?', 'What evidence supports the sample?'] });
      } else if (system.includes('MULTIHOP_COVERAGE')) {
        text = JSON.stringify({ complete: true, follow_up_subqueries: [] });
      } else if (system.includes('MULTIHOP_SUBANSWER') || system.includes('MULTIHOP_COMPOSER')) {
        text = 'The synthetic PDF sample provides fixture evidence.';
      } else {
        const scores = Object.fromEntries(['context_precision', 'context_recall', 'relevant_chunk_ranking',
          'context_utilization', 'semantic_similarity', 'completeness', 'hallucination_rate']
          .map((key) => [key, { score: 4, confidence: 'high', reasoning: 'Fixture evidence.', flags: [] }]));
        for (const [key, field] of [['relevant_chunk_ranking', 'relevant_chunk_ranks'], ['context_utilization', 'chunks_used'],
          ['semantic_similarity', 'contradicting_claims'], ['completeness', 'missing_elements'], ['hallucination_rate', 'unsupported_claims']]) scores[key][field] = [];
        text = JSON.stringify(scores);
      }
      if (input.stream) {
        counts.stream++;
        response.setHeader('content-type', 'text/event-stream');
        response.end(`data: ${JSON.stringify({ choices: [{ index: 0, delta: { content: text }, finish_reason: null }] })}\n\n` +
          `data: ${JSON.stringify({ choices: [{ index: 0, delta: {}, finish_reason: 'stop' }] })}\n\ndata: [DONE]\n\n`);
      } else response.end(JSON.stringify({ choices: [{ index: 0, message: { role: 'assistant', content: text }, finish_reason: 'stop' }] }));
    } catch { response.writeHead(500).end(); }
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  return { counts, url: `http://127.0.0.1:${server.address().port}/v1`,
    close: () => new Promise((resolve) => { server.close(resolve); server.closeAllConnections(); }) };
}