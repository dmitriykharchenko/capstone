import { spawn, execFileSync } from 'node:child_process';
import { writeFileSync } from 'node:fs';
import { lstat, readFile, realpath, unlink } from 'node:fs/promises';
import { basename, dirname, isAbsolute, join, resolve } from 'node:path';
import { tmpdir } from 'node:os';
import { pathToFileURL } from 'node:url';
import { allowedInventoryName, ensure, exclusive, fixtures, isolatedEnvironment, nativePreflight, prepare, reviewed, source, verify } from './pdf-acceptance-stage.mjs';

const categories = new Set(['prepared', 'verified', 'launch-started', 'processes-running',
  'launch-failed', 'app-exited', 'owned-processes-stopped', 'cleanup-unconfirmed', 'preparation-failed',
  'password-file-removed', 'password-cleanup-refused']);
export function safeReport(category, pids = []) {
  ensure(categories.has(category) && pids.every((pid) => Number.isSafeInteger(pid) && pid > 0));
  return JSON.stringify({ category, pids });
}
export function processRows() {
  // Read executable identity/start time, never argv or environment. Nothing raw escapes.
  const output = execFileSync('/bin/ps', ['-axo', 'pid=,ppid=,lstart=,comm='],
    { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'], maxBuffer: 4 * 1024 * 1024 });
  const lines = output.split('\n').filter((line) => line.trim());
  const rows = lines.flatMap((line) => {
    const match = line.trim().match(/^(\d+)\s+(\d+)\s+(\S+\s+\S+\s+\d+\s+[\d:]+\s+\d+)\s+(.+)$/);
    return match ? [{ pid: Number(match[1]), ppid: Number(match[2]), started: match[3], executable: match[4] }] : [];
  });
  ensure(rows.length === lines.length && rows.some((row) => row.pid === process.pid));
  return rows;
}
export function sameProcess(a, b) {
  return Boolean(a && b && a.pid === b.pid && a.started === b.started && a.executable === b.executable);
}
export function descendants(rows, parentPid) {
  const ids = new Set([parentPid]), result = [];
  for (let changed = true; changed;) {
    changed = false;
    for (const row of rows) if (!ids.has(row.pid) && ids.has(row.ppid)) {
      ids.add(row.pid); result.push(row); changed = true;
    }
  }
  return result;
}
export function childOptions(root) {
  return { cwd: join(root, 'resources/app'), env: isolatedEnvironment(root), stdio: 'ignore', windowsHide: false };
}
export async function claimSingleLaunch(root) {
  await exclusive(join(root, 'launch-once.json'), JSON.stringify({ launcherPid: process.pid, created: new Date().toISOString() }));
}
export async function stopOwnedProcesses(known) {
  for (const signal of ['SIGTERM', 'SIGKILL']) {
    for (const row of [...known.values()].reverse()) {
      const current = processRows().find((item) => item.pid === row.pid);
      if (sameProcess(row, current)) {
        try { process.kill(row.pid, signal); } catch (error) { if (error.code !== 'ESRCH') throw error; }
      }
    }
    await new Promise((resolve) => setTimeout(resolve, signal === 'SIGTERM' ? 1500 : 250));
  }
  const rows = processRows();
  return [...known.values()].every((row) => !sameProcess(row, rows.find((item) => item.pid === row.pid)));
}
const validObservation = (row) => row && Number.isSafeInteger(row.pid) && row.pid > 0 &&
  Number.isSafeInteger(row.ppid) && row.ppid >= 0 && typeof row.started === 'string' &&
  Number.isFinite(Date.parse(row.started)) && typeof row.executable === 'string' && row.executable.length > 0;
const validIdentity = (row) => validObservation(row) && isAbsolute(row.executable);
export function confirmStopped(root, manifest, launch, ownership, rows) {
  ensure(Number.isSafeInteger(launch.launcherPid) && launch.launcherPid > 0 && Number.isFinite(Date.parse(launch.created)));
  ensure(validIdentity(ownership.launcher) && ownership.launcher.pid === launch.launcherPid);
  ensure(Array.isArray(ownership.processes) && ownership.processes.length > 0 && ownership.processes.every(validIdentity));
  const identities = [ownership.launcher, ...ownership.processes];
  ensure(new Set(identities.map((row) => row.pid)).size === identities.length);
  ensure(ownership.processes.some((row) => row.executable === manifest.electron && row.ppid === launch.launcherPid));
  const bundle = manifest.electron.slice(0, -'Contents/MacOS/Electron'.length);
  const isApp = (row) => row.executable === manifest.electron || row.executable.startsWith(bundle + 'Contents/Frameworks/');
  const isNative = (row) => row.executable === join(root, 'resources/app/backend/target/debug/research-backend');
  ensure(ownership.processes.every((row) => isApp(row) || isNative(row)));
  ensure(Array.isArray(rows) && rows.length > 0 && rows.every(validObservation) && rows.some((row) => row.pid === process.pid));
  ensure(identities.every((row) => !rows.some((current) => row.pid === current.pid && row.started === current.started)));
  // Fail closed even on an unrecorded isolated child or another app sharing this runtime.
  // This check never signals a process; a reused PID alone is not a live identity.
  ensure(!rows.some((row) => row.executable.startsWith(root + '/') || isApp(row) || isNative(row)));
}
async function cleanupEvidence(root, name) {
  const path = join(root, name), entry = await lstat(path);
  ensure(entry.isFile() && entry.nlink === 1 && entry.uid === process.getuid() &&
    (entry.mode & 0o777) === 0o600 && entry.size > 0 && entry.size <= 262144);
  return JSON.parse(await readFile(path, 'utf8'));
}
export function assertPasswordIdentity(entry, identity) {
  ensure(entry.isFile() && entry.nlink === 1 && entry.uid === process.getuid() &&
    (entry.mode & 0o777) === 0o600 && entry.dev === identity?.dev && entry.ino === identity?.ino);
}
export async function cleanupPassword(root, inspectProcesses = processRows) {
  try {
    ensure(resolve(root) === await realpath(root) && dirname(root) === await realpath(tmpdir()) &&
      basename(root).startsWith('capstone-pdfjs-acceptance-'));
    for (const path of [root, join(root, 'private')]) {
      const entry = await lstat(path);
      ensure(entry.isDirectory() && entry.uid === process.getuid() && (entry.mode & 0o777) === 0o700);
    }
    const manifest = await cleanupEvidence(root, 'preparation.json');
    ensure(manifest.version === 1 && manifest.root === root && manifest.reviewed === reviewed &&
      manifest.appRoot === join(root, 'resources/app') &&
      manifest.electron === join(source, 'node_modules/electron/dist/Electron.app/Contents/MacOS/Electron'));
    ensure(Array.isArray(manifest.inventory) && manifest.inventory.length > 0 &&
      manifest.inventory.every((item) => allowedInventoryName(item.name) && /^[a-f0-9]{64}$/.test(item.sha256)) &&
      new Set(manifest.inventory.map((item) => item.name)).size === manifest.inventory.length);
    const launch = await cleanupEvidence(root, 'launch-once.json');
    const ownership = await cleanupEvidence(root, 'owned-processes.json');
    confirmStopped(root, manifest, launch, ownership, inspectProcesses());
    const path = join(root, 'private/protected-password.txt');
    assertPasswordIdentity(await lstat(path), manifest.passwordIdentity);
    confirmStopped(root, manifest, launch, ownership, inspectProcesses());
    assertPasswordIdentity(await lstat(path), manifest.passwordIdentity);
    // The only removal path. Never open, read or hash this file; retain all other artifacts.
    await unlink(path);
    return 'password-file-removed';
  } catch { throw new Error('Password cleanup refused.'); }
}
export async function launch(root) {
  const manifest = await verify(root);
  ensure(manifest.electron === join(source, 'node_modules/electron/dist/Electron.app/Contents/MacOS/Electron'));
  await nativePreflight(root);
  // Persists even on a failed spawn: a new launch requires fresh human authorization.
  await claimSingleLaunch(root);
  const ownershipFile = join(root, 'owned-processes.json');
  await exclusive(ownershipFile, '{}');
  const child = spawn(manifest.electron, [join(root, 'resources/app/acceptance-entry.mjs')], childOptions(root));
  const known = new Map();
  const report = (category) => console.log(safeReport(category, [...known.keys()]));
  const electronBundle = manifest.electron.slice(0, -'Contents/MacOS/Electron'.length);
  const native = join(root, 'resources/app/backend/target/debug/research-backend');
  let stopping, polling, lastPids = '', closed = false;
  const observe = () => {
    const rows = processRows();
    for (const row of descendants(rows, process.pid)) {
      if (row.executable === manifest.electron || row.executable === native ||
        row.executable.startsWith(electronBundle + 'Contents/Frameworks/')) known.set(row.pid, row);
    }
    return rows;
  };
  const snapshot = async () => {
    const rows = observe();
    writeFileSync(ownershipFile, JSON.stringify({ launcher: rows.find((row) => row.pid === process.pid),
      processes: [...known.values()] }), { mode: 0o600 });
    const live = [...known.values()].filter((row) => sameProcess(row, rows.find((item) => item.pid === row.pid)));
    const pids = live.map((row) => row.pid).join(',');
    if (pids !== lastPids) { lastPids = pids; console.log(safeReport('processes-running', live.map((row) => row.pid))); }
  };
  const stop = () => stopping ??= (async () => {
    clearInterval(polling);
    try {
      // Persist the final observed identities before shutdown; synchronous writes
      // cannot race a periodic snapshot and overwrite this with older evidence.
      await snapshot();
      const drained = await stopOwnedProcesses(known);
      report(drained ? 'owned-processes-stopped' : 'cleanup-unconfirmed');
      process.exitCode = drained ? 0 : 1;
    } catch { report('cleanup-unconfirmed'); process.exitCode = 1; }
    process.removeListener('SIGTERM', stop); process.removeListener('SIGINT', stop);
  })();
  child.on('error', () => { report('launch-failed'); void stop(); });
  child.on('close', () => { closed = true; report('app-exited'); void stop(); });
  process.on('SIGTERM', stop); process.on('SIGINT', stop);
  report('launch-started');
  if (!closed) {
    try { await snapshot(); } catch { report('cleanup-unconfirmed'); await stop(); }
    if (!stopping) polling = setInterval(() => { void snapshot().catch(() => stop()); }, 1000);
  }
}
async function main() {
  const [command, root] = process.argv.slice(2);
  if (command === 'prepare' && !root) {
    const prepared = await prepare();
    const result = await nativePreflight(prepared);
    const secret = join(prepared, 'private/protected-password.txt');
    const mode = (await lstat(secret)).mode & 0o777;
    console.log(JSON.stringify({ ...result, category: 'prepared', root: prepared,
      filenames: Object.keys(fixtures), passwordFile: secret, passwordMode: mode.toString(8), guiLaunched: false }));
  } else if (command === 'verify' && root) {
    console.log(JSON.stringify(await nativePreflight(root)));
  } else if (command === 'launch' && root) await launch(root);
  else if (command === 'cleanup-after-human' && root) {
    try { console.log(safeReport(await cleanupPassword(root))); }
    catch { console.error(safeReport('password-cleanup-refused')); process.exitCode = 1; }
  }
  else throw new Error('Invalid acceptance command.');
}
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch(() => { console.error(safeReport('preparation-failed')); process.exitCode = 1; });
}