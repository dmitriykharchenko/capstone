import { readdirSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
for (const folder of ['electron', 'scripts']) {
  for (const file of readdirSync(new URL(`../${folder}/`, import.meta.url))) {
    if (!/\.(mjs|cjs)$/.test(file)) continue;
    const result = spawnSync(process.execPath, ['--check', fileURLToPath(new URL(`../${folder}/${file}`, import.meta.url))], { stdio: 'inherit' });
    if (result.status !== 0) process.exit(result.status ?? 1);
  }
}
console.log('Electron main/preload and scripts syntax checks passed.');