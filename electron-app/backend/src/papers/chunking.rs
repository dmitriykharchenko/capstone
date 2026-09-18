//! Legacy paragraph boundaries and sentence-preferred UTF-8 byte budgets.

pub const DEFAULT_MAX_CHUNK_BYTES: usize = 8_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChunkingError {
    ZeroBudget,
    CharacterExceedsBudget,
}

// Python str.isspace also includes these four ASCII separators.
fn whitespace(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

fn trim(text: &str) -> &str {
    text.trim_matches(whitespace)
}

/// Blank lines separate paragraphs; internal newlines and spacing are unchanged.
pub fn split_into_paragraphs(text: &str) -> Vec<String> {
    let mut paragraphs = Vec::new();
    let mut chars = text.char_indices().peekable();
    let mut start = 0;
    while let Some((at, c)) = chars.next() {
        if c != '\r' && c != '\n' {
            continue;
        }
        if c == '\r' && chars.peek().is_some_and(|(_, c)| *c == '\n') {
            chars.next();
        }
        while chars
            .peek()
            .is_some_and(|(_, c)| whitespace(*c) && !matches!(c, '\r' | '\n'))
        {
            chars.next();
        }
        if !chars.peek().is_some_and(|(_, c)| matches!(c, '\r' | '\n')) {
            continue;
        }
        let paragraph = trim(&text[start..at]);
        if !paragraph.is_empty() {
            paragraphs.push(paragraph.to_owned());
        }
        while chars.peek().is_some_and(|(_, c)| whitespace(*c)) {
            chars.next();
        }
        start = chars.peek().map_or(text.len(), |(i, _)| *i);
    }
    let paragraph = trim(&text[start..]);
    if !paragraph.is_empty() {
        paragraphs.push(paragraph.to_owned());
    }
    paragraphs
}

/// Preserve small chunks exactly; prefer the last sentence end fitting the budget.
pub fn split_oversized_chunk(text: &str, max_bytes: usize) -> Result<Vec<String>, ChunkingError> {
    if max_bytes == 0 {
        return Err(ChunkingError::ZeroBudget);
    }
    if trim(text).is_empty() {
        return Ok(Vec::new());
    }
    if text.len() <= max_bytes {
        return Ok(vec![text.to_owned()]);
    }
    let mut boundaries = Vec::new();
    for (at, c) in text.char_indices() {
        if !matches!(c, '.' | '!' | '?') {
            continue;
        }
        let mut end = at + 1;
        for c in text[end..].chars() {
            if !matches!(c, '"' | '\'' | '’' | '”' | ')' | ']') {
                break;
            }
            end += c.len_utf8();
        }
        if text[end..].chars().next().is_none_or(whitespace) {
            boundaries.push(end);
        }
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        start += text[start..].len() - text[start..].trim_start_matches(whitespace).len();
        if start == text.len() {
            break;
        }
        let mut hard_end = start.saturating_add(max_bytes).min(text.len());
        while !text.is_char_boundary(hard_end) {
            hard_end -= 1;
        }
        if hard_end == start {
            return Err(ChunkingError::CharacterExceedsBudget);
        }
        let position = boundaries.partition_point(|end| *end <= hard_end);
        let end = position
            .checked_sub(1)
            .map(|i| boundaries[i])
            .filter(|end| *end > start)
            .unwrap_or(hard_end);
        let chunk = trim(&text[start..end]);
        if !chunk.is_empty() {
            chunks.push(chunk.to_owned());
        }
        start = end;
    }
    Ok(chunks)
}
