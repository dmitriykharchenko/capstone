// Generated valid PDF with a sparse NUL-whitespace content stream. Only its
// bounded header and tail are materialized; no fixture document buffer exists.
export function sparsePdf() {
  let header = '%PDF-1.4\n';
  const offsets = [0];
  const objects = ['<< /Type /Catalog /Pages 2 0 R >>', '<< /Type /Pages /Kids [3 0 R] /Count 1 >>',
    '<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << >> /Contents 4 0 R >>'];
  objects.forEach((object, i) => { offsets.push(header.length); header += `${i + 1} 0 obj\n${object}\nendobj\n`; });
  offsets.push(header.length);
  header += '4 0 obj\n<< /Length 68157440 >>\nstream\n';
  const tailStart = header.length + 65 * 1024 * 1024, suffix = '\nendstream\nendobj\n';
  const tail = suffix + 'xref\n0 5\n0000000000 65535 f \n' + offsets.slice(1).map(n => `${String(n).padStart(10, '0')} 00000 n \n`).join('') +
    `trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n${tailStart + suffix.length}\n%%EOF\n`;
  const parts = [[0, Buffer.from(header)], [tailStart, Buffer.from(tail)]];
  return { parts, length: tailStart + tail.length,
    read(begin, end) {
      const bytes = new Uint8Array(end - begin);
      for (const [start, data] of parts) {
        const from = Math.max(start, begin), to = Math.min(start + data.length, end);
        if (to > from) bytes.set(data.subarray(from - start, to - start), from - begin);
      }
      return bytes;
    } };
}