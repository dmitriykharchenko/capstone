import { app } from 'electron';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

// Test-only entry, copied to <isolation>/resources/app before unchanged main.
// No renderer, IPC, policy, console, navigation or diagnostic hooks.
const root = dirname(dirname(dirname(fileURLToPath(import.meta.url))));
for (const [name, directory] of Object.entries({ home: 'home', appData: 'app-data',
  userData: 'profile', sessionData: 'session', temp: 'tmp', crashDumps: 'crashes' })) {
  app.setPath(name, join(root, directory));
}
app.setAppLogsPath(join(root, 'logs'));
app.commandLine.appendSwitch('disk-cache-dir', join(root, 'cache'));
await import('./electron/main.mjs');