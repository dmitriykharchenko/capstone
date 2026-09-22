import { createHash, randomBytes } from 'node:crypto';

const md5 = (data) => createHash('md5').update(data).digest();
const padding = Buffer.from('28bf4e5e4e758a4164004e56fffa01082e2e00b6d0683e802f0ca9fe6453697a', 'hex');
const pad = (value) => Buffer.concat([Buffer.from(value, 'ascii'), padding]).subarray(0, 32);

// PDF standard-security revision 2, for synthetic test fixtures only.
function rc4(key, bytes) {
  const state = Uint8Array.from({ length: 256 }, (_, i) => i);
  let j = 0;
  for (let i = 0; i < 256; i++) {
    j = (j + state[i] + key[i % key.length]) & 255;
    [state[i], state[j]] = [state[j], state[i]];
  }
  let i = 0; j = 0;
  return Buffer.from(bytes.map((value) => {
    i = (i + 1) & 255; j = (j + state[i]) & 255;
    [state[i], state[j]] = [state[j], state[i]];
    return value ^ state[(state[i] + state[j]) & 255];
  }));
}
function stream(bytes, dictionary = '') {
  return Buffer.concat([Buffer.from(`<< /Length ${bytes.length} ${dictionary} >>\nstream\n`),
    bytes, Buffer.from('\nendstream')]);
}
function pdf(objects, trailer = '') {
  const chunks = [Buffer.from('%PDF-1.4\n')], offsets = [0];
  let length = chunks[0].length;
  for (const [i, object] of objects.entries()) {
    offsets.push(length);
    const chunk = Buffer.concat([Buffer.from(`${i + 1} 0 obj\n`), Buffer.from(object), Buffer.from('\nendobj\n')]);
    chunks.push(chunk); length += chunk.length;
  }
  const table = offsets.slice(1).map((offset) => `${String(offset).padStart(10, '0')} 00000 n \n`).join('');
  chunks.push(Buffer.from(`xref\n0 ${objects.length + 1}\n0000000000 65535 f \n${table}` +
    `trailer\n<< /Size ${objects.length + 1} /Root 1 0 R ${trailer} >>\nstartxref\n${length}\n%%EOF\n`));
  return new Uint8Array(Buffer.concat(chunks));
}

export function syntheticPdf(kind = 'text') {
  let content = Buffer.from(kind === 'blank' ? '' : 'BT /F1 16 Tf 20 100 Td (Synthetic PDF sample) Tj ET');
  let resources = '/Font << /F1 5 0 R >>';
  let resource = '<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>';
  let trailer = '', encryption, password;
  if (kind === 'image' || kind === 'fax') {
    resources = '/XObject << /Im0 5 0 R >>';
    content = Buffer.from('q 80 0 0 80 20 20 cm /Im0 Do Q');
    resource = kind === 'image' ? stream(Buffer.from([255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 0]),
      '/Type /XObject /Subtype /Image /Width 2 /Height 2 /ColorSpace /DeviceRGB /BitsPerComponent 8') :
      stream(Buffer.from([0x80]), '/Type /XObject /Subtype /Image /Width 8 /Height 1 /ColorSpace /DeviceGray ' +
        '/BitsPerComponent 1 /Filter /CCITTFaxDecode /DecodeParms << /K -1 /Columns 8 /Rows 1 /EndOfBlock false >>');
  }
  if (kind === 'locked') {
    password = randomBytes(16).toString('hex');
    const owner = rc4(md5(pad(randomBytes(16).toString('hex'))).subarray(0, 5), pad(password));
    const id = randomBytes(16), permissions = Buffer.alloc(4);
    permissions.writeInt32LE(-4);
    const key = md5(Buffer.concat([pad(password), owner, permissions, id])).subarray(0, 5);
    const user = rc4(key, padding);
    const streamKey = md5(Buffer.concat([key, Buffer.from([4, 0, 0, 0, 0])])).subarray(0, 10);
    content = rc4(streamKey, content);
    encryption = `<< /Filter /Standard /V 1 /R 2 /O <${owner.toString('hex')}> /U <${user.toString('hex')}> /P -4 >>`;
    trailer = `/Encrypt 6 0 R /ID [<${id.toString('hex')}> <${id.toString('hex')}>]`;
  }
  const objects = ['<< /Type /Catalog /Pages 2 0 R >>', '<< /Type /Pages /Kids [3 0 R] /Count 1 >>',
    `<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << ${resources} >> /Contents 4 0 R >>`,
    stream(content), resource];
  if (encryption) objects.push(encryption);
  return { bytes: pdf(objects, trailer), password };
}