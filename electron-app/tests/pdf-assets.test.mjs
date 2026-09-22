import assert from 'node:assert/strict';
import test from 'node:test';
import { build, createServer } from 'vite';
import { mkdtemp, readFile, rm, writeFile, symlink } from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import { pdfjsAssetPrefix, pdfjsAssets } from '../scripts/pdfjs-assets.mjs';
import { appResponse, csp, webPreferences } from '../electron/security.mjs';

const root = fileURLToPath(new URL('..', import.meta.url));
test('local asset inventory matches the installed runtime/worker and includes fallback licenses only', async () => {
  const assets = await pdfjsAssets();
  const names = assets.map((asset) => asset.fileName);
  for (const path of ['build/pdf.mjs', 'build/pdf.worker.mjs', 'cmaps/Adobe-Japan1-UCS2.bcmap',
    'standard_fonts/LiberationSans-Regular.ttf', 'standard_fonts/FoxitSerif.pfb',
    'wasm/openjpeg_nowasm_fallback.js', 'wasm/jbig2_nowasm_fallback.js', 'LICENSE',
    'cmaps/LICENSE', 'standard_fonts/LICENSE_FOXIT', 'standard_fonts/LICENSE_LIBERATION',
    'wasm/LICENSE_OPENJPEG', 'wasm/LICENSE_JBIG2']) {
    assert.ok(names.includes(pdfjsAssetPrefix.slice(1) + path), path);
  }
  assert.ok(names.every((name) => name.startsWith(pdfjsAssetPrefix.slice(1))));
  assert.ok(names.every((name) => !/\.wasm$|quickjs|sandbox|web\/|iccs\/|\.map$/.test(name)));
  const source = assets.filter((asset) => /\.m?js$/.test(asset.fileName)).map((asset) => asset.source.toString()).join('\n');
  assert.doesNotMatch(source, /\bnew Function\s*\(|\beval\s*\(/);
});

test('built PDF.js assets serve byte-exact with correct MIME and strict app containment', async (t) => {
  const temporary = await mkdtemp(join(tmpdir(), 'capstone-pdfjs-build-'));
  t.after(() => rm(temporary, { recursive: true, force: true }));
  const dist = join(temporary, 'dist');
  await build({ root, logLevel: 'silent', build: { outDir: dist, emptyOutDir: false, reportCompressedSize: false } });
  for (const asset of await pdfjsAssets()) {
    const emitted = await readFile(join(dist, asset.fileName));
    assert.deepEqual(emitted, asset.source, asset.fileName);
    const response = await appResponse(new Request(`app://research/${asset.fileName}`), dist);
    assert.equal(response.status, 200, asset.fileName);
    assert.equal(response.headers.get('Content-Type'), asset.mime, asset.fileName);
    assert.equal(response.headers.get('X-Content-Type-Options'), 'nosniff');
    assert.equal(response.headers.get('Content-Security-Policy'), csp());
    assert.deepEqual(Buffer.from(await response.arrayBuffer()), asset.source, asset.fileName);
  }
  await writeFile(join(temporary, 'outside'), 'synthetic outside marker');
  await symlink(join(temporary, 'outside'), join(dist, 'assets', 'escape'));
  for (const url of ['app://foreign/assets/file.mjs', 'app://research.evil/assets/file.mjs',
    'app://user@research/assets/file.mjs', 'https://research/assets/file.mjs',
    'app://research/electron/main.mjs', 'app://research/node_modules/pdfjs-dist/build/pdf.mjs',
    'app://research/assets/escape', 'app://research/assets/%2e%2e%2f%2e%2e%2foutside',
    `app://research${pdfjsAssetPrefix}../../%2e%2e%2foutside`]) {
    assert.notEqual((await appResponse({ url, method: 'GET' }, dist)).status, 200);
  }
  // Dist containment alone must not let encoded segments escape /assets/ or
  // create non-canonical aliases. Backslashes are separators on Windows too.
  for (const path of ['/assets/%2e%2e%2findex.html', '/assets/%2E.%2Findex.html',
    `${pdfjsAssetPrefix}%2e%2e%2f%2e%2e%2findex.html`,
    `${pdfjsAssetPrefix}build/%2e%2fpdf.worker.mjs`,
    '/assets/%2e%2e%5cindex.html', `${pdfjsAssetPrefix}%2e%2e%5c%2e%2e%5cindex.html`]) {
    const response = await appResponse({ url: `app://research${path}`, method: 'GET' }, dist);
    assert.equal(response.status, 403, path);
  }
  assert.equal((await appResponse(new Request(`app://research${pdfjsAssetPrefix}build/pdf.worker.mjs`,
    { method: 'POST' }), dist)).status, 403);
});

test('dev serves identical versioned assets without opening another filesystem route', async (t) => {
  const server = await createServer({ root, logLevel: 'silent', server: { port: 0, strictPort: false } });
  t.after(async () => { await server.waitForRequestsIdle(); await server.close(); });
  await server.listen();
  const origin = `http://127.0.0.1:${server.httpServer.address().port}`;
  for (const asset of await pdfjsAssets()) {
    const response = await fetch(`${origin}/${asset.fileName}`);
    assert.equal(response.status, 200, asset.fileName);
    assert.equal(response.headers.get('Content-Type'), asset.mime);
    assert.equal(response.headers.get('X-Content-Type-Options'), 'nosniff');
    assert.deepEqual(Buffer.from(await response.arrayBuffer()), asset.source, asset.fileName);
  }
  for (const path of ['../package.json', '%2e%2e%2fpackage.json', 'build/pdf.sandbox.mjs', 'wasm/quickjs-eval.wasm']) {
    const response = await fetch(`${origin}${pdfjsAssetPrefix}${path}`);
    // Vite may supply its HTML SPA fallback; it must never supply the requested asset.
    assert.ok(response.status !== 200 || response.headers.get('Content-Type')?.includes('text/html'));
  }
  for (const path of ['/electron/main.mjs', '/backend/src/main.rs', '/scripts/pdfjs-assets.mjs']) {
    assert.equal((await fetch(origin + path)).status, 403);
  }
});

test('CSP permits only self workers and keeps all existing sandbox/navigation restrictions', () => {
  for (const dev of [false, true]) {
    const policy = new Map(csp(dev).split('; ').map((value) => {
      const [key, ...parts] = value.split(' '); return [key, parts.join(' ')];
    }));
    assert.equal(policy.get('worker-src'), "'self'");
    assert.equal(policy.get('script-src'), "'self'");
    assert.equal(policy.get('object-src'), "'none'");
    assert.equal(policy.get('frame-ancestors'), "'none'");
    assert.equal(policy.get('frame-src'), 'blob:');
    assert.doesNotMatch(csp(dev), /unsafe-eval|wasm-unsafe-eval|https:|\*/);
  }
  assert.equal(webPreferences.sandbox, true);
  assert.equal(webPreferences.contextIsolation, true);
  assert.equal(webPreferences.webSecurity, true);
  assert.equal(webPreferences.nodeIntegrationInWorker, false);
});