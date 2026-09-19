//! Single-store comparison/restoration primitives, NOT a journal or publication
//! claim. A zero-row paper cannot distinguish absent, removed and published empty.
//! SQLite's existing decimal JSON payload needs serde_json/float_roundtrip for
//! exact finite-vector decoding. This is compatible with existing JSON, but
//! cannot recover precision already lost by an older decode/rewrite cycle.
use super::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MAX_PAPER_ARTIFACT_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_RECOVERY_CHUNKS: usize = 65_536;
pub const MAX_RECOVERY_VECTOR_VALUES: usize = 4 * 1024 * 1024;
const MAX_FIELD_BYTES: usize = 4096;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct WireChunk {
    id: String,
    paper_id: String,
    chunk_index: u32,
    filename: String,
    title: String,
    page: u32,
    text: String,
    // Canonical IEEE-754 bits avoid JSON parser decimal rounding and retain -0.
    #[serde(deserialize_with = "vector_bits")]
    vector_bits: Vec<String>,
}
impl WireChunk {
    fn from_chunk(c: &EmbeddedChunk) -> Self {
        Self {
            id: c.id.clone(),
            paper_id: c.paper_id.clone(),
            chunk_index: c.chunk_index,
            filename: c.filename.clone(),
            title: c.title.clone(),
            page: c.page,
            text: c.text.clone(),
            vector_bits: c
                .vector
                .iter()
                .map(|v| format!("{:016x}", v.to_bits()))
                .collect(),
        }
    }
    fn chunk(&self) -> RetrievalResult<EmbeddedChunk> {
        let vector = self
            .vector_bits
            .iter()
            .map(|s| {
                if s.len() != 16
                    || !s
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                {
                    return Err(RetrievalError::InvalidArtifact);
                }
                let bits =
                    u64::from_str_radix(s, 16).map_err(|_| RetrievalError::InvalidArtifact)?;
                Ok(f64::from_bits(bits))
            })
            .collect::<RetrievalResult<_>>()?;
        Ok(EmbeddedChunk {
            id: self.id.clone(),
            paper_id: self.paper_id.clone(),
            chunk_index: self.chunk_index,
            filename: self.filename.clone(),
            title: self.title.clone(),
            page: self.page,
            text: self.text.clone(),
            vector,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Body {
    version: u32,
    paper_id: String,
    identity: EmbeddingIdentity,
    #[serde(deserialize_with = "wire_chunks")]
    chunks: Vec<WireChunk>,
}

fn vector_bits<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    struct Bits;
    impl<'de> serde::de::Visitor<'de> for Bits {
        type Value = Vec<String>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("bounded vector bits")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut a: A,
        ) -> Result<Self::Value, A::Error> {
            let mut out = Vec::new();
            while let Some(v) = a.next_element::<String>()? {
                if out.len() == 65_536 || v.len() != 16 {
                    return Err(serde::de::Error::custom("invalid vector bounds"));
                }
                out.push(v);
            }
            Ok(out)
        }
    }
    d.deserialize_seq(Bits)
}
fn wire_chunks<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<WireChunk>, D::Error> {
    struct Chunks;
    impl<'de> serde::de::Visitor<'de> for Chunks {
        type Value = Vec<WireChunk>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("bounded chunks")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut a: A,
        ) -> Result<Self::Value, A::Error> {
            let mut out = Vec::new();
            let mut values = 0usize;
            while let Some(v) = a.next_element::<WireChunk>()? {
                values = values.saturating_add(v.vector_bits.len());
                if out.len() == MAX_RECOVERY_CHUNKS || values > MAX_RECOVERY_VECTOR_VALUES {
                    return Err(serde::de::Error::custom("invalid chunk bounds"));
                }
                out.push(v);
            }
            Ok(out)
        }
    }
    d.deserialize_seq(Chunks)
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    body: Body,
    sha256: String,
}

/// Immutable validated v1 content; no Deserialize implementation can bypass new.
/// Canonical order is (chunk_index,id), not SQLite physical insertion sequence.
/// Digests cover version, paper/embedding identity, all provenance/text/vector
/// fields. They are comparison aids, not authenticity or provider attestation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaperRecoveryArtifact {
    body: Body,
    digest: String,
}
impl PaperRecoveryArtifact {
    pub fn from_prepared(paper: &PreparedPaper) -> RetrievalResult<Self> {
        // Bound BEFORE allocating the wire copy, including vector hex strings.
        if paper.chunks.len() > MAX_RECOVERY_CHUNKS {
            return Err(RetrievalError::ArtifactTooLarge);
        }
        let mut budget = paper
            .paper_id
            .len()
            .saturating_add(paper.identity.model.len());
        let mut values = 0usize;
        for c in &paper.chunks {
            values = values.saturating_add(c.vector.len());
            for s in [&c.id, &c.paper_id, &c.filename, &c.title, &c.text] {
                budget = budget.saturating_add(s.len().saturating_mul(6));
            }
            budget = budget
                .saturating_add(c.vector.len().saturating_mul(19))
                .saturating_add(256);
        }
        if budget > MAX_PAPER_ARTIFACT_BYTES - 128 || values > MAX_RECOVERY_VECTOR_VALUES {
            return Err(RetrievalError::ArtifactTooLarge);
        }
        let mut chunks: Vec<_> = paper.chunks.iter().map(WireChunk::from_chunk).collect();
        chunks.sort_by(|a, b| (a.chunk_index, &a.id).cmp(&(b.chunk_index, &b.id)));
        Self::validated(Body {
            version: 1,
            paper_id: paper.paper_id.clone(),
            identity: paper.identity.clone(),
            chunks,
        })
    }
    /// No rows is the entire observable state, not evidence of empty publication.
    pub fn no_rows(paper_id: String, identity: EmbeddingIdentity) -> RetrievalResult<Self> {
        Self::from_prepared(&PreparedPaper::new(paper_id, identity, vec![])?)
    }
    fn validated(body: Body) -> RetrievalResult<Self> {
        if body.version != 1 {
            return Err(RetrievalError::UnsupportedArtifact);
        }
        field(&body.paper_id)?;
        field(&body.identity.model)?;
        if body.chunks.len() > MAX_RECOVERY_CHUNKS {
            return Err(RetrievalError::ArtifactTooLarge);
        }
        let mut values = 0usize;
        let mut text_bytes = 0usize;
        for c in &body.chunks {
            for s in [&c.id, &c.paper_id, &c.filename, &c.title] {
                field(s)?;
            }
            values = values.saturating_add(c.vector_bits.len());
            text_bytes = text_bytes.saturating_add(c.text.len());
            if c.text.len() > crate::papers::chunking::DEFAULT_MAX_CHUNK_BYTES
                || text_bytes > crate::papers::MAX_EXTRACTED_BYTES
                || values > MAX_RECOVERY_VECTOR_VALUES
            {
                return Err(RetrievalError::ArtifactTooLarge);
            }
        }
        if body
            .chunks
            .windows(2)
            .any(|w| w[0].chunk_index >= w[1].chunk_index)
        {
            return Err(RetrievalError::InvalidArtifact);
        }
        let artifact = Self {
            body,
            digest: String::new(),
        };
        artifact.prepared()?;
        let bytes =
            serde_json::to_vec(&artifact.body).map_err(|_| RetrievalError::InvalidArtifact)?;
        // Leave room for the fixed envelope and digest.
        if bytes.len() > MAX_PAPER_ARTIFACT_BYTES - 128 {
            return Err(RetrievalError::ArtifactTooLarge);
        }
        Ok(Self {
            digest: Sha256::digest(bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
            ..artifact
        })
    }
    pub fn from_json(bytes: &[u8]) -> RetrievalResult<Self> {
        if bytes.len() > MAX_PAPER_ARTIFACT_BYTES {
            return Err(RetrievalError::ArtifactTooLarge);
        }
        // Parse directly to strict structs: Value would erase duplicate keys.
        let wire: Envelope =
            serde_json::from_slice(bytes).map_err(|_| RetrievalError::InvalidArtifact)?;
        let artifact = Self::validated(wire.body)?;
        if artifact.digest != wire.sha256 {
            return Err(RetrievalError::DigestMismatch);
        }
        Ok(artifact)
    }
    pub fn to_json(&self) -> RetrievalResult<Vec<u8>> {
        serde_json::to_vec(&Envelope {
            body: self.body.clone(),
            sha256: self.digest.clone(),
        })
        .map_err(|_| RetrievalError::InvalidArtifact)
    }
    pub fn digest(&self) -> &str {
        &self.digest
    }
    pub fn paper_id(&self) -> &str {
        &self.body.paper_id
    }
    pub fn identity(&self) -> &EmbeddingIdentity {
        &self.body.identity
    }
    pub fn has_rows(&self) -> bool {
        !self.body.chunks.is_empty()
    }
    fn prepared(&self) -> RetrievalResult<PreparedPaper> {
        PreparedPaper::new(
            self.body.paper_id.clone(),
            self.body.identity.clone(),
            self.body
                .chunks
                .iter()
                .map(WireChunk::chunk)
                .collect::<RetrievalResult<_>>()?,
        )
        .map_err(|_| RetrievalError::InvalidArtifact)
    }
    /// Revalidate at the existing apply boundary, with no embedding/provider call.
    /// Caller separately includes RebuildLexical and an expected revision as needed.
    pub fn restore_operation(
        &self,
        identity: &EmbeddingIdentity,
    ) -> RetrievalResult<PreparedOperation> {
        if identity != &self.body.identity {
            return Err(RetrievalError::ReindexRequired);
        }
        let prepared = self.prepared()?;
        Ok(if self.has_rows() {
            PreparedOperation::ReplacePaper(prepared)
        } else {
            PreparedOperation::DeletePaper(prepared.paper_id)
        })
    }
}
fn field(s: &str) -> RetrievalResult<()> {
    if s.len() > MAX_FIELD_BYTES {
        return Err(RetrievalError::ArtifactTooLarge);
    }
    super::types::nonempty(s).map_err(|_| RetrievalError::InvalidArtifact)?;
    if s.chars().any(char::is_control) {
        return Err(RetrievalError::InvalidArtifact);
    }
    Ok(())
}

/// Identity (in artifact), revision, lexical state and rows from ONE transaction.
/// A stale lexical cache is not repaired by this read. Revision/lexical are not
/// artifact content: a restore may advance the revision. Not a corpus backup;
/// exchange rows and global insertion-order tie breaking are outside this state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaperObservation {
    pub revision: i64,
    pub lexical: LexicalStatus,
    pub artifact: PaperRecoveryArtifact,
}

