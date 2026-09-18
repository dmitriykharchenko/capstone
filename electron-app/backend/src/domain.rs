use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SourceRecord {
    pub rank: u32,
    pub paper_id: String,
    pub chunk_index: u32,
    pub filename: String,
    pub title: String,
    pub page: u32,
    pub score: f64,
    pub snippet: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Chunk {
    pub source: SourceRecord,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
    pub exchange_id: Option<i64>,
    pub sources: Vec<SourceRecord>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalStrategy {
    Vector,
    Bm25,
    Hybrid,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalScope {
    // Both Chat and adaptive RAG Evals use this same bounded candidate path.
    ChatCandidates,
    // Retains the reusable legacy explicit-strategy/full-corpus interface.
    FullCorpus,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RagResult {
    pub reply: String,
    pub sources: Vec<SourceRecord>,
    pub chunks: Vec<Chunk>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaperInfo {
    pub id: String,
    pub filename: String,
    pub title: String,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PdfInfo {
    pub id: String,
    pub filename: String,
    pub mime_type: String,
    pub total_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvaluationCase {
    pub question: String,
    pub grading_notes: String,
    pub sources: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "query", rename_all = "snake_case", deny_unknown_fields)]
pub enum GraphQuery {
    PaperDetails { paper_id: String },
    PapersByAuthor { author_id: String },
    PapersByTopic { topic_id: String },
    TopicsByAuthor { author_id: String },
    Coauthors { author_id: String },
    Neighborhood { node_id: String, max_depth: u32 },
}
