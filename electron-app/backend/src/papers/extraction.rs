use std::{io, panic::AssertUnwindSafe};

use super::{MAX_PDF_BYTES, PaperError, PaperResult};
use crate::runtime::Cancellation;

pub const MAX_PDF_PAGES: usize = 10_000;
pub const MAX_EXTRACTED_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtractedPage {
    pub page_number: u32,
    pub text: String,
}

pub(super) fn check(cancellation: &Cancellation) -> PaperResult<()> {
    cancellation.check().map_err(|_| PaperError::Cancelled)
}

struct TextBuffer<'a> {
    bytes: Vec<u8>,
    remaining: usize,
    limit_hit: bool,
    cancellation: &'a Cancellation,
}

impl io::Write for TextBuffer<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.cancellation.check().is_err() {
            return Err(io::Error::other("cancelled"));
        }
        if bytes.len() > self.remaining {
            self.limit_hit = true;
            return Err(io::Error::other("text limit"));
        }
        self.bytes.extend_from_slice(bytes);
        self.remaining -= bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Extract every page or fail the whole document. Blank/image-only pages are
/// retained as empty strings (and yield no chunks); no OCR is attempted.
/// Call from blocking work: parsing/decompression itself is not interruptible.
pub fn extract_pdf_pages(
    bytes: &[u8],
    cancellation: &Cancellation,
) -> PaperResult<Vec<ExtractedPage>> {
    check(cancellation)?;
    if bytes.is_empty() {
        return Err(PaperError::EmptyFile);
    }
    if bytes.len() as u64 > MAX_PDF_BYTES {
        return Err(PaperError::TooLarge);
    }
    std::panic::catch_unwind(AssertUnwindSafe(|| extract(bytes, cancellation)))
        .unwrap_or(Err(PaperError::CorruptPdf))
}

fn extract(bytes: &[u8], cancellation: &Cancellation) -> PaperResult<Vec<ExtractedPage>> {
    let mut doc = pdf_extract::Document::load_mem(bytes).map_err(|_| PaperError::CorruptPdf)?;
    if doc.is_encrypted() {
        doc.decrypt("").map_err(|_| PaperError::CorruptPdf)?;
    }
    // pdf-extract treats every Do XObject as a content stream, including image
    // pixels. Remove only image payloads from this in-memory extraction copy;
    // keep real Form XObjects and never rewrite the source PDF.
    for object in doc.objects.values_mut() {
        if let pdf_extract::Object::Stream(stream) = object
            && stream
                .dict
                .get(b"Subtype")
                .and_then(pdf_extract::Object::as_name)
                .ok()
                == Some(b"Image")
        {
            stream.set_plain_content(Vec::new());
        }
    }
    check(cancellation)?;
    let catalog = doc.catalog().map_err(|_| PaperError::CorruptPdf)?;
    let page_tree = doc
        .get_dict_in_dict(catalog, b"Pages")
        .map_err(|_| PaperError::CorruptPdf)?;
    let declared_pages = page_tree
        .get(b"Count")
        .and_then(pdf_extract::Object::as_i64)
        .map_err(|_| PaperError::CorruptPdf)?;
    let declared_pages = usize::try_from(declared_pages).map_err(|_| PaperError::CorruptPdf)?;
    if declared_pages > MAX_PDF_PAGES {
        return Err(PaperError::ExtractionLimit);
    }
    let pages = doc.get_pages();
    if pages.len() != declared_pages {
        return Err(PaperError::CorruptPdf);
    }
    let mut extracted = Vec::with_capacity(pages.len());
    let mut remaining = MAX_EXTRACTED_BYTES;
    for page_number in pages.into_keys() {
        check(cancellation)?;
        let mut buffer = TextBuffer {
            bytes: Vec::new(),
            remaining,
            limit_hit: false,
            cancellation,
        };
        let output = pdf_extract::output_doc_page(
            &doc,
            &mut pdf_extract::PlainTextOutput::new(&mut buffer as &mut dyn io::Write),
            page_number,
        );
        check(cancellation)?;
        if buffer.limit_hit {
            return Err(PaperError::ExtractionLimit);
        }
        output.map_err(|_| PaperError::CorruptPdf)?;
        remaining = buffer.remaining;
        let text = String::from_utf8(buffer.bytes).map_err(|_| PaperError::CorruptPdf)?;
        extracted.push(ExtractedPage { page_number, text });
    }
    Ok(extracted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn text_writer_never_partially_accepts_an_over_budget_character() {
        let cancellation = Cancellation::default();
        let mut buffer = TextBuffer {
            bytes: Vec::new(),
            remaining: 4,
            limit_hit: false,
            cancellation: &cancellation,
        };
        buffer.write_all("é".as_bytes()).unwrap();
        assert!(buffer.write_all("中".as_bytes()).is_err());
        assert!(buffer.limit_hit);
        assert_eq!(buffer.bytes, "é".as_bytes());
        assert_eq!(buffer.remaining, 2);
    }
}
