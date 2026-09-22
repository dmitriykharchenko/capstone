//! Embedded, app-owned graph storage. No legacy database is imported implicitly.
//!
//! Prepare operations before calling `CozoGraphStore::apply`. A batch is ONE final
//! graph transaction and seals its request token exactly once. Do not reuse that
//! token for another paper/store, or manufacture unlinked tokens for indexing.
//! Cross-store publication/reconciliation is an indexing integration concern.
//!
//! Schema v2 adds a monotonic physical fence and node-first reverse relations.
//! Marked v1 stores (including empty ones) require explicit rebuild; open never
//! converts them. Standalone v2 writes are external/untrusted, not journal-owned.
//! Future production readiness must refuse nonempty data without matching journal
//! authority. These isolated APIs do not enable a user-facing rebuild/reset flow.
mod identity;
mod incidence;
mod metadata;
mod publication;
mod recovery;
mod store;
mod tracking;
mod transaction;
mod types;

pub use identity::{normalize_node_name, stable_node_id};
pub use metadata::{
    MAX_METADATA_TEXT_CHARS, METADATA_SYSTEM_PROMPT, MetadataError, PaperMetadata,
    PaperMetadataExtractor, parse_metadata_output,
};
pub use recovery::{MAX_GRAPH_ARTIFACT_BYTES, MAX_RECOVERY_NODES, PaperRecoveryArtifact};
pub use store::CozoGraphStore;
pub use tracking::{
    GraphJournalAuthority, GraphPaperObservation, GraphPublication, GraphWriteKind,
};
pub use types::*;

use crate::{domain::GraphQuery, registry::BackendFuture, runtime::Cancellation};
use serde_json::Value;

pub trait GraphStore: Send + Sync {
    fn query(&self, query: GraphQuery, cancellation: Cancellation) -> BackendFuture<Value>;
    fn replace_paper(
        &self,
        paper_id: String,
        title: String,
        authors: Vec<String>,
        topics: Vec<String>,
        cancellation: Cancellation,
    ) -> BackendFuture<()>;
}

pub trait MetadataExtractor: Send + Sync {
    fn extract(&self, text: String, cancellation: Cancellation) -> BackendFuture<Value>;
}
