import assert from 'node:assert/strict';
import test from 'node:test';
import { spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { readFile, lstat } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { syntheticPdf } from './pdf-fixtures.mjs';
import { fixtures, isolatedEnvironment, privateRoot, exclusive, allowedInventoryName } from './helpers/pdf-acceptance-stage.mjs';
import { childOptions, claimSingleLaunch, descendants, sameProcess, safeReport, processRows, stopOwnedProcesses } from './helpers/pdf-acceptance.mjs';

test('generated filenames are safe IDs and fixtures contain no literal generated password', () => {
  for (const [name, kind] of Object.entries(fixtures)) {
    assert.ok(/^[A-Za-z0-9-]+\.pdf$/.test(name));
    if (kind === 'invalid') continue;
    const fixture = syntheticPdf(kind);
    try {
      assert.ok(fixture.bytes.length > 0);
      if (kind === 'locked') {
        assert.ok(typeof fixture.password === 'string' && fixture.password.length === 32);
        assert.ok(!Buffer.from(fixture.bytes).includes(Buffer.from(fixture.password)));
      }
    } finally { fixture.password = undefined; }
  }
});

test('multipage fixture has real distinct page text using PDF.js Node fake worker', async () => {
  await import('pdfjs-dist/legacy/build/pdf.mjs');
  const library = await import('pdfjs-dist/build/pdf.mjs');
  const task = library.getDocument({ data: syntheticPdf('multipage').bytes, verbosity: 0, useSystemFonts: false });
  try {
    const document = await task.promise;
    assert.equal(document.numPages, 3);
    for (let number = 1; number <= 3; number++) {
      const page = await document.getPage(number), text = await page.getTextContent();
      assert.ok(text.items.some((item) => item.str === `Synthetic PDF page ${number} of 3`));
      assert.equal(page.getViewport({ scale: 1 }).width, 612);
    }
  } finally { await task.destroy(); }
});

test('launcher environment and argv cannot inherit credentials or debug flags', () => {
  const env = isolatedEnvironment('/isolated');
  assert.deepEqual(Object.keys(env).sort(), ['PATH', 'HOME', 'CFFIXED_USER_HOME', 'TMPDIR', 'XDG_CONFIG_HOME',
    'XDG_CACHE_HOME', 'XDG_DATA_HOME', 'LANG', 'CAPSTONE_APP_DATA_ROOT', 'CAPSTONE_RESOURCES_DIR', 'CAPSTONE_PAPERS_DIR'].sort());
  assert.ok(Object.values(env).every((value) => value.startsWith('/isolated') ||
    value === '/usr/bin:/bin:/usr/sbin:/sbin' || value === 'en_US.UTF-8'));
  assert.equal(childOptions('/isolated').stdio, 'ignore');
  assert.equal(dirname(childOptions('/isolated').cwd), env.CAPSTONE_RESOURCES_DIR);
  assert.ok(!env.CAPSTONE_APP_DATA_ROOT.startsWith(env.CAPSTONE_RESOURCES_DIR + '/'));
});

test('raw child output is discarded and child closes without emitting fixture secrets', async () => {
  const root = await privateRoot();
  const child = spawn(process.execPath, ['-e',
    'const s=require("node:crypto").randomBytes(24).toString("hex");console.log(s);console.error(s);'], childOptions(root));
  assert.equal(child.stdout, null); assert.equal(child.stderr, null);
  const code = await new Promise((resolve) => child.once('close', resolve));
  assert.equal(code, 0);
  assert.ok(!processRows().some((row) => row.pid === child.pid));
});

test('private paths use exclusive creation and persisted marker refuses a second launch', async () => {
  const root = await privateRoot();
  assert.equal((await lstat(root)).mode & 0o777, 0o700);
  const path = join(root, 'private', 'exclusive-test.txt');
  await exclusive(path, 'non-secret synthetic marker');
  assert.equal((await lstat(path)).mode & 0o777, 0o600);
  await assert.rejects(exclusive(path, 'replacement'), { code: 'EEXIST' });
  await claimSingleLaunch(root);
  await assert.rejects(claimSingleLaunch(root), { code: 'EEXIST' });
});

test('process ownership excludes unrelated trees and rejects PID reuse', () => {
  const rows = [{ pid: 1, ppid: 0 }, { pid: 10, ppid: 1 }, { pid: 11, ppid: 10 },
    { pid: 12, ppid: 11 }, { pid: 20, ppid: 1 }];
  assert.deepEqual(descendants(rows, 10).map((row) => row.pid), [11, 12]);
  const owned = { pid: 12, started: 'fixed-time', executable: '/isolated/app' };
  assert.ok(sameProcess(owned, { ...owned, ppid: 1 }));
  assert.ok(!sameProcess(owned, { ...owned, started: 'another-time' }));
  assert.ok(!sameProcess(owned, { ...owned, executable: '/unrelated/app' }));
});

test('exact-owned child cleanup drains the selected process and leaves its sibling alive', async () => {
  const root = await privateRoot();
  const start = () => spawn(process.execPath, ['-e', 'setInterval(()=>{},1000)'], childOptions(root));
  const owned = start(), sibling = start();
  const closed = new Promise((resolve) => owned.once('close', resolve));
  const siblingClosed = new Promise((resolve) => sibling.once('close', resolve));
  try {
    const row = processRows().find((item) => item.pid === owned.pid);
    assert.ok(row && row.ppid === process.pid);
    assert.equal(await stopOwnedProcesses(new Map([[row.pid, row]])), true);
    await closed;
    assert.ok(processRows().some((item) => item.pid === sibling.pid));
  } finally {
    for (const child of [owned, sibling]) if (child.exitCode === null && child.signalCode === null) child.kill();
    await Promise.all([closed, siblingClosed]);
  }
});

test('inventory can never request passwords, user data, traversal or node_modules', () => {
  for (const name of ['private/protected-password.txt', 'app/dist/assets/../../private/protected-password.txt',
    'profile/Cookies', 'app/node_modules/package.json', '/private/protected-password.txt']) {
    assert.equal(allowedInventoryName(name), false);
  }
  assert.equal(allowedInventoryName('resources/app/electron/main.mjs'), true);
  assert.equal(allowedInventoryName('resources/app/dist/assets/pdfjs-6.3.289/build/pdf.mjs'), true);
});

test('output categories reject arbitrary diagnostic values', () => {
  const unknown = randomBytes(24).toString('hex');
  assert.throws(() => safeReport(unknown));
  assert.throws(() => safeReport('app-exited', [unknown]));
  assert.equal(safeReport('app-exited', [12]), '{"category":"app-exited","pids":[12]}');
});

test('preinit isolates paths before main and does not instrument the renderer', async () => {
  const entry = await readFile(new URL('./helpers/pdf-acceptance-entry.mjs', import.meta.url), 'utf8');
  assert.ok(entry.indexOf('app.setPath') < entry.indexOf("await import('./electron/main.mjs')"));
  assert.doesNotMatch(entry, /webContents|ipcMain|webRequest|BrowserWindow|console\.|debugger|remote-debugging/);
  for (const name of ['home', 'appData', 'userData', 'sessionData', 'temp', 'crashDumps']) assert.ok(entry.includes(name));
  const paths = {}, switches = {};
  const app = { setPath: (name, value) => { paths[name] = value; },
    setAppLogsPath: (value) => { paths.logs = value; },
    commandLine: { appendSwitch: (name, value) => { switches[name] = value; } } };
  const body = entry.replace(/^import .*;$/gm, '').replace('import.meta.url', 'entryURL')
    .replace("await import('./electron/main.mjs')", 'await importMain()');
  const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
  let imported = false;
  await new AsyncFunction('app', 'dirname', 'join', 'fileURLToPath', 'entryURL', 'importMain', body)(
    app, dirname, join, fileURLToPath, 'file:///isolated/resources/app/acceptance-entry.mjs', async () => {
      assert.deepEqual(paths, { home: '/isolated/home', appData: '/isolated/app-data', userData: '/isolated/profile',
        sessionData: '/isolated/session', temp: '/isolated/tmp', crashDumps: '/isolated/crashes', logs: '/isolated/logs' });
      assert.deepEqual(switches, { 'disk-cache-dir': '/isolated/cache' });
      imported = true;
    });
  assert.equal(imported, true);
});