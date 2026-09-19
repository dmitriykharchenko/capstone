use super::identity::{compact, normalize_node_name, stable_node_id};
use crate::{domain::GraphQuery, protocol::ErrorCode};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MAX_NEIGHBORHOOD_DEPTH: u32 = 5;
pub type GraphResult<T> = Result<T, GraphError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphError {
    InvalidInput,
    InvalidPath,
    InvalidSchema,
    Locked,
    Closed,
    Storage,
    Cancelled,
    AlreadySealed,
    UnsupportedArtifact,
    InvalidArtifact,
    ArtifactTooLarge,
    DigestMismatch,
    SharedNodeConflict,
}
impl std::fmt::Display for GraphError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Graph operation failed ({self:?}).")
    }
}
impl std::error::Error for GraphError {}
impl From<GraphError> for ErrorCode {
    fn from(e: GraphError) -> Self {
        match e {
            GraphError::InvalidInput | GraphError::InvalidPath => Self::InvalidParams,
            GraphError::Cancelled => Self::Cancelled,
            GraphError::Locked => Self::Busy,
            _ => Self::Internal,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Author,
    Paper,
    Topic,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    AuthoredBy,
    HasTopic,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuthorNode {
    pub author_id: String,
    pub name: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TopicNode {
    pub topic_id: String,
    pub name: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PaperGraph {
    pub paper_id: String,
    pub title: String,
    pub authors: Vec<AuthorNode>,
    pub topics: Vec<TopicNode>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GraphNode {
    pub node_id: String,
    pub kind: NodeKind,
    pub label: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GraphEdge {
    pub source_id: String,
    pub target_id: String,
    pub kind: EdgeKind,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GraphNeighborhood {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "query", rename_all = "snake_case")]
pub enum GraphQueryData {
    PaperDetails { paper: Option<PaperGraph> },
    PapersByAuthor { papers: Vec<PaperGraph> },
    PapersByTopic { papers: Vec<PaperGraph> },
    TopicsByAuthor { topics: Vec<TopicNode> },
    Coauthors { coauthors: Vec<AuthorNode> },
    Neighborhood { neighborhood: GraphNeighborhood },
}
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct GraphResponse {
    pub enabled: bool,
    pub available: bool,
    #[serde(flatten)]
    pub data: GraphQueryData,
}
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct GraphStatus {
    pub papers: usize,
    pub authors: usize,
    pub topics: usize,
    pub edges: usize,
}

/// Immutable, validated, normalized replacement. Preparation has no store effects.
#[derive(Clone, Debug)]
pub struct PreparedPaper(pub(super) PaperGraph);
impl PreparedPaper {
    pub fn new(
        paper_id: &str,
        title: &str,
        authors: &[String],
        topics: &[String],
    ) -> GraphResult<Self> {
        let paper_id = identifier(paper_id)?;
        // Namespaces must remain disjoint in a graph keyed by node_id.
        if paper_id.starts_with("author:") || paper_id.starts_with("topic:") {
            return Err(GraphError::InvalidInput);
        }
        let nodes = |values: &[String], kind| -> GraphResult<Vec<(String, String)>> {
            let mut names = BTreeMap::new();
            for value in values {
                let name = compact(value)?;
                names.entry(normalize_node_name(&name)?).or_insert(name);
            }
            if names.is_empty() {
                return Err(GraphError::InvalidInput);
            }
            names
                .into_values()
                .map(|name| Ok((stable_node_id(kind, &name)?, name)))
                .collect()
        };
        Ok(Self(PaperGraph {
            paper_id,
            title: compact(title)?,
            authors: nodes(authors, NodeKind::Author)?
                .into_iter()
                .map(|(author_id, name)| AuthorNode { author_id, name })
                .collect(),
            topics: nodes(topics, NodeKind::Topic)?
                .into_iter()
                .map(|(topic_id, name)| TopicNode { topic_id, name })
                .collect(),
        }))
    }
    pub fn paper(&self) -> &PaperGraph {
        &self.0
    }
}

#[derive(Clone, Debug)]
pub enum GraphOperation {
    Replace(PreparedPaper),
    Remove(String),
    Reset,
}
impl GraphOperation {
    pub(super) fn validate(self) -> GraphResult<Self> {
        match self {
            Self::Remove(id) => Ok(Self::Remove(identifier(&id)?)),
            other => Ok(other),
        }
    }
}

// IDs are opaque strings in the legacy wire contract, not necessarily hashes or
// filenames. Unknown nonblank IDs return empty results; reject control payloads.
pub(super) fn identifier(value: &str) -> GraphResult<String> {
    if value.chars().any(|c| c.is_control() && !c.is_whitespace()) {
        return Err(GraphError::InvalidInput);
    }
    compact(value)
}
pub(super) fn validate_query(query: GraphQuery) -> GraphResult<GraphQuery> {
    Ok(match query {
        GraphQuery::PaperDetails { paper_id } => GraphQuery::PaperDetails {
            paper_id: identifier(&paper_id)?,
        },
        GraphQuery::PapersByAuthor { author_id } => GraphQuery::PapersByAuthor {
            author_id: identifier(&author_id)?,
        },
        GraphQuery::PapersByTopic { topic_id } => GraphQuery::PapersByTopic {
            topic_id: identifier(&topic_id)?,
        },
        GraphQuery::TopicsByAuthor { author_id } => GraphQuery::TopicsByAuthor {
            author_id: identifier(&author_id)?,
        },
        GraphQuery::Coauthors { author_id } => GraphQuery::Coauthors {
            author_id: identifier(&author_id)?,
        },
        GraphQuery::Neighborhood { node_id, max_depth } => {
            if max_depth > MAX_NEIGHBORHOOD_DEPTH {
                return Err(GraphError::InvalidInput);
            }
            GraphQuery::Neighborhood {
                node_id: identifier(&node_id)?,
                max_depth,
            }
        }
    })
}
