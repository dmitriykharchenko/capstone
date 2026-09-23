import assert from 'node:assert/strict';
import { cp, mkdtemp, readFile, readdir, realpath, readlink, rm, stat } from 'node:fs/promises';
import { dirname, isAbsolute, join, relative, resolve } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { createHash } from 'node:crypto';
import { getRawHeader, extractAll, extractFile } from '@electron/asar';
import { pdfjsAssets } from './pdfjs-assets.mjs';
import { nativePackageSmoke } from '../tests/helpers/native-package-smoke.mjs';
import { launchPackagedSmoke } from '../tests/helpers/packaged-launch.mjs';

assert.equal(process.platform, 'darwin', 'macOS package smoke only');
assert.ok(process.argv.slice(2).every((arg) => arg === '--launch'), 'only --launch is supported');
const source = fileURLToPath(new URL('..', import.meta.url));
const app = join(source, 'release', process.arch === 'arm64' ? 'mac-arm64' : 'mac', 'Research Assistant.app');
const root = await realpath(await mkdtemp(join(tmpdir(), 'capstone-package-smoke-')));
const copy = join(root, 'Research Assistant.app');
const digest = (bytes) => createHash('sha256').update(bytes).digest('hex');
try {
  await cp(app, copy, { recursive: true, verbatimSymlinks: true });
  async function checkBundle(directory) {
    for (const entry of await readdir(directory, { withFileTypes: true })) {
      const path = join(directory, entry.name);
      if (entry.isSymbolicLink()) {
        const target = await readlink(path), resolved = relative(copy, resolve(dirname(path), target));
        assert.ok(!isAbsolute(target) && !resolved.startsWith('..') && !isAbsolute(resolved), 'bundle link escapes package');
      } else if (entry.isDirectory()) await checkBundle(path);
      else assert.ok(!/(?:\.py[co]?|\.sqlite3?|\.db|\.env(?:\..*)?|\.pem|\.key)$/i.test(entry.name), 'forbidden runtime/data file');
    }
  }
  await checkBundle(copy);
  const resources = join(copy, 'Contents/Resources'), archive = join(resources, 'app.asar');
  const entries = [];
  function walk(node, prefix = '') {
    for (const [name, value] of Object.entries(node.files)) {
      assert.ok(!value.link && !value.unpacked, 'application archive cannot have external links or unpacked files');
      if (value.files) walk(value, `${prefix}${name}/`);
      else entries.push(`${prefix}${name}`);
    }
  }
  walk(getRawHeader(archive).header);
  const modules = (await readdir(join(source, 'electron'))).filter((name) => /\.(mjs|cjs)$/.test(name));
  const assets = await pdfjsAssets();
  const allowed = new Set(['package.json', 'dist/index.html', ...modules.map((name) => `electron/${name}`),
    ...assets.map((asset) => `dist/${asset.fileName}`)]);
  for (const name of entries) assert.ok(allowed.has(name) || /^dist\/assets\/index-[\w-]+\.(js|css)$/.test(name), `Unexpected packaged path: ${name}`);
  for (const name of allowed) assert.ok(entries.includes(name), `Missing packaged path: ${name}`);
  for (const name of modules) assert.deepEqual(extractFile(archive, `electron/${name}`), await readFile(join(source, 'electron', name)));
  for (const asset of assets) assert.deepEqual(extractFile(archive, `dist/${asset.fileName}`), asset.source);
  const manifest = JSON.parse(extractFile(archive, 'package.json'));
  assert.equal(manifest.main, 'electron/main.mjs');
  assert.equal(manifest.type, 'module');
  const executable = join(resources, 'backend/research-backend');
  assert.ok((await stat(executable)).mode & 0o111);
  assert.deepEqual(await readFile(executable), await readFile(join(source, 'backend/target/release/research-backend')));
  const outside = await readdir(resources);
  assert.ok(outside.every((name) => name === 'app.asar' || name === 'backend' || name === 'electron.icns' || /^[a-zA-Z_]+\.lproj$/.test(name)));
  assert.deepEqual(await readdir(join(resources, 'backend')), ['research-backend']);
  const extracted = join(root, 'extracted');
  extractAll(archive, extracted);
  const { Backend } = await import(pathToFileURL(join(extracted, 'electron/backend.mjs')));
  const { Broker } = await import(pathToFileURL(join(extracted, 'electron/broker.mjs')));
  const { appResponse, csp } = await import(pathToFileURL(join(extracted, 'electron/security.mjs')));
  for (const asset of assets) {
    const response = await appResponse(new Request(`app://research/${asset.fileName}`), join(extracted, 'dist'));
    assert.equal(response.status, 200);
    assert.equal(response.headers.get('content-type'), asset.mime);
    assert.equal(response.headers.get('content-security-policy'), csp());
    assert.equal(response.headers.get('x-content-type-options'), 'nosniff');
    assert.deepEqual(Buffer.from(await response.arrayBuffer()), asset.source);
  }
  const native = await nativePackageSmoke({ root, executable, resources, Backend, Broker });
  const launch = process.argv.includes('--launch') ? await launchPackagedSmoke(copy, root) : 'not requested';
  console.log(JSON.stringify({ artifact: app, architecture: process.arch, archiveFiles: entries.length,
    pdfjsAssets: assets.length, archiveSha256: digest(await readFile(archive)),
    backendSha256: digest(await readFile(executable)), native, launch,
    limitations: ['unsigned and not notarized', 'no visual PDF/password acceptance', 'no live providers', 'macOS host architecture only'] }));
} finally { await rm(root, { recursive: true, force: true }); }