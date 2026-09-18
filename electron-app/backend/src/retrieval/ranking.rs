use super::{
    EmbeddedChunk, PaperMatch, RetrievalError, RetrievalResult, SearchRequest,
    types::{check, python_whitespace},
};
use crate::{
    domain::{Chunk, RetrievalScope, RetrievalStrategy, SourceRecord},
    runtime::Cancellation,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Python str.lower followed by ASCII [a-z0-9]+ (NOT Unicode word tokens,
/// case-folding or accent normalization). Unicode final-sigma differences cannot
/// affect the retained ASCII tokens; expanding U+0130 does affect them.
pub fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_ascii_lowercase() && !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Bm25Index {
    frequencies: Vec<BTreeMap<String, usize>>,
    lengths: Vec<usize>,
    document_frequency: BTreeMap<String, usize>,
}
impl Bm25Index {
    pub(super) fn build<'a>(
        documents: impl Iterator<Item = &'a str>,
        c: &Cancellation,
    ) -> RetrievalResult<Self> {
        let mut index = Self {
            frequencies: vec![],
            lengths: vec![],
            document_frequency: BTreeMap::new(),
        };
        for document in documents {
            check(c)?;
            let tokens = tokenize(document);
            let mut frequencies = BTreeMap::new();
            for token in &tokens {
                *frequencies.entry(token.clone()).or_default() += 1;
            }
            for token in frequencies.keys() {
                *index.document_frequency.entry(token.clone()).or_default() += 1;
            }
            index.lengths.push(tokens.len());
            index.frequencies.push(frequencies);
        }
        Ok(index)
    }
    pub(super) fn score(&self, query: &str, c: &Cancellation) -> RetrievalResult<Vec<f64>> {
        let terms: BTreeSet<_> = tokenize(query).into_iter().collect();
        let count = self.lengths.len();
        let total: usize = self.lengths.iter().sum();
        if total == 0 || terms.is_empty() {
            return Ok(vec![0.0; count]);
        }
        let avg = total as f64 / count as f64;
        self.frequencies
            .iter()
            .zip(&self.lengths)
            .map(|(freqs, length)| {
                check(c)?;
                let mut score = 0.0;
                for term in &terms {
                    let tf = *freqs.get(term).unwrap_or(&0) as f64;
                    if tf == 0.0 {
                        continue;
                    }
                    let df = *self
                        .document_frequency
                        .get(term)
                        .ok_or(RetrievalError::InvalidStore)? as f64;
                    let idf = (1.0 + (count as f64 - df + 0.5) / (df + 0.5)).ln();
                    score += idf * (tf * 2.5) / (tf + 1.5 * (0.25 + 0.75 * *length as f64 / avg));
                }
                Ok(score)
            })
            .collect()
    }
}
pub fn bm25_scores(query: &str, documents: &[&str], c: &Cancellation) -> RetrievalResult<Vec<f64>> {
    Bm25Index::build(documents.iter().copied(), c)?.score(query, c)
}

pub fn squared_l2(a: &[f64], b: &[f64]) -> RetrievalResult<f64> {
    if a.is_empty() || a.len() != b.len() {
        return Err(RetrievalError::ReindexRequired);
    }
    let mut sum = 0.0;
    for (a, b) in a.iter().zip(b) {
        if !a.is_finite() || !b.is_finite() {
            return Err(RetrievalError::InvalidInput);
        }
        let delta = a - b;
        sum += delta * delta;
    }
    if !sum.is_finite() {
        return Err(RetrievalError::InvalidInput);
    }
    Ok(sum)
}
// Decimal formatting rounds the original binary float, ties-to-even, like
// Python round(x,4); multiplying first by 10000 can introduce false halfway ties.
fn round4(value: f64) -> f64 {
    format!("{value:.4}").parse().expect("finite decimal")
}
pub fn similarity(distance: f64) -> RetrievalResult<f64> {
    if !distance.is_finite() || distance < 0.0 {
        return Err(RetrievalError::InvalidInput);
    }
    Ok(round4(1.0 / (1.0 + distance)))
}
pub fn hybrid_scores(similarities: &[f64], lexical: &[f64]) -> RetrievalResult<Vec<f64>> {
    if similarities.len() != lexical.len()
        || similarities
            .iter()
            .chain(lexical)
            .any(|v| !v.is_finite() || *v < 0.0)
    {
        return Err(RetrievalError::InvalidInput);
    }
    if similarities.is_empty() {
        return Ok(vec![]);
    }
    let low = similarities.iter().copied().fold(f64::INFINITY, f64::min);
    let high = similarities
        .iter()
        .copied()
        .fold(f64::NEG_INFINITY, f64::max);
    let lexical_high = lexical.iter().copied().fold(0.0, f64::max);
    Ok(similarities
        .iter()
        .zip(lexical)
        .map(|(v, l)| {
            round4(
                0.5 * if high == low {
                    1.0
                } else {
                    (v - low) / (high - low)
                } + 0.5
                    * if lexical_high <= 0.0 {
                        0.0
                    } else {
                        l / lexical_high
                    },
            )
        })
        .collect())
}
pub(super) fn snippet(text: &str) -> String {
    // Python's str.split also treats ASCII information separators as whitespace.
    let text = text
        .split(python_whitespace)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if text.chars().count() <= 240 {
        text
    } else {
        format!("{}…", text.chars().take(240).collect::<String>().trim_end())
    }
}
fn key(c: &EmbeddedChunk) -> (&str, u32, &str, u32, &str) {
    (&c.paper_id, c.chunk_index, &c.filename, c.page, &c.id)
}
pub(super) fn vector_order(
    chunks: &[EmbeddedChunk],
    vector: &[f64],
    c: &Cancellation,
) -> RetrievalResult<Vec<(usize, f64)>> {
    let mut order = chunks
        .iter()
        .enumerate()
        .map(|(i, chunk)| {
            check(c)?;
            Ok((i, squared_l2(vector, &chunk.vector)?))
        })
        .collect::<RetrievalResult<Vec<_>>>()?;
    // Stable insertion order for exact distance ties; never sort rounded scores.
    order.sort_by(|a, b| a.1.total_cmp(&b.1));
    check(c)?;
    Ok(order)
}
pub(super) fn candidate_scores(
    chunks: &[EmbeddedChunk],
    query: &str,
    vector: &[f64],
    limit: usize,
    c: &Cancellation,
) -> RetrievalResult<Vec<(usize, f64)>> {
    let mut order = vector_order(chunks, vector, c)?;
    order.truncate(limit.saturating_mul(5).min(50));
    let lexical = Bm25Index::build(order.iter().map(|(i, _)| chunks[*i].text.as_str()), c)?
        .score(query, c)?;
    let sims = order
        .iter()
        .map(|(_, d)| similarity(*d))
        .collect::<RetrievalResult<Vec<_>>>()?;
    Ok(order
        .into_iter()
        .zip(hybrid_scores(&sims, &lexical)?)
        .map(|((i, _), s)| (i, s))
        .collect())
}
pub(super) fn retrieve(
    snapshot: &super::IndexSnapshot,
    request: &SearchRequest,
    vector: Option<&[f64]>,
    c: &Cancellation,
) -> RetrievalResult<Vec<Chunk>> {
    request.validate()?;
    check(c)?;
    let chunks = &snapshot.chunks;
    let chat = request.scope == RetrievalScope::ChatCandidates;
    let limit = if chat {
        request.limit
    } else {
        request.limit.min(5)
    };
    let mut scored = if chat {
        candidate_scores(
            chunks,
            &request.query,
            vector.ok_or(RetrievalError::InvalidInput)?,
            limit,
            c,
        )?
    } else if request.strategy == RetrievalStrategy::Bm25 {
        snapshot
            .lexical
            .score(&request.query, c)?
            .into_iter()
            .enumerate()
            .collect()
    } else {
        let mut order = vector_order(chunks, vector.ok_or(RetrievalError::InvalidInput)?, c)?;
        if request.strategy == RetrievalStrategy::Vector {
            order.truncate(limit);
        }
        let similarities = order
            .iter()
            .map(|(_, d)| similarity(*d))
            .collect::<RetrievalResult<Vec<_>>>()?;
        let scores = if request.strategy == RetrievalStrategy::Hybrid {
            let corpus_scores = snapshot.lexical.score(&request.query, c)?;
            let lexical = order
                .iter()
                .map(|(i, _)| corpus_scores[*i])
                .collect::<Vec<_>>();
            hybrid_scores(&similarities, &lexical)?
        } else {
            similarities
        };
        order
            .into_iter()
            .zip(scores)
            .map(|((i, _), s)| (i, s))
            .collect()
    };
    if chat {
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    } else if request.strategy != RetrievalStrategy::Vector {
        scored.sort_by(|a, b| {
            b.1.total_cmp(&a.1)
                .then_with(|| key(&chunks[a.0]).cmp(&key(&chunks[b.0])))
        });
    }
    check(c)?;
    Ok(scored
        .into_iter()
        .take(limit)
        .enumerate()
        .map(|(i, (idx, score))| {
            let chunk = &chunks[idx];
            Chunk {
                text: chunk.text.clone(),
                source: SourceRecord {
                    rank: i as u32 + 1,
                    score,
                    paper_id: chunk.paper_id.clone(),
                    chunk_index: chunk.chunk_index,
                    filename: chunk.filename.clone(),
                    title: chunk.title.clone(),
                    page: chunk.page,
                    snippet: snippet(&chunk.text),
                },
            }
        })
        .collect())
}
pub(super) fn group(
    chunks: &[EmbeddedChunk],
    scored: Vec<(usize, f64)>,
    limit: usize,
) -> Vec<PaperMatch> {
    let mut best: Vec<PaperMatch> = vec![];
    for (i, score) in scored {
        let chunk = &chunks[i];
        let found = best.iter().position(|m| m.paper_id == chunk.paper_id);
        if found.is_some_and(|i| best[i].score >= score) {
            continue;
        }
        let m = PaperMatch {
            paper_id: chunk.paper_id.clone(),
            title: chunk.title.clone(),
            filename: chunk.filename.clone(),
            page: chunk.page,
            snippet: snippet(&chunk.text),
            score,
        };
        if let Some(i) = found {
            best[i] = m;
        } else {
            best.push(m);
        }
    }
    best.sort_by(|a, b| b.score.total_cmp(&a.score));
    best.truncate(limit);
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retrieval_decimal_rounding_uses_original_binary_float() {
        assert_eq!(round4(0.00005), 0.0001);
        assert_eq!(round4(0.00015), 0.0001);
        assert_eq!(round4(0.06245), 0.0624);
        assert_eq!(round4(0.06255), 0.0625);
    }
    #[test]
    fn retrieval_group_tie_keeps_first_paper_occurrence_even_if_best_chunk_is_later() {
        let record = |paper: &str, page| EmbeddedChunk {
            id: format!("{paper}:{page}"),
            paper_id: paper.into(),
            chunk_index: page - 1,
            filename: format!("{paper}.pdf"),
            title: paper.into(),
            page,
            text: " full\n text\u{1e}here ".into(),
            vector: vec![0.0],
        };
        let records = vec![
            record("z", 1),
            record("a", 1),
            record("z", 2),
            record("z", 3),
        ];
        let result = group(&records, vec![(0, 0.2), (1, 0.8), (2, 0.8), (3, 0.8)], 5);
        assert_eq!(
            result
                .iter()
                .map(|r| r.paper_id.as_str())
                .collect::<Vec<_>>(),
            ["z", "a"]
        );
        assert_eq!(result[0].page, 2);
        assert_eq!(result[0].snippet, "full text here");
    }
}
