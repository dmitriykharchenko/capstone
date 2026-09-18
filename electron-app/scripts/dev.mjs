import { spawn, spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { createServer } from 'vite';
import electron from 'electron';
const root = fileURLToPath(new URL('..', import.meta.url));
const build = spawnSync('rtk', ['cargo', 'build', '--manifest-path', 'backend/Cargo.toml', '--locked', '--offline'], { cwd: root, stdio: 'inherit' });
if (build.status !== 0) process.exit(build.status ?? 1);
const server = await createServer({ root });
await server.listen();
const child = spawn(electron, ['.', '--dev'], { cwd: root, stdio: 'inherit', env: process.env });
let closing = false;
async function close(code = 0) {
  if (closing) return;
  closing = true;
  child.kill();
  await server.close();
  process.exitCode = code;
}
child.once('exit', (code) => { void close(code ?? 1); });
child.once('error', () => { void close(1); });
process.once('SIGINT', () => { void close(); });
process.once('SIGTERM', () => { void close(); });