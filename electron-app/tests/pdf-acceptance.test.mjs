import assert from 'node:assert/strict';
import test from 'node:test';
import { spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { readFile, lstat, writeFile, chmod, link, symlink, mkdir } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { syntheticPdf } from './pdf-fixtures.mjs';
import { fixtures, isolatedEnvironment, privateRoot, exclusive, allowedInventoryName, reviewed, source } from './helpers/pdf-acceptance-stage.mjs';
import { childOptions, claimSingleLaunch, descendants, sameProcess, safeReport, processRows, stopOwnedProcesses, cleanupPassword, assertPasswordIdentity } from './helpers/pdf-acceptance.mjs';

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

async function cleanupFixture(kind = 'file') {
  // Disposable dummy text only. Never use the real prepared root or its credential.
  const root = await privateRoot(), password = join(root, 'private/protected-password.txt');
  const sentinel = join(root, 'private/keep.txt');
  await exclusive(sentinel, 'retained non-secret test marker');
  if (kind === 'symlink') await symlink(sentinel, password);
  else if (kind === 'directory') await mkdir(password, { mode: 0o700 });
  else await exclusive(password, 'not a credential');
  const stat = await lstat(password);
  const manifest = { version: 1, root, reviewed, appRoot: join(root, 'resources/app'),
    electron: join(source, 'node_modules/electron/dist/Electron.app/Contents/MacOS/Electron'),
    inventory: [{ name: 'resources/app/acceptance-entry.mjs', sha256: '0'.repeat(64) }],
    passwordIdentity: { dev: stat.dev, ino: stat.ino } };
  const launch = { launcherPid: 2147483645, created: '2000-01-01T00:00:00.000Z' };
  const ownership = { launcher: { pid: launch.launcherPid, ppid: 1, started: 'Sat Jan 1 00:00:00 2000', executable: process.execPath },
    processes: [{ pid: 2147483644, ppid: launch.launcherPid, started: 'Sat Jan 1 00:00:01 2000', executable: manifest.electron }] };
  await exclusive(join(root, 'preparation.json'), JSON.stringify(manifest));
  const saveLaunch = () => exclusive(join(root, 'launch-once.json'), JSON.stringify(launch));
  const saveOwnership = () => exclusive(join(root, 'owned-processes.json'), JSON.stringify(ownership));
  const self = processRows().find((row) => row.pid === process.pid);
  return { root, password, sentinel, manifest, launch, ownership, saveLaunch, saveOwnership, rows: [self] };
}

test('post-human cleanup removes only the exact dummy file with independent stopped-process evidence', async () => {
  const f = await cleanupFixture();
  await f.saveLaunch(); await f.saveOwnership();
  // Real OS process-table check, not a GUI launch or the actual saved credential.
  assert.equal(await cleanupPassword(f.root), 'password-file-removed');
  await assert.rejects(lstat(f.password), { code: 'ENOENT' });
  for (const path of [f.root, f.sentinel, join(f.root, 'profile'), join(f.root, 'preparation.json'),
    join(f.root, 'launch-once.json'), join(f.root, 'owned-processes.json')]) assert.ok(await lstat(path));
  await assert.rejects(cleanupPassword(f.root), { message: 'Password cleanup refused.' });
});

for (const scenario of ['no-launch', 'no-ownership', 'empty-ownership', 'live-launcher', 'live-app', 'changed-executable',
  'late-live-process', 'unrecorded-native', 'invalid-process-table', 'process-inspection-error', 'wrong-root', 'unexpected-inventory',
  'wrong-inode', 'wrong-device', 'wrong-mode', 'hardlink', 'symlink', 'directory', 'unsafe-private-mode', 'unsafe-evidence-mode']) {
  test(`post-human cleanup refuses ${scenario} and retains the dummy file`, async () => {
    const f = await cleanupFixture(scenario);
    let inspect = () => f.rows;
    if (scenario === 'empty-ownership') f.ownership.processes = [];
    if (scenario === 'live-launcher') inspect = () => [...f.rows, f.ownership.launcher];
    if (scenario === 'live-app') inspect = () => [...f.rows, f.ownership.processes[0]];
    if (scenario === 'changed-executable') inspect = () => [...f.rows, { ...f.ownership.launcher, executable: '/unrelated' }];
    if (scenario === 'late-live-process') {
      let calls = 0;
      inspect = () => ++calls === 1 ? f.rows : [...f.rows, f.ownership.processes[0]];
    }
    if (scenario === 'unrecorded-native') inspect = () => [...f.rows, { ...f.ownership.launcher, pid: 2147483643,
      executable: join(f.root, 'resources/app/backend/target/debug/research-backend') }];
    if (scenario === 'invalid-process-table') inspect = () => [];
    if (scenario === 'process-inspection-error') inspect = () => { throw new Error('untrusted diagnostic'); };
    if (scenario === 'wrong-root') f.manifest.root += '-other';
    if (scenario === 'unexpected-inventory') f.manifest.inventory[0].name = 'private/protected-password.txt';
    if (scenario === 'wrong-inode') f.manifest.passwordIdentity.ino++;
    if (scenario === 'wrong-device') f.manifest.passwordIdentity.dev++;
    if (scenario === 'wrong-mode') await chmod(f.password, 0o644);
    if (scenario === 'hardlink') await link(f.password, join(f.root, 'private/extra-link.txt'));
    if (scenario === 'unsafe-private-mode') await chmod(join(f.root, 'private'), 0o755);
    await writeFile(join(f.root, 'preparation.json'), JSON.stringify(f.manifest));
    if (scenario === 'unsafe-evidence-mode') await chmod(join(f.root, 'preparation.json'), 0o644);
    if (scenario !== 'no-launch') await f.saveLaunch();
    if (scenario !== 'no-ownership') await f.saveOwnership();
    await assert.rejects(cleanupPassword(f.root, inspect), { message: 'Password cleanup refused.' });
    assert.ok(await lstat(f.password)); assert.ok(await lstat(f.sentinel));
  });
}

test('post-human cleanup distinguishes a reused PID without signalling the unrelated process', async () => {
  const f = await cleanupFixture();
  await f.saveLaunch(); await f.saveOwnership();
  const reused = { ...f.ownership.launcher, started: 'Sun Jan 2 00:00:00 2000', executable: '/unrelated' };
  assert.equal(await cleanupPassword(f.root, () => [...f.rows, reused]), 'password-file-removed');
  assert.ok(await lstat(f.sentinel));
});

test('password identity guard rejects different ownership without needing host ownership changes', async () => {
  const f = await cleanupFixture();
  const entry = await lstat(f.password);
  assert.throws(() => assertPasswordIdentity({ ...entry, uid: entry.uid + 1, isFile: () => true }, f.manifest.passwordIdentity));
  assert.ok(await lstat(f.password));
});

for (const allowed of [false, true]) {
  test(`cleanup command emits only its fixed ${allowed ? 'success' : 'refusal'} category`, async () => {
    const f = await cleanupFixture();
    if (allowed) { await f.saveLaunch(); await f.saveOwnership(); }
    const child = spawn(process.execPath, [fileURLToPath(new URL('./helpers/pdf-acceptance.mjs', import.meta.url)),
      'cleanup-after-human', f.root], { ...childOptions(f.root),
      // Standalone operator helper uses the original temp parent, not the app's isolated TMPDIR.
      env: { ...childOptions(f.root).env, TMPDIR: dirname(f.root) }, stdio: ['ignore', 'pipe', 'pipe'] });
    let stdout = '', stderr = '';
    child.stdout.on('data', (bytes) => { stdout += bytes; });
    child.stderr.on('data', (bytes) => { stderr += bytes; });
    assert.equal(await new Promise((resolve) => child.once('close', resolve)), allowed ? 0 : 1);
    assert.ok(allowed ? stderr === '' && stdout.trim() === safeReport('password-file-removed') :
      stdout === '' && stderr.trim() === safeReport('password-cleanup-refused'));
    if (allowed) await assert.rejects(lstat(f.password), { code: 'ENOENT' });
    else assert.ok(await lstat(f.password));
    assert.ok(await lstat(f.sentinel));
  });
}