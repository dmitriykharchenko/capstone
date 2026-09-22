import { readFile, readdir } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { dirname, join } from 'node:path';

const require = createRequire(import.meta.url);
const root = dirname(require.resolve('pdfjs-dist/package.json'));
const { version } = require('pdfjs-dist/package.json');
export const pdfjsAssetPrefix = `/assets/pdfjs-${version}/`;

/** Only runtime assets used with useWasm:false; never the upstream viewer/sandbox. */
export async function pdfjsAssets() {
  const names = ['build/pdf.mjs', 'build/pdf.worker.mjs', 'LICENSE',
    'wasm/openjpeg_nowasm_fallback.js', 'wasm/jbig2_nowasm_fallback.js',
    'wasm/LICENSE_OPENJPEG', 'wasm/LICENSE_PDFJS_OPENJPEG',
    'wasm/LICENSE_JBIG2', 'wasm/LICENSE_PDFJS_JBIG2'];
  for (const folder of ['cmaps', 'standard_fonts']) {
    for (const entry of await readdir(join(root, folder), { withFileTypes: true })) {
      if (entry.isFile() && /^(?:[\w-]+\.(?:bcmap|pfb|ttf)|LICENSE(?:_[A-Z]+)?)$/.test(entry.name)) {
        names.push(`${folder}/${entry.name}`);
      }
    }
  }
  return Promise.all(names.sort().map(async (name) => ({
    fileName: `${pdfjsAssetPrefix.slice(1)}${name}`,
    source: await readFile(join(root, name)),
    mime: /\.m?js$/.test(name) ? 'text/javascript' : name.endsWith('.ttf') ? 'font/ttf' : 'application/octet-stream',
  })));
}

/** @returns {import('vite').Plugin} */
export function pdfjsAssetsPlugin() {
  return {
    name: 'local-pdfjs-assets',
    async generateBundle() {
      for (const asset of await pdfjsAssets()) {
        this.emitFile({ type: 'asset', fileName: asset.fileName, source: asset.source });
      }
    },
    async configureServer(server) {
      const assets = new Map((await pdfjsAssets()).map((asset) => [`/${asset.fileName}`, asset]));
      server.middlewares.use((request, response, next) => {
        const path = request.url?.split('?')[0] ?? '';
        if (!path.startsWith(pdfjsAssetPrefix)) return next();
        const asset = assets.get(path);
        if (!asset || request.method !== 'GET') {
          response.statusCode = 404;
          response.end();
          return;
        }
        response.setHeader('Content-Type', asset.mime);
        response.setHeader('X-Content-Type-Options', 'nosniff');
        response.end(asset.source);
      });
    },
  };
}