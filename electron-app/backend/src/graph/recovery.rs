//! Exact logical per-paper state, independent of display DTOs/counts. Shared
//! node labels are global: restoration must use the store's transactional guard.
use super::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MAX_GRAPH_ARTIFACT_BYTES: usize = 1024 * 1024;
pub const MAX_RECOVERY_NODES: usize = 4096;
const MAX_FIELD_BYTES: usize = 4096;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Body {
    version: u32,
    paper_id: String,
    #[serde(deserialize_with = "required_paper")]
    paper: Option<PaperGraph>,
}
fn required_paper<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<PaperGraph>, D::Error> {
    Option::deserialize(d)
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    body: Body,
    sha256: String,
}

/// Validated canonical v1 recovery artifact; never deserializes a PreparedPaper.
/// Digest covers identity/title, canonical author/topic IDs, labels and edges
/// (membership in this paper's arrays). Normalized names are uniquely derived
/// and checked against storage on capture. No authenticity/provider claim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaperRecoveryArtifact {
    body: Body,
    digest: String,
}
impl PaperRecoveryArtifact {
    pub fn from_prepared(paper: &PreparedPaper) -> GraphResult<Self> {
        validate_paper(paper.paper())?;
        Self::from_observed(paper.paper().paper_id.clone(), Some(paper.paper().clone()))
    }
    /// Absence is observable, but does not prove a journal's empty publication.
    pub fn absent(paper_id: String) -> GraphResult<Self> {
        Self::from_observed(paper_id, None)
    }
    pub(super) fn from_observed(paper_id: String, paper: Option<PaperGraph>) -> GraphResult<Self> {
        Self::validated(Body {
            version: 1,
            paper_id,
            paper,
        })
    }
    fn validated(body: Body) -> GraphResult<Self> {
        if body.version != 1 {
            return Err(GraphError::UnsupportedArtifact);
        }
        canonical_id(&body.paper_id)?;
        if let Some(p) = &body.paper {
            if p.paper_id != body.paper_id {
                return Err(GraphError::InvalidArtifact);
            }
            validate_paper(p)?;
        }
        let bytes = serde_json::to_vec(&body).map_err(|_| GraphError::InvalidArtifact)?;
        if bytes.len() > MAX_GRAPH_ARTIFACT_BYTES - 128 {
            return Err(GraphError::ArtifactTooLarge);
        }
        Ok(Self {
            body,
            digest: Sha256::digest(bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
        })
    }
    pub fn from_json(bytes: &[u8]) -> GraphResult<Self> {
        if bytes.len() > MAX_GRAPH_ARTIFACT_BYTES {
            return Err(GraphError::ArtifactTooLarge);
        }
        let wire: Envelope =
            serde_json::from_slice(bytes).map_err(|_| GraphError::InvalidArtifact)?;
        let artifact = Self::validated(wire.body)?;
        if artifact.digest != wire.sha256 {
            return Err(GraphError::DigestMismatch);
        }
        Ok(artifact)
    }
    pub fn to_json(&self) -> GraphResult<Vec<u8>> {
        serde_json::to_vec(&Envelope {
            body: self.body.clone(),
            sha256: self.digest.clone(),
        })
        .map_err(|_| GraphError::InvalidArtifact)
    }
    pub fn digest(&self) -> &str {
        &self.digest
    }
    pub fn paper_id(&self) -> &str {
        &self.body.paper_id
    }
    pub fn paper(&self) -> Option<&PaperGraph> {
        self.body.paper.as_ref()
    }
    /// Private conversion: callers must use CozoGraphStore::restore_paper so an
    /// ordinary Replace cannot bypass the recovery-specific shared-label guard.
    pub(super) fn restore_operation(&self) -> GraphResult<GraphOperation> {
        canonical_id(&self.body.paper_id)?;
        match &self.body.paper {
            Some(p) => Ok(GraphOperation::Replace(validate_paper(p)?)),
            None => Ok(GraphOperation::Remove(self.body.paper_id.clone())),
        }
    }
}
pub(super) fn canonical_id(id: &str) -> GraphResult<()> {
    field(id)?;
    if super::types::identifier(id).map_err(|_| GraphError::InvalidArtifact)? != id
        || id.starts_with("author:")
        || id.starts_with("topic:")
    {
        return Err(GraphError::InvalidArtifact);
    }
    Ok(())
}
fn field(s: &str) -> GraphResult<()> {
    if s.len() > MAX_FIELD_BYTES {
        return Err(GraphError::ArtifactTooLarge);
    }
    Ok(())
}
fn validate_paper(p: &PaperGraph) -> GraphResult<PreparedPaper> {
    canonical_id(&p.paper_id)?;
    field(&p.title)?;
    if p.authors.len() > MAX_RECOVERY_NODES || p.topics.len() > MAX_RECOVERY_NODES {
        return Err(GraphError::ArtifactTooLarge);
    }
    let mut bytes = p.title.len() + p.paper_id.len();
    for (id, name) in p
        .authors
        .iter()
        .map(|a| (&a.author_id, &a.name))
        .chain(p.topics.iter().map(|t| (&t.topic_id, &t.name)))
    {
        field(id)?;
        field(name)?;
        bytes = bytes.saturating_add(id.len()).saturating_add(name.len());
    }
    if bytes > (MAX_GRAPH_ARTIFACT_BYTES - 128) / 6 {
        return Err(GraphError::ArtifactTooLarge);
    }
    let prepared = PreparedPaper::new(
        &p.paper_id,
        &p.title,
        &p.authors.iter().map(|a| a.name.clone()).collect::<Vec<_>>(),
        &p.topics.iter().map(|t| t.name.clone()).collect::<Vec<_>>(),
    )
    .map_err(|_| GraphError::InvalidArtifact)?;
    // Reject rather than silently normalize malformed IDs, duplicate/reordered
    // nodes, noncanonical Unicode/whitespace, title or display labels.
    if prepared.paper() != p {
        return Err(GraphError::InvalidArtifact);
    }
    Ok(prepared)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recovery_graph_rejects_invalid_content_even_with_recomputed_digest() {
        for change in 0..8 {
            let prepared = PreparedPaper::new(
                "p",
                "Title",
                &["Alice".into(), "Zed".into()],
                &["RAG".into()],
            )
            .unwrap();
            let mut body = PaperRecoveryArtifact::from_prepared(&prepared)
                .unwrap()
                .body;
            let paper = body.paper.as_mut().unwrap();
            match change {
                0 => body.version = 2,
                1 => paper.authors[0].author_id = "author:bad".into(),
                2 => paper.authors[0].name = "Ａlice".into(),
                3 => paper.authors.push(paper.authors[0].clone()),
                4 => paper.authors.reverse(),
                5 => paper.title = " Title ".into(),
                6 => paper.paper_id = "q".into(),
                _ => paper.topics[0].topic_id = "topic:bad".into(),
            }
            let sha256 = Sha256::digest(serde_json::to_vec(&body).unwrap())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            let wire = serde_json::to_vec(&Envelope { body, sha256 }).unwrap();
            assert_ne!(
                PaperRecoveryArtifact::from_json(&wire).unwrap_err(),
                GraphError::DigestMismatch
            );
        }
    }
}
