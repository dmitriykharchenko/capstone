import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { join } from 'node:path';
import { unlink } from 'node:fs/promises';
import { build, Platform, Arch } from 'electron-builder';
import { build as buildRenderer } from 'vite';
import config from '../electron-builder.config.cjs';

if (process.platform !== 'darwin' || !['arm64', 'x64'].includes(process.arch)) {
  throw new Error('Unsigned packaging is currently supported on macOS arm64/x64 hosts only.');
}
const root = fileURLToPath(new URL('..', import.meta.url));
const native = spawnSync('rtk', ['cargo', 'build', '--manifest-path', 'backend/Cargo.toml',
  '--release', '--locked', '--offline'], { cwd: root, stdio: 'inherit' });
if (native.status !== 0) process.exit(native.status ?? 1);
await buildRenderer({ root, logLevel: 'warn' });
// Never discover certificates or publish. Use the already-installed Electron runtime.
process.env.CSC_IDENTITY_AUTO_DISCOVERY = 'false';
delete process.env.CSC_LINK;
delete process.env.CSC_KEY_PASSWORD;
await build({ projectDir: root, config, targets: Platform.MAC.createTarget('dir', Arch[process.arch]), publish: 'never' });
// Custom local Electron distributions retain their demo app. Remove only that
// generated scaffold, never anything from the installed runtime or source tree.
await unlink(join(root, 'release', process.arch === 'arm64' ? 'mac-arm64' : 'mac',
  'Research Assistant.app/Contents/Resources/default_app.asar'));
console.log(`Unsigned macOS ${process.arch} application packaged; signing/notarization are not verified.`);