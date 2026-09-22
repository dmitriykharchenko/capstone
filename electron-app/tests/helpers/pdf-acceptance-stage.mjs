import { constants } from 'node:fs';
import { chmod, copyFile, lstat, mkdir, mkdtemp, open, readFile, readdir, realpath } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { basename, dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { syntheticPdf } from '../pdf-fixtures.mjs';
import { pdfjsAssets } from '../../scripts/pdfjs-assets.mjs';

export const source = fileURLToPath(new URL('../..', import.meta.url));
export const reviewed = '54a9cc3fbafd020af23af0cee577df6fc41bf6f9';
export const modules = ['main.mjs', 'backend.mjs', 'broker.mjs', 'security.mjs',
  'validation.mjs', 'adapters.mjs', 'preload.cjs'];
export const fixtures = Object.freeze({ 'Acceptance-Text.pdf': 'multipage',
  'Acceptance-Image.pdf': 'image', 'Acceptance-Blank.pdf': 'blank',
  'Acceptance-Protected.pdf': 'locked', 'Acceptance-Invalid.pdf': 'invalid' });
const directories = ['resources', 'resources/app', 'resources/ResearchPapers', 'private', 'home', 'app-data', 'profile',
  'session', 'tmp', 'crashes', 'logs', 'cache', 'config'];
const hash = (bytes) => createHash('sha256').update(bytes).digest('hex');
export function ensure(value) { if (!value) throw new Error('Acceptance preparation check failed.'); }
export function allowedInventoryName(name) {
  return modules.some((module) => name === `resources/app/electron/${module}`) ||
    name === 'resources/app/acceptance-entry.mjs' || name === 'resources/app/backend/target/debug/research-backend' ||
    name === 'resources/app/dist/index.html' || /^resources\/app\/dist\/assets\/(?:[A-Za-z0-9_.-]+\/)*[A-Za-z0-9_.-]+$/.test(name) &&
      !name.split('/').some((part) => part === '.' || part === '..') ||
    Object.keys(fixtures).some((file) => name === `resources/ResearchPapers/${file}`);
}
export async function exclusive(path, value, mode = 0o600) {
  const handle = await open(path, 'wx', mode);
  try { await handle.writeFile(value); } finally { await handle.close(); }
}
export function isolatedEnvironment(root) {
  // No parent environment spread, provider keys, proxies, NODE_OPTIONS or Electron overrides.
  return { PATH: '/usr/bin:/bin:/usr/sbin:/sbin', HOME: join(root, 'home'),
    CFFIXED_USER_HOME: join(root, 'home'), TMPDIR: join(root, 'tmp'),
    XDG_CONFIG_HOME: join(root, 'config'), XDG_CACHE_HOME: join(root, 'cache'),
    XDG_DATA_HOME: join(root, 'app-data'), LANG: 'en_US.UTF-8',
    CAPSTONE_APP_DATA_ROOT: join(root, 'profile'), CAPSTONE_RESOURCES_DIR: join(root, 'resources'),
    CAPSTONE_PAPERS_DIR: join(root, 'resources/ResearchPapers') };
}
export async function privateRoot() {
  const root = await realpath(await mkdtemp(join(tmpdir(), 'capstone-pdfjs-acceptance-')));
  await chmod(root, 0o700);
  for (const name of directories) await mkdir(join(root, name), { mode: 0o700 });
  return root;
}
export async function filesUnder(root, prefix = '') {
  const result = [];
  for (const entry of await readdir(join(root, prefix), { withFileTypes: true })) {
    const name = join(prefix, entry.name);
    ensure(!entry.isSymbolicLink());
    if (entry.isDirectory()) result.push(...await filesUnder(root, name));
    else { ensure(entry.isFile()); result.push(name); }
  }
  return result.sort();
}
export async function checkReviewedSource() {
  for (const name of modules) {
    const path = `electron-app/electron/${name}`;
    const original = execFileSync('/usr/bin/git', ['show', `${reviewed}:${path}`],
      { cwd: source, stdio: ['ignore', 'pipe', 'ignore'] });
    ensure(original.equals(await readFile(join(source, 'electron', name))));
    for (const match of original.toString().matchAll(/(?:from\s+|require\()['"]([^'"]+)['"]/g)) {
      ensure(match[1] === 'electron' || match[1].startsWith('node:') ||
        (match[1].startsWith('./') && modules.includes(match[1].slice(2))));
    }
  }
  execFileSync('/usr/bin/git', ['diff', '--exit-code', reviewed, '--', 'src', 'electron',
    'backend', 'scripts', 'package.json', 'package-lock.json', 'vite.config.ts', 'tsconfig.json'],
  { cwd: source, stdio: ['ignore', 'ignore', 'ignore'] });
}
async function copy(sourcePath, target, mode) {
  ensure((await lstat(sourcePath)).isFile());
  await mkdir(dirname(target), { recursive: true, mode: 0o700 });
  await copyFile(sourcePath, target, constants.COPYFILE_EXCL);
  await chmod(target, mode);
  ensure((await readFile(sourcePath)).equals(await readFile(target)));
}
export async function prepare() {
  await checkReviewedSource();
  const root = await privateRoot(), appRoot = join(root, 'resources/app');
  const inventory = [];
  const recordCopy = async (from, to, mode = 0o600) => {
    await copy(from, join(root, to), mode);
    inventory.push({ name: to, sha256: hash(await readFile(from)) });
  };
  for (const name of modules) await recordCopy(join(source, 'electron', name), `resources/app/electron/${name}`);
  const assets = await pdfjsAssets();
  const distFiles = await filesUnder(join(source, 'dist'));
  const assetNames = new Set(assets.map((asset) => asset.fileName));
  ensure(distFiles.includes('index.html') && distFiles.every((name) => name === 'index.html' ||
    assetNames.has(name) || /^assets\/index-[A-Za-z0-9_-]+\.(?:js|css)$/.test(name)));
  for (const asset of assets) ensure(asset.source.equals(await readFile(join(source, 'dist', asset.fileName))));
  for (const name of distFiles) await recordCopy(join(source, 'dist', name), `resources/app/dist/${name}`);
  await recordCopy(join(source, 'backend/target/debug/research-backend'), 'resources/app/backend/target/debug/research-backend', 0o700);
  await recordCopy(fileURLToPath(new URL('./pdf-acceptance-entry.mjs', import.meta.url)), 'resources/app/acceptance-entry.mjs');
  for (const [name, kind] of Object.entries(fixtures)) {
    const fixture = kind === 'invalid' ? { bytes: Buffer.from('%PDF-1.4\ninvalid\n%%EOF\n') } : syntheticPdf(kind);
    try {
      await exclusive(join(root, 'resources/ResearchPapers', name), fixture.bytes);
      if (kind === 'locked') await exclusive(join(root, 'private', 'protected-password.txt'), fixture.password + '\n');
      inventory.push({ name: `resources/ResearchPapers/${name}`, sha256: hash(fixture.bytes) });
    } finally { fixture.password = undefined; }
  }
  const electron = join(source, 'node_modules/electron/dist/Electron.app/Contents/MacOS/Electron');
  ensure((await lstat(electron)).isFile());
  const secret = await lstat(join(root, 'private/protected-password.txt'));
  const manifest = { version: 1, root, reviewed, electron, appRoot, inventory,
    passwordIdentity: { dev: secret.dev, ino: secret.ino },
    created: new Date().toISOString() };
  await exclusive(join(root, 'preparation.json'), JSON.stringify(manifest));
  await verify(root);
  return root;
}
export async function verify(root) {
  ensure(resolve(root) === await realpath(root));
  ensure(dirname(root) === await realpath(tmpdir()) && basename(root).startsWith('capstone-pdfjs-acceptance-'));
  const directory = await lstat(root);
  ensure(directory.isDirectory() && directory.uid === process.getuid() && (directory.mode & 0o777) === 0o700);
  const manifest = JSON.parse(await readFile(join(root, 'preparation.json'), 'utf8'));
  ensure(manifest.root === root && manifest.reviewed === reviewed && manifest.appRoot === join(root, 'resources/app'));
  ensure(manifest.inventory.every((item) => allowedInventoryName(item.name)));
  const allowed = new Set(manifest.inventory.map((item) => item.name));
  const actual = [...await filesUnder(join(root, 'resources/app'))].map((name) => `resources/app/${name}`)
    .concat((await filesUnder(join(root, 'resources/ResearchPapers'))).map((name) => `resources/ResearchPapers/${name}`));
  ensure(actual.length === allowed.size && actual.every((name) => allowed.has(name)));
  for (const item of manifest.inventory) {
    ensure(!relative(root, resolve(root, item.name)).startsWith('..'));
    const entry = await lstat(join(root, item.name));
    ensure(entry.isFile() && entry.uid === process.getuid() && (entry.mode & 0o077) === 0);
    ensure(hash(await readFile(join(root, item.name))) === item.sha256);
  }
  for (const name of directories) {
    const entry = await lstat(join(root, name));
    ensure(entry.isDirectory() && entry.uid === process.getuid() && (entry.mode & 0o777) === 0o700);
  }
  // Never open/read/hash the local password file. Only ownership, mode and inode metadata.
  const secret = await lstat(join(root, 'private/protected-password.txt'));
  ensure(secret.isFile() && secret.nlink === 1 && secret.uid === process.getuid() && (secret.mode & 0o777) === 0o600 &&
    secret.dev === manifest.passwordIdentity.dev && secret.ino === manifest.passwordIdentity.ino);
  const { appResponse, csp } = await import(pathToFileURL(join(root, 'resources/app/electron/security.mjs')));
  for (const name of actual.filter((name) => name.startsWith('resources/app/dist/'))) {
    const route = name === 'resources/app/dist/index.html' ? '/papers' : '/' + name.slice('resources/app/dist/'.length);
    const response = await appResponse(new Request(`app://research${route}`), join(root, 'resources/app/dist'));
    ensure(response.status === 200 && response.headers.get('content-security-policy') === csp() &&
      response.headers.get('x-content-type-options') === 'nosniff');
    ensure(Buffer.from(await response.arrayBuffer()).equals(await readFile(join(root, name))));
  }
  for (const asset of await pdfjsAssets()) {
    const response = await appResponse(new Request(`app://research/${asset.fileName}`), join(root, 'resources/app/dist'));
    ensure(response.headers.get('content-type') === asset.mime &&
      Buffer.from(await response.arrayBuffer()).equals(asset.source));
  }
  return manifest;
}
export async function nativePreflight(root) {
  await verify(root);
  const { Backend } = await import(pathToFileURL(join(root, 'resources/app/electron/backend.mjs')));
  const { Broker } = await import(pathToFileURL(join(root, 'resources/app/electron/broker.mjs')));
  const backend = new Backend({ executable: join(root, 'resources/app/backend/target/debug/research-backend'),
    cwd: join(root, 'profile'), env: isolatedEnvironment(root), requestTimeout: 5000 });
  const broker = new Broker(backend);
  let sequence = 0, child;
  const run = (method, args = {}) => broker.run(1, { token: `preflight-${++sequence}`, method, args }, () => {});
  try {
    const health = await run('health'); child = backend.child;
    ensure(health.ok && health.data.search_enabled === false);
    ensure(JSON.stringify(health.data.commands.filter((item) => item.available).map((item) => item.command)) ===
      JSON.stringify(['get_paper', 'health', 'list_papers', 'read_pdf']));
    const listed = await run('listPapers');
    ensure(listed.ok && JSON.stringify(listed.data.map((item) => item.filename).sort()) === JSON.stringify(Object.keys(fixtures).sort()));
    for (const [filename, kind] of Object.entries(fixtures)) {
      const paper_id = filename.slice(0, -4), info = await run('getPaper', { paper_id });
      const bytes = await readFile(join(root, 'resources/ResearchPapers', filename));
      ensure(info.ok && info.data.filename === filename && info.data.size_bytes === bytes.length);
      const result = await run('readPdf', { paper_id });
      ensure(kind === 'invalid' ? !result.ok && result.data === undefined :
        result.ok && result.data.mime_type === 'application/pdf' && Buffer.from(result.data.bytes).equals(bytes));
    }
    ensure(backend.pending.size === 0 && broker.requests.size === 0);
  } finally { await backend.stop(); }
  ensure(child && child.exitCode === 0 && backend.child === null);
  ensure((await readdir(join(root, 'profile'))).length === 0);
  return { category: 'native-preflight-passed', fixtures: Object.keys(fixtures).length,
    validTransfers: 4, backendExit: 0, profileEmpty: true };
}