import { syntheticPdf } from '../pdf-fixtures.mjs';

// Real bundled PDF.js registration/teardown with a FontFaceSet-shaped double.
// No native FontFace/glyph rendering claim. Private access is fault injection
// confined to tests; production uses the documented ownerDocument option only.
export async function fontHarness(lifecycle, ownerDocument = { fonts: new Set() }) {
  await import('pdfjs-dist/legacy/build/pdf.mjs');
  const library = await import('pdfjs-dist/build/pdf.mjs');
  const bytes = syntheticPdf('multipage').bytes, loads = [];
  return { ownerDocument, loads, load(options) {
    const record = { closed: 0, disposed: 0, terminated: 0 };
    const load = lifecycle.createPdfLoader({ assetBase: '', ownerDocument,
      loadLibrary: async () => ({ ...library, getDocument(parameters) {
        record.context = parameters.ownerDocument;
        return record.task = library.getDocument({ ...parameters, disableFontFace: true, useWorkerFetch: false });
      } }),
      createWorker() {
        const worker = library.PDFWorker.create({ verbosity: 0 });
        return { worker, dispose() { worker.destroy(); record.disposed++; } };
      },
    });
    record.handle = load({ length: bytes.length, read: async (begin, end) => bytes.slice(begin, end),
      close: async () => { record.closed++; } }, options);
    loads.push(record);
    return record;
  } };
}

export async function holdFontTermination(record) {
  const { document } = await record.handle.promise;
  const transport = document._transport;
  const gate = Promise.withResolvers(), send = transport.messageHandler.sendWithPromise;
  transport.messageHandler.sendWithPromise = function (action, ...args) {
    const result = send.call(this, action, ...args);
    if (action !== 'Terminate') return result;
    record.terminated++;
    return Promise.all([result, gate.promise]);
  };
  return {
    document,
    add(font) { transport.fontLoader.addNativeFontFace(font); },
    remaining: () => transport.fontLoader.nativeFontFaces.size,
    release: gate.resolve,
    async drain() { gate.resolve(); await record.task.destroy(); await record.handle.destroy(); },
  };
}