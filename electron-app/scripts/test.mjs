import { readdirSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
const cwd = fileURLToPath(new URL('..', import.meta.url));
const native = spawnSync('rtk', ['cargo', 'build', '--manifest-path', 'backend/Cargo.toml', '--locked', '--offline'], { cwd, stdio: 'inherit' });
if (native.status !== 0) process.exit(native.status ?? 1);
// Retain the mounted graph suite, but never start a browser without authorization.
const files = readdirSync(new URL('../tests', import.meta.url)).filter((name) =>
  name.endsWith('.test.mjs') && name !== 'graph-flow-rendered.test.mjs').sort();
console.log('Browser-mounted graph test not run: requires separate user authorization.');
const result = spawnSync(process.execPath, ['--test', ...files.map((name) => `tests/${name}`)], { cwd, stdio: 'inherit' });
process.exit(result.status ?? 1);