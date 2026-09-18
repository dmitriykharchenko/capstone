// Sandboxed preload: Electron's built-in module only, no filesystem/process IPC surface.
const { contextBridge, ipcRenderer } = require('electron');
const active = new Map();
let sequence = 0;
let disposed = false;
const session = crypto.randomUUID();
function cancel(token) {
  const job = active.get(token);
  if (!job || job.cancelled) return;
  job.callback = null;
  job.cancelled = true;
  void ipcRenderer.invoke('research:cancel', token).catch(() => {});
}
ipcRenderer.on('research:event', (_event, token, value, seq) => {
  const callback = active.get(token)?.callback;
  void ipcRenderer.invoke('research:ack', token, seq).catch(() => {});
  if (callback) {
    try { callback(value); }
    catch { cancel(token); }
  }
});
function request(method, args = {}, callback) {
  if (disposed) throw new Error('The desktop view is closed.');
  const token = `${session}-${++sequence}`;
  if (active.size >= 32) throw new Error('Too many active requests.');
  active.set(token, { callback: callback ?? null, cancelled: false });
  const done = ipcRenderer.invoke('research:run', { token, method, args }).then((result) => {
    if (!result.ok) throw new Error(result.error);
    return result.data;
  }, () => {
    throw new Error('The backend connection closed. The operation outcome is unknown; check its state before retrying. No operation was replayed.');
  }).finally(() => active.delete(token));
  return { done, cancel: () => cancel(token) };
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
  disposed = true;
  for (const job of active.values()) { job.callback = null; job.cancelled = true; }
  ipcRenderer.removeAllListeners('research:event');
  void ipcRenderer.invoke('research:cancel-all').catch(() => {});
});