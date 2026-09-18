use super::{ProviderError, ProviderResult, config::MAX_FRAME_BYTES};

/// Byte-oriented SSE framing handles split UTF-8, CRLF (including split CR/LF),
/// bare CR/LF, comments, and multiline data without retaining the whole stream.
#[derive(Default)]
pub(crate) struct Decoder {
    line: Vec<u8>,
    data: String,
    frame_bytes: usize,
    skip_lf: bool,
    started: bool,
}

impl Decoder {
    pub fn push(&mut self, byte: u8) -> ProviderResult<Option<String>> {
        if self.skip_lf {
            self.skip_lf = false;
            if byte == b'\n' {
                return Ok(None);
            }
        }
        self.frame_bytes += 1;
        if self.frame_bytes > MAX_FRAME_BYTES {
            return Err(ProviderError::TooLarge);
        }
        if byte != b'\n' && byte != b'\r' {
            self.line.push(byte);
            return Ok(None);
        }
        self.skip_lf = byte == b'\r';
        let line = std::str::from_utf8(&self.line).map_err(|_| ProviderError::MalformedResponse)?;
        let line = if self.started {
            line
        } else {
            line.trim_start_matches('\u{feff}')
        };
        self.started = true;
        if line.is_empty() {
            self.frame_bytes = 0;
            if !self.data.is_empty() {
                self.data.pop(); // SSE joins data lines with LF, removes final LF.
                let data = std::mem::take(&mut self.data);
                self.line.clear();
                return Ok(Some(data));
            }
        } else if let Some(value) = line.strip_prefix("data:") {
            self.data.push_str(value.strip_prefix(' ').unwrap_or(value));
            self.data.push('\n');
        } else if line == "data" {
            self.data.push('\n');
        }
        self.line.clear();
        Ok(None)
    }
}
