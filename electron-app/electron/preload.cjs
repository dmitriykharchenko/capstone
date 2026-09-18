// Sandboxed preload: Electron's built-in module only, no filesystem/process IPC surface.
const { contextBridge, ipcRenderer } = require('electron');
const active = new Map();
let sequence = 0;
const session = crypto.randomUUID();
ipcRenderer.on('research:event', (_event, token, value, seq) => {
  const callback = active.get(token);
  void ipcRenderer.invoke('research:ack', token, seq).catch(() => {});
  if (callback) {
    try { callback(value); }
    catch { active.delete(token); void ipcRenderer.invoke('research:cancel', token).catch(() => {}); }
  }
});
function request(method, args = {}, callback) {
  const token = `${session}-${++sequence}`;
  if (active.size >= 32) throw new Error('Too many active requests.');
  active.set(token, callback ?? null);
  const done = ipcRenderer.invoke('research:run', { token, method, args }).then((result) => {
    if (!result.ok) throw new Error(result.error);
    return result.data;
  }).finally(() => active.delete(token));
  return { done, cancel: () => {
    if (!active.has(token)) return;
    active.delete(token);
    void ipcRenderer.invoke('research:cancel', token).catch(() => {});
  } };
}
contextBridge.exposeInMainWorld('research', Object.freeze({
  health: () => request('health'), listPapers: () => request('listPapers'),
  getPaper: (paper_id) => request('getPaper', { paper_id }),
  readPdf: (paper_id) => request('readPdf', { paper_id }),
  chat: (session_id, message) => request('chat', { session_id, message }),
  chatStream: (session_id, message, callback) => request('chatStream', { session_id, message }, callback),
  listSessions: () => request('listSessions'),
  getMessages: (session_id) => request('getMessages', { session_id }),
  search: (query, limit, session_id) => request('search', { query, limit, session_id }),
  searchPapers: (query, limit) => request('searchPapers', { query, limit }),
  graphNeighborhood: (node_id, max_depth) => request('graphNeighborhood', { node_id, max_depth }),
  indexPapers: () => request('indexPapers'),
  indexPapersStream: (callback) => request('indexPapersStream', {}, callback),
  paperStatuses: () => request('paperStatuses'), bm25Status: () => request('bm25Status'),
  resetAllIndices: () => request('resetAllIndices'),
  runEvaluation: (test_cases, callback) => request('runEvaluation', { retrieval_type: 'hybrid', test_cases }, callback),
  toggleMaximize: () => ipcRenderer.invoke('research:toggle-maximize'),
}));
window.addEventListener('beforeunload', () => {
  active.clear();
  ipcRenderer.removeAllListeners('research:event');
  void ipcRenderer.invoke('research:cancel-all').catch(() => {});
});