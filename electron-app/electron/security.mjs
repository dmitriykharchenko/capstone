import { readFile, realpath } from 'node:fs/promises';
import { resolve, relative, extname, isAbsolute } from 'node:path';

export const APP_ORIGIN = 'app://research';
export const DEV_ORIGIN = 'http://127.0.0.1:5173';
export const webPreferences = Object.freeze({ contextIsolation: true, sandbox: true, nodeIntegration: false,
  nodeIntegrationInWorker: false, nodeIntegrationInSubFrames: false, webSecurity: true, webviewTag: false,
  plugins: true }); // Chromium's built-in PDF viewer; never external plugins/webviews.
export function trustedURL(value, dev = false) {
  try {
    const url = new URL(value);
    return !url.username && !url.password && (dev ? url.origin === DEV_ORIGIN :
      url.protocol === 'app:' && url.hostname === 'research' && !url.port);
  } catch { return false; }
}
export function assertSender(event, window, dev = false) {
  if (!window || window.isDestroyed() || event.sender !== window.webContents ||
      !event.senderFrame || event.senderFrame !== window.webContents.mainFrame ||
      !trustedURL(event.senderFrame.url, dev)) throw new Error('Desktop request denied.');
}
export function csp(dev = false) {
  return `default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; ` +
    `font-src 'self' data:; connect-src 'self'${dev ? ' ws://127.0.0.1:5173' : ''}; ` +
    `worker-src 'self'; frame-src blob:; object-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'`;
}
export function secureContents(contents, dev = false) {
  contents.setWindowOpenHandler(() => ({ action: 'deny' }));
  contents.on('will-navigate', (event, url) => { if (!trustedURL(url, dev)) event.preventDefault(); });
  contents.on('will-redirect', (event, url) => { if (!trustedURL(url, dev)) event.preventDefault(); });
  contents.on('will-frame-navigate', (event) => {
    const allowed = event.isMainFrame ? trustedURL(event.url, dev) :
      event.url.startsWith('blob:') && trustedURL(event.url.slice(5), dev);
    if (!allowed) event.preventDefault();
  });
  contents.on('will-attach-webview', (event) => event.preventDefault());
  contents.session.setPermissionRequestHandler((_contents, _permission, callback) => callback(false));
  contents.session.setPermissionCheckHandler(() => false);
  contents.session.on('will-download', (event) => event.preventDefault());
}

const MIME = { '.html': 'text/html', '.js': 'text/javascript', '.mjs': 'text/javascript', '.ttf': 'font/ttf', '.css': 'text/css', '.svg': 'image/svg+xml',
  '.png': 'image/png', '.woff2': 'font/woff2', '.woff': 'font/woff', '.ico': 'image/x-icon' };
export async function appResponse(request, dist) {
  if (request.method !== 'GET' || !trustedURL(request.url)) return new Response(null, { status: 403 });
  try {
    const path = decodeURIComponent(new URL(request.url).pathname);
    const spa = /^\/(?:chat(?:\/[^/]+)?|history|papers(?:\/[^/]+)?|graph|evals)?\/?$/.test(path);
    if (!spa && !path.startsWith('/assets/')) return new Response(null, { status: 404 });
    const root = await realpath(dist);
    const target = await realpath(resolve(root, spa ? 'index.html' : `.${path}`));
    const rel = relative(root, target);
    if (rel.startsWith('..') || isAbsolute(rel)) return new Response(null, { status: 403 });
    const bytes = await readFile(target);
    return new Response(bytes, { headers: { 'Content-Type': MIME[extname(target)] ?? 'application/octet-stream',
      'Content-Security-Policy': csp(), 'X-Content-Type-Options': 'nosniff' } });
  } catch { return new Response(null, { status: 404 }); }
}