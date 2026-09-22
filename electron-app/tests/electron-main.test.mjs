import assert from 'node:assert/strict';
import test from 'node:test';
import { readFile } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { EventEmitter } from 'node:events';
import vm from 'node:vm';
import { Broker } from '../electron/broker.mjs';
import * as security from '../electron/security.mjs';

// Execute the actual main module against a deterministic Electron host, not a browser.
const settle = () => new Promise((resolve) => setImmediate(resolve));
async function host({ locked = true, ready = true, failure, stopped = Promise.resolve(), packaged = false, env = {} } = {}) {
  const app = new EventEmitter();
  let quits = 0;
  let sandbox = false;
  let backend;
  let window;
  let evaluated = false;
  let resolveReady;
  let rejectReady;
  const readiness = new Promise((resolve, reject) => { resolveReady = resolve; rejectReady = reject; });
  const counts = { backend: 0, window: 0, ready: 0, protocol: 0, handlers: 0 };
  const jobs = [];
  const diagnostics = [];
  const fail = (stage) => { if (failure === stage) throw new Error('PRIVATE startup details'); };
  Object.assign(app, { isPackaged: packaged, requestSingleInstanceLock: () => locked,
    quit: () => {
      let prevented = false;
      app.emit('before-quit', { preventDefault() { prevented = true; } });
      if (!prevented) quits++;
    },
    whenReady: () => { counts.ready++; return readiness; },
    enableSandbox: () => { sandbox = true; }, getPath: () => '/tmp/fake-user-data' });
  const handlers = new Map();
  const protocol = { registerSchemesAsPrivileged: (schemes) => { protocol.schemes = schemes; },
    handle: (name, fn) => { fail('protocol'); counts.protocol++; protocol.name = name; protocol.handler = fn; } };
  class FakeBackend {
    constructor(options) { fail('backend'); counts.backend++; backend = this; this.options = options; this.stopped = false; }
    request(command, params, options) {
      const job = { command, params, options, cancelled: false };
      job.done = new Promise((resolve, reject) => { job.resolve = resolve; job.reject = reject; });
      job.cancel = () => { job.cancelled = true; };
      jobs.push(job); return job;
    }
    stop() { this.stopped = true; return stopped; }
  }
  class FakeWindow {
    constructor(options) {
      fail('window');
      counts.window++;
      window = this; this.options = options;
      const contents = this.webContents = new EventEmitter();
      contents.id = 10;
      contents.mainFrame = { url: '', send() {} };
      contents.isDestroyed = () => false;
      contents.setWindowOpenHandler = () => {};
      contents.session = new EventEmitter();
      contents.session.setPermissionRequestHandler = () => {};
      contents.session.setPermissionCheckHandler = () => {};
    }
    static getAllWindows() { return window ? [window] : []; }
    isDestroyed() { return false; }
    async loadURL(url) { fail('load'); this.url = url; this.webContents.mainFrame.url = url; }
    isMaximized() { return !!this.maximized; }
    maximize() { this.maximized = true; }
    unmaximize() { this.maximized = false; }
    restore() {}
    focus() {}
  }
  const url = new URL('../electron/main.mjs', import.meta.url);
  const source = (await readFile(url, 'utf8')).replace(/^import .*;$/gm, '').replaceAll('import.meta.url', JSON.stringify(url.href));
  const evaluation = vm.runInNewContext(`(async () => {${source}\n})()`, { app, BrowserWindow: FakeWindow,
    ipcMain: { handle: (name, fn) => { counts.handlers++; handlers.set(name, fn); } }, protocol, Backend: FakeBackend, Broker,
    console: { error: (...args) => diagnostics.push(args) },
    ...security, dirname, join, fileURLToPath, process: { argv: [], env, resourcesPath: '/tmp/fake-resources' } });
  void evaluation.then(() => { evaluated = true; });
  if (ready) { resolveReady(); await evaluation; await settle(); }
  const event = () => ({ sender: window.webContents, senderFrame: window.webContents.mainFrame });
  return { app, get window() { return window; }, get backend() { return backend; }, jobs, handlers, protocol, event,
    counts, resolveReady, rejectReady, diagnostics, evaluated: () => evaluated, quits: () => quits, sandbox: () => sandbox };
}

test('trusted startup supplies deterministic browsing roots, never backend cwd or renderer paths', async () => {
  const repository = dirname(dirname(fileURLToPath(new URL('../electron/main.mjs', import.meta.url))));
  const dev = await host();
  assert.equal(dev.backend.options.cwd, '/tmp/fake-user-data');
  assert.equal(dev.backend.options.env.CAPSTONE_APP_DATA_ROOT, '/tmp/fake-user-data');
  assert.equal(dev.backend.options.env.CAPSTONE_RESOURCES_DIR, dirname(repository));
  assert.equal(dev.backend.options.env.CAPSTONE_PAPERS_DIR, join(dirname(repository), 'ResearchPapers'));
  const packaged = await host({ packaged: true });
  assert.equal(packaged.backend.options.env.CAPSTONE_RESOURCES_DIR, '/tmp/fake-resources');
  assert.equal(packaged.backend.options.env.CAPSTONE_PAPERS_DIR, '/tmp/fake-resources/ResearchPapers');
  assert.equal(packaged.backend.options.executable, '/tmp/fake-resources/backend/research-backend');
  for (const override of ['/tmp/approved-pdfs', '', 'relative-invalid']) {
    const custom = await host({ env: { CAPSTONE_PAPERS_DIR: override,
      CAPSTONE_APP_DATA_ROOT: '/wrong', CAPSTONE_RESOURCES_DIR: '/wrong' } });
    assert.equal(custom.backend.options.env.CAPSTONE_PAPERS_DIR, override, 'do not silently fallback');
    assert.equal(custom.backend.options.env.CAPSTONE_APP_DATA_ROOT, '/tmp/fake-user-data');
    assert.equal(custom.backend.options.env.CAPSTONE_RESOURCES_DIR, dirname(repository));
  }
});

