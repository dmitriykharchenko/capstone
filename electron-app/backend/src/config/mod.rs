use std::path::PathBuf;

// Config/history owner validates these before opening anything. Paths are not
// accepted from renderer requests; legacy history is a read-only import source.
pub struct StoragePaths {
    pub data_root: PathBuf,
    pub history_database: PathBuf,
    pub vector_database: PathBuf,
    pub graph_database: PathBuf,
    pub bm25_index: PathBuf,
    pub evaluation_results: PathBuf,
    pub resources: PathBuf,
    pub papers: PathBuf,
}

pub trait Configuration: Send + Sync {
    fn paths(&self) -> &StoragePaths;
}
