import { isAbsolute, normalize } from 'node:path';
import { realpathSync, statSync } from 'node:fs';

// Trusted launcher-only override, deliberately separate from backend env roots.
// Require an existing explicit directory; never infer it from cwd or read dotenv.
export function configureDesktopData(app, env) {
  if (env.CAPSTONE_DESKTOP_DATA_DIR === undefined) return;
  const value = env.CAPSTONE_DESKTOP_DATA_DIR;
  if (!isAbsolute(value) || normalize(value) !== value || !statSync(value).isDirectory()) {
    throw new Error('Invalid desktop data directory.');
  }
  const path = realpathSync(value);
  app.setPath('userData', path);
  app.setPath('sessionData', path);
}