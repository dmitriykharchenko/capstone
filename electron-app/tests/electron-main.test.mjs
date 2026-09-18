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
async function host({ locked = true } = {}) {
  const app = new EventEmitter();
  let quits = 0;
  let sandbox = false;
  let backend;
  let window;
  const jobs = [];
  Object.assign(app, { isPackaged: false, requestSingleInstanceLock: () => locked, quit: () => quits++,
    whenReady: async () => {}, enableSandbox: () => { sandbox = true; }, getPath: () => '/tmp/fake-user-data' });
  const handlers = new Map();
  const protocol = { registerSchemesAsPrivileged: (schemes) => { protocol.schemes = schemes; },
    handle: (name, fn) => { protocol.name = name; protocol.handler = fn; } };
  class FakeBackend {
    constructor(options) { backend = this; this.options = options; this.stopped = false; }
    request(command, params, options) {
      const job = { command, params, options, cancelled: false };
      job.done = new Promise((resolve, reject) => { job.resolve = resolve; job.reject = reject; });
      job.cancel = () => { job.cancelled = true; job.reject(Object.assign(new Error('cancelled'), { code: 'cancelled' })); };
      jobs.push(job); return job;
    }
    async stop() { this.stopped = true; }
  }
  class FakeWindow {
    constructor(options) {
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
    async loadURL(url) { this.url = url; this.webContents.mainFrame.url = url; }
    isMaximized() { return !!this.maximized; }
    maximize() { this.maximized = true; }
    unmaximize() { this.maximized = false; }
    restore() {}
    focus() {}
  }
  const url = new URL('../electron/main.mjs', import.meta.url);
  const source = (await readFile(url, 'utf8')).replace(/^import .*;$/gm, '').replaceAll('import.meta.url', JSON.stringify(url.href));
  await vm.runInNewContext(`(async () => {${source}\n})()`, { app, BrowserWindow: FakeWindow,
    ipcMain: { handle: (name, fn) => handlers.set(name, fn) }, protocol, Backend: FakeBackend, Broker,
    ...security, dirname, join, fileURLToPath, process: { argv: [], env: {}, resourcesPath: '/tmp/fake-resources' } });
  const event = () => ({ sender: window.webContents, senderFrame: window.webContents.mainFrame });
  return { app, window, backend, jobs, handlers, protocol, event, quits: () => quits, sandbox: () => sandbox };
}

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

test('reload, renderer crash and destruction cancel pending streams without replay', async () => {
  const h = await host();
  for (const type of ['reload', 'render-process-gone', 'destroyed']) {
    const result = h.handlers.get('research:run')(h.event(), { token: type, method: 'chatStream', args: { session_id: 's', message: '?' } });
    const job = h.jobs.at(-1);
    if (type === 'reload') h.window.webContents.emit('did-start-navigation', {}, h.window.url, false, true);
    else h.window.webContents.emit(type);
    assert.equal(job.cancelled, true);
    assert.equal((await result).ok, false);
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
  const h = await host({ locked: false });
  assert.equal(h.quits(), 1);
  assert.equal(h.backend, undefined);
  assert.equal(h.window, undefined);
});