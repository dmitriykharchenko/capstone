import { app, BrowserWindow, ipcMain, protocol } from 'electron';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { Backend } from './backend.mjs';
import { Broker } from './broker.mjs';
import { configureDesktopData } from './paths.mjs';
import { APP_ORIGIN, DEV_ORIGIN, webPreferences, assertSender, secureContents, appResponse, csp } from './security.mjs';

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const dev = !app.isPackaged && process.argv.includes('--dev');
protocol.registerSchemesAsPrivileged([{ scheme: 'app', privileges: { standard: true, secure: true,
  supportFetchAPI: true, stream: true } }]);
app.enableSandbox();
let window;
let backend;
let quitting = false;

try {
  configureDesktopData(app, process.env);
  if (!app.requestSingleInstanceLock()) app.quit();
  else {
    app.on('second-instance', () => { if (window && !window.isDestroyed()) { window.restore(); window.focus(); } });
    // Electron waits for ESM entry evaluation before ready; never await readiness here.
    void app.whenReady().then(initialize).catch(startupFailed);
  }
} catch { startupFailed(); }

function startupFailed() {
  console.error('Application startup failed.');
  app.quit();
}

function initialize() {
  // Trusted main, not backend cwd (userData) or renderer input, supplies roots.
  // Packaged PDFs must be bundled under resources/ResearchPapers or explicitly
  // configured. Missing/invalid roots leave browsing unavailable; never fallback.
  const resources = app.isPackaged ? process.resourcesPath : dirname(root);
  const data = app.getPath('userData');
  const env = { ...process.env, CAPSTONE_APP_DATA_ROOT: data, CAPSTONE_RESOURCES_DIR: resources,
    CAPSTONE_PAPERS_DIR: process.env.CAPSTONE_PAPERS_DIR ?? join(resources, 'ResearchPapers') };
  backend = new Backend({ executable: app.isPackaged ? join(process.resourcesPath, 'backend', 'research-backend') :
    join(root, 'backend', 'target', 'debug', 'research-backend'), cwd: data, env });
  app.on('before-quit', (event) => {
    if (quitting) return;
    event.preventDefault();
    quitting = true;
    void backend.stop().finally(() => app.quit());
  });
  // Developer launcher and isolated package smoke use the same graceful drain.
  for (const signal of ['SIGINT', 'SIGTERM']) process.once(signal, () => app.quit());
  const broker = new Broker(backend);
  let generation = 0;
  protocol.handle('app', (request) => appResponse(request, join(root, 'dist')));
  ipcMain.handle('research:run', (event, payload) => {
    assertSender(event, window, dev);
    const frame = event.senderFrame;
    const epoch = generation;
    return broker.run(event.sender.id, payload, (token, value, seq) => {
      if (epoch === generation && !event.sender.isDestroyed() && event.sender.mainFrame === frame) frame.send('research:event', token, value, seq);
    });
  });
  ipcMain.handle('research:ack', (event, token, seq) => { assertSender(event, window, dev); broker.acknowledge(event.sender.id, token, seq); });
  ipcMain.handle('research:cancel', (event, token) => { assertSender(event, window, dev); broker.cancel(event.sender.id, token); });
  ipcMain.handle('research:cancel-all', (event) => { assertSender(event, window, dev); broker.cancelOwner(event.sender.id); });
  ipcMain.handle('research:toggle-maximize', (event) => {
    assertSender(event, window, dev);
    if (window.isMaximized()) window.unmaximize(); else window.maximize();
  });
  function createWindow() {
    window = new BrowserWindow({ width: 1280, height: 850, minWidth: 760, minHeight: 500,
      title: 'Research assistant', titleBarStyle: 'hiddenInset', backgroundColor: '#111111',
      webPreferences: { ...webPreferences, preload: join(root, 'electron', 'preload.cjs') } });
    const contents = window.webContents;
    secureContents(contents, dev);
    if (dev) contents.session.webRequest.onHeadersReceived((details, callback) => {
      callback({ responseHeaders: { ...details.responseHeaders, 'Content-Security-Policy': [csp(true)] } });
    });
    contents.on('did-start-navigation', (_event, _url, inPlace, mainFrame) => {
      if (mainFrame && !inPlace) { generation++; broker.cancelOwner(contents.id); }
    });
    contents.on('render-process-gone', () => broker.cancelOwner(contents.id));
    contents.on('destroyed', () => broker.cancelOwner(contents.id));
    void window.loadURL(dev ? DEV_ORIGIN : `${APP_ORIGIN}/chat`).catch(startupFailed);
  }
  createWindow();
  app.on('activate', () => { if (BrowserWindow.getAllWindows().length === 0) createWindow(); });
  app.on('window-all-closed', () => app.quit());
}