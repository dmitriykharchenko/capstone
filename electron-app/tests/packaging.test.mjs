import assert from 'node:assert/strict';
import test from 'node:test';
import { mkdtemp, mkdir, rm, realpath, writeFile, cp } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { Backend } from '../electron/backend.mjs';
import { Broker } from '../electron/broker.mjs';
import { configureDesktopData } from '../electron/paths.mjs';
import { nativePackageSmoke } from './helpers/native-package-smoke.mjs';
import config from '../electron-builder.config.cjs';

test('package allowlist excludes dependencies, user content and signing', () => {
  assert.deepEqual(config.files, ['dist/**/*', 'electron/**/*', 'package.json', '!node_modules/**/*']);
  assert.deepEqual(config.extraResources, [{ from: 'backend/target/release/research-backend', to: 'backend/research-backend' }]);
  assert.equal(config.mac.identity, null);
  assert.equal(config.mac.notarize, false);
  assert.equal(config.npmRebuild, false);
  assert.equal(config.electronDist, 'node_modules/electron/dist');
});

test('desktop writable path is explicit, existing and independent of backend overrides', async (t) => {
  const root = await realpath(await mkdtemp(join(tmpdir(), 'capstone-desktop-paths-')));
  t.after(() => rm(root, { recursive: true, force: true }));
  const calls = [];
  const app = { setPath: (...args) => calls.push(args) };
  configureDesktopData(app, { CAPSTONE_APP_DATA_ROOT: '/ignored' });
  assert.equal(calls.length, 0);
  configureDesktopData(app, { CAPSTONE_DESKTOP_DATA_DIR: root });
  assert.deepEqual(calls, [['userData', root], ['sessionData', root]]);
  await writeFile(join(root, 'file'), 'fixture');
  for (const path of ['', 'relative', `${root}/../escape`, join(root, 'missing'), join(root, 'file')]) {
    assert.throws(() => configureDesktopData(app, { CAPSTONE_DESKTOP_DATA_DIR: path }));
  }
  assert.equal(calls.length, 2, 'invalid overrides do not fall back or change paths');
});

test('complete production composition outside repository uses one Python-free native backend', { timeout: 60_000 }, async (t) => {
  const root = await realpath(await mkdtemp(join(tmpdir(), 'capstone-native-package-')));
  t.after(() => rm(root, { recursive: true, force: true }));
  const resources = join(root, 'resources');
  await mkdir(resources);
  const executable = join(resources, 'research-backend');
  await cp(fileURLToPath(new URL('../backend/target/debug/research-backend', import.meta.url)), executable);
  const result = await nativePackageSmoke({ root, executable, resources, Backend, Broker });
  assert.equal(result.commands, 19);
});