impl VectorStore {
    /// Uses the existing corpus snapshot (same memory/work characteristics), not
    /// raced status/read_paper calls. Run synchronous work on a joined worker.
    pub fn observe_paper(
        &self,
        paper_id: &str,
        c: &crate::runtime::Cancellation,
    ) -> RetrievalResult<PaperObservation> {
        field(paper_id)?;
        let snapshot = self.snapshot(c)?;
        let prepared = PreparedPaper::new(
            paper_id.into(),
            snapshot.status.identity,
            snapshot
                .chunks
                .into_iter()
                .filter(|c| c.paper_id == paper_id)
                .collect(),
        )?;
        let artifact = PaperRecoveryArtifact::from_prepared(&prepared)?;
        super::types::check(c)?;
        Ok(PaperObservation {
            revision: snapshot.status.revision,
            lexical: snapshot.status.lexical,
            artifact,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifact() -> PaperRecoveryArtifact {
        PaperRecoveryArtifact::from_prepared(
            &PreparedPaper::new(
                "p".into(),
                EmbeddingIdentity {
                    model: "fake".into(),
                    dimension: 1,
                },
                vec![EmbeddedChunk {
                    id: "p:0".into(),
                    paper_id: "p".into(),
                    chunk_index: 0,
                    filename: "p.pdf".into(),
                    title: "Title".into(),
                    page: 1,
                    text: "text".into(),
                    vector: vec![0.25],
                }],
            )
            .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn recovery_rejects_invalid_content_even_with_recomputed_digest() {
        for change in 0..9 {
            let mut body = artifact().body;
            match change {
                0 => body.version = 2,
                1 => body.chunks[0].vector_bits[0] = "7ff0000000000000".into(),
                2 => body.chunks[0].vector_bits[0] = "fff8000000000000".into(),
                3 => body.identity.dimension = 2,
                4 => body.chunks[0].paper_id = "q".into(),
                5 => body.chunks.push(body.chunks[0].clone()),
                6 => body.chunks[0].page = 0,
                7 => body.chunks[0].text = " ".into(),
                _ => body.chunks[0].vector_bits[0] = "BFD0000000000000".into(),
            }
            let sha256 = Sha256::digest(serde_json::to_vec(&body).unwrap())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            let wire = serde_json::to_vec(&Envelope { body, sha256 }).unwrap();
            let error = PaperRecoveryArtifact::from_json(&wire).unwrap_err();
            assert_ne!(error, RetrievalError::DigestMismatch);
        }
    }

    #[test]
    fn recovery_parsing_and_construction_enforce_collection_budgets() {
        let mut body = artifact().body;
        body.chunks[0].vector_bits = vec!["0000000000000000".into(); 65_537];
        let wire = serde_json::to_vec(&Envelope {
            body,
            sha256: String::new(),
        })
        .unwrap();
        assert_eq!(
            PaperRecoveryArtifact::from_json(&wire),
            Err(RetrievalError::InvalidArtifact)
        );
        let mut body = artifact().body;
        body.chunks = vec![body.chunks[0].clone(); MAX_RECOVERY_CHUNKS + 1];
        let wire = serde_json::to_vec(&Envelope {
            body,
            sha256: String::new(),
        })
        .unwrap();
        assert_eq!(
            PaperRecoveryArtifact::from_json(&wire),
            Err(RetrievalError::InvalidArtifact)
        );
        let mut c = artifact().prepared().unwrap().chunks.remove(0);
        c.vector = vec![0.0; 65_536];
        let chunks = (0..65)
            .map(|index| {
                let mut c = c.clone();
                c.chunk_index = index;
                c.id = format!("p:{index}");
                c
            })
            .collect();
        let prepared = PreparedPaper::new(
            "p".into(),
            EmbeddingIdentity {
                model: "fake".into(),
                dimension: 65_536,
            },
            chunks,
        )
        .unwrap();
        assert_eq!(
            PaperRecoveryArtifact::from_prepared(&prepared),
            Err(RetrievalError::ArtifactTooLarge)
        );
    }
}