test('entry evaluation finishes before readiness, then initializes exactly once', async () => {
  const h = await host({ ready: false });
  await settle();
  assert.equal(h.evaluated(), true, 'Electron cannot become ready until entry evaluation finishes');
  assert.equal(h.sandbox(), true);
  assert.equal(h.protocol.schemes[0].scheme, 'app');
  assert.equal(h.protocol.schemes[0].privileges.secure, true);
  assert.deepEqual(h.counts, { backend: 0, window: 0, ready: 1, protocol: 0, handlers: 0 });
  assert.equal(h.backend, undefined);
  assert.equal(h.window, undefined);
  assert.equal(h.handlers.size, 0);
  h.app.emit('activate');
  h.app.emit('second-instance');
  assert.equal(h.window, undefined);
  h.resolveReady();
  await settle();
  assert.deepEqual(h.counts, { backend: 1, window: 1, ready: 1, protocol: 1, handlers: 5 });
  h.resolveReady();
  h.app.emit('ready');
  h.app.emit('activate');
  h.app.emit('second-instance');
  await settle();
  assert.deepEqual(h.counts, { backend: 1, window: 1, ready: 1, protocol: 1, handlers: 5 });
  assert.equal(h.quits(), 0);
});

test('main creates one isolated window/backend and validates every privileged handler', async () => {
  const h = await host();
  assert.equal(h.sandbox(), true);
  assert.equal(h.window.options.webPreferences.contextIsolation, true);
  assert.equal(h.window.options.webPreferences.sandbox, true);
  assert.equal(h.window.options.webPreferences.nodeIntegration, false);
  assert.equal(h.window.url, 'app://research/chat');
  assert.equal(h.protocol.name, 'app');
  assert.match(h.backend.options.executable, /electron-app\/backend\/target\/debug\/research-backend$/);
  assert.equal(h.handlers.size, 5);
  for (const handler of h.handlers.values()) assert.throws(() => handler({ sender: {} }, {}), /denied/);
  h.handlers.get('research:toggle-maximize')(h.event());
  assert.equal(h.window.maximized, true);
  h.handlers.get('research:toggle-maximize')(h.event());
  assert.equal(h.window.maximized, false);
});

test('reload, renderer crash and destruction detach streams but retain authoritative results', async () => {
  const h = await host();
  for (const type of ['reload', 'render-process-gone', 'destroyed']) {
    const result = h.handlers.get('research:run')(h.event(), { token: type, method: 'chatStream', args: { session_id: 's', message: '?' } });
    const job = h.jobs.at(-1);
    let settled = false;
    void result.then(() => { settled = true; });
    if (type === 'reload') h.window.webContents.emit('did-start-navigation', {}, h.window.url, false, true);
    else h.window.webContents.emit(type);
    assert.equal(job.cancelled, true);
    await Promise.resolve();
    assert.equal(settled, false);
    for (let i = 0; i < 40; i++) job.options.onEvent('delta', { delta: 'late' });
    job.resolve({ session_id: 's', reply: 'committed', chat_mode: 'rag', indexed: true, sources: [] });
    assert.equal((await result).ok, true);
  }
  assert.equal(h.jobs.length, 3);
  let prevented = false;
  h.app.emit('before-quit', { preventDefault() { prevented = true; } });
  await Promise.resolve();
  assert.equal(prevented, true);
  assert.equal(h.backend.stopped, true);
  assert.equal(h.quits(), 1);
});

test('second app instance exits without constructing a backend/window', async () => {
  const h = await host({ locked: false, ready: false });
  await settle();
  assert.equal(h.evaluated(), true);
  assert.equal(h.quits(), 1);
  assert.equal(h.backend, undefined);
  assert.equal(h.window, undefined);
  h.resolveReady();
  h.app.emit('ready');
  h.app.emit('activate');
  await settle();
  assert.deepEqual(h.counts, { backend: 0, window: 0, ready: 0, protocol: 0, handlers: 0 });
  assert.equal(h.handlers.size, 0);
});

test('readiness failure quits with only a fixed startup diagnostic', async () => {
  const h = await host({ ready: false });
  h.rejectReady(new Error('PRIVATE readiness details'));
  await settle();
  assert.equal(h.evaluated(), true);
  assert.equal(h.quits(), 1);
  assert.deepEqual(h.counts, { backend: 0, window: 0, ready: 1, protocol: 0, handlers: 0 });
  assert.deepEqual(h.diagnostics, [['Application startup failed.']]);
});

for (const failure of ['backend', 'protocol', 'window', 'load']) {
  test(`${failure} startup failure is sanitized and drains any owned backend before exit`, async () => {
    let finishStop;
    const stopped = new Promise((resolve) => { finishStop = resolve; });
    const h = await host({ failure, stopped });
    assert.deepEqual(h.diagnostics, [['Application startup failed.']]);
    assert.equal(h.jobs.length, 0);
    if (failure === 'backend') {
      assert.equal(h.backend, undefined);
      assert.equal(h.window, undefined);
      assert.equal(h.handlers.size, 0);
      assert.equal(h.quits(), 1);
    } else {
      assert.equal(h.backend.stopped, true);
      assert.equal(h.quits(), 0, 'exit must wait for backend shutdown');
      finishStop();
      await settle();
      assert.equal(h.quits(), 1);
    }
  });
}