import assert from 'node:assert/strict';
import { spawn, execFileSync } from 'node:child_process';
import { mkdir, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { once } from 'node:events';
import { setTimeout as delay } from 'node:timers/promises';
import { syntheticPdf } from '../pdf-fixtures.mjs';

// Native startup only: no browser/CDP, injected code, screenshots or UI automation.
export async function launchPackagedSmoke(app, root) {
  const profile = join(root, 'desktop'), papers = join(root, 'desktop-papers');
  for (const path of [profile, papers, join(root, 'home'), join(root, 'tmp')]) await mkdir(path);
  await writeFile(join(papers, 'Desktop-Fixture.pdf'), syntheticPdf('text').bytes);
  const executable = join(app, 'Contents/MacOS/Research Assistant');
  const child = spawn(executable, [], { cwd: root, stdio: ['ignore', 'pipe', 'pipe'], env: {
    PATH: join(root, 'empty-path'), HOME: join(root, 'home'), CFFIXED_USER_HOME: join(root, 'home'),
    TMPDIR: join(root, 'tmp'), LANG: 'en_US.UTF-8', CAPSTONE_DESKTOP_DATA_DIR: profile, CAPSTONE_PAPERS_DIR: papers,
  } });
  let startupFailed = false, outputBytes = 0, backendPid;
  const drain = (chunk) => { outputBytes += chunk.length; if (chunk.toString().includes('Application startup failed.')) startupFailed = true; };
  child.stdout.on('data', drain); child.stderr.on('data', drain);
  const exited = once(child, 'exit');
  const alive = (pid) => { try { process.kill(pid, 0); return true; } catch { return false; } };
  try {
    for (let attempt = 0; attempt < 150; attempt++) {
      assert.equal(child.exitCode, null, 'packaged Electron exited before startup');
      assert.equal(child.signalCode, null);
      assert.equal(startupFailed, false);
      // Only process identities, parent IDs and executable paths; never argv/env.
      const rows = execFileSync('/bin/ps', ['-axo', 'pid=,ppid=,comm='], { encoding: 'utf8' }).split('\n');
      const owned = rows.map((row) => row.trim().match(/^(\d+)\s+(\d+)\s+(.*)$/)).filter(Boolean)
        .filter((row) => Number(row[2]) === child.pid && row[3] === join(app, 'Contents/Resources/backend/research-backend'));
      assert.ok(owned.length <= 1, 'at most one backend per Electron owner');
      if (owned.length === 1) { backendPid = Number(owned[0][1]); break; }
      await delay(100);
    }
    assert.ok(backendPid, 'renderer must reach native health through the packaged preload');
    assert.equal(startupFailed, false);
    child.kill('SIGTERM');
    const result = await Promise.race([exited, delay(10_000).then(() => { throw new Error('Packaged shutdown timed out'); })]);
    assert.deepEqual(result, [0, null]);
    assert.equal(alive(backendPid), false, 'owned backend drained before app exit');
    return { started: true, oneBackend: true, gracefulExit: 0, backendStopped: true, outputBytes };
  } finally {
    if (child.exitCode === null && child.signalCode === null) { child.kill('SIGKILL'); await exited; }
    if (backendPid && alive(backendPid)) process.kill(backendPid, 'SIGTERM');
  }
}