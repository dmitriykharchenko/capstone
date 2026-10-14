//! Native papers listing and PDF serving.
//!
//! Ported from the retired Python `papers.py`: a paper's id is its filename
//! stem, which must be a safe token so it can never escape the papers
//! directory when resolved. Indexed title, author, and topic metadata is loaded
//! read-only through the Python bridge; no PDF parsing is performed.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::CommandError;
use crate::{bridge, project_root, run_blocking};

fn repository() -> PaperRepository {
    PaperRepository::new(papers_directory(&project_root()))
}

#[tauri::command]
pub(crate) async fn list_papers() -> Result<Vec<PaperInfo>, CommandError> {
    let root = project_root();
    run_blocking(move || {
        let mut papers = repository().list_papers()?;
        let paper_ids = papers.iter().map(|paper| &paper.id).collect::<Vec<_>>();
        let request = serde_json::json!({
            "command": "paper_metadata",
            "paper_ids": paper_ids,
        });
        if let Ok(response) = bridge::call(&root, request) {
            if let Ok(metadata) = serde_json::from_value::<PaperMetadataResponse>(response) {
                merge_paper_metadata(&mut papers, metadata);
            }
        }
        Ok(papers)
    })
    .await
}

#[tauri::command]
pub(crate) async fn read_pdf(paper_id: String) -> Result<PdfData, CommandError> {
    run_blocking(move || repository().read_pdf(&paper_id)).await
}

#[tauri::command]
pub(crate) async fn search_papers(
    query: String,
    limit: Option<u32>,
) -> Result<Value, CommandError> {
    let root = project_root();
    run_blocking(move || {
        let mut request = serde_json::json!({ "command": "search_papers", "query": query });
        if let Some(limit) = limit {
            request["limit"] = serde_json::json!(limit);
        }
        bridge::call(&root, request)
    })
    .await
}

/// Metadata describing one available paper.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PaperInfo {
    pub id: String,
    pub filename: String,
    pub title: Option<String>,
    pub authors: Vec<String>,
    pub topics: Vec<String>,
    pub metadata_status: PaperMetadataStatus,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PaperMetadataStatus {
    Available,
    Missing,
    Unavailable,
}

#[derive(Debug, Deserialize)]
struct PaperMetadataResponse {
    available: bool,
    papers: Vec<IndexedPaperMetadata>,
}

#[derive(Debug, Deserialize)]
struct IndexedPaperMetadata {
    paper_id: String,
    title: String,
    authors: Vec<String>,
    topics: Vec<String>,
}

/// PDF bytes plus the metadata a webview needs to render them inline.
#[derive(Debug, Serialize)]
pub struct PdfData {
    pub id: String,
    pub filename: String,
    pub mime_type: String,
    pub bytes: Vec<u8>,
}

/// Resolve the papers directory, honoring `CAPSTONE_PAPERS_DIR`.
pub fn papers_directory(project_root: &Path) -> PathBuf {
    if let Ok(explicit) = std::env::var("CAPSTONE_PAPERS_DIR") {
        if !explicit.trim().is_empty() {
            return PathBuf::from(explicit);
        }
    }
    project_root.join("ResearchPapers")
}

/// List available papers and resolve their PDF paths within one directory.
pub struct PaperRepository {
    directory: PathBuf,
}

impl PaperRepository {
    pub fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    /// Return all papers sorted by id; an empty list when none exist.
    pub fn list_papers(&self) -> Result<Vec<PaperInfo>, CommandError> {
        if !self.directory.is_dir() {
            return Ok(Vec::new());
        }
        let mut papers = Vec::new();
        for entry in std::fs::read_dir(&self.directory)? {
            let path = entry?.path();
            if !path.is_file() {
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("pdf") {
                continue;
            }
            let stem = match path.file_stem().and_then(|s| s.to_str()) {
                Some(stem) if is_safe_id(stem) => stem.to_string(),
                _ => continue,
            };
            let filename = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();
            let size_bytes = std::fs::metadata(&path)?.len();
            papers.push(PaperInfo {
                id: stem,
                filename,
                title: None,
                authors: Vec::new(),
                topics: Vec::new(),
                metadata_status: PaperMetadataStatus::Unavailable,
                size_bytes,
            });
        }
        papers.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(papers)
    }

    /// Read the PDF bytes for `paper_id` for inline rendering in the webview.
    pub fn read_pdf(&self, paper_id: &str) -> Result<PdfData, CommandError> {
        let path = self.resolve_pdf_path(paper_id)?;
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        let bytes = std::fs::read(&path)?;
        Ok(PdfData {
            id: paper_id.to_string(),
            filename,
            mime_type: "application/pdf".to_string(),
            bytes,
        })
    }

    /// Resolve `paper_id` to an existing PDF strictly inside the directory.
    fn resolve_pdf_path(&self, paper_id: &str) -> Result<PathBuf, CommandError> {
        if !is_safe_id(paper_id) {
            return Err(CommandError::PaperNotFound(paper_id.to_string()));
        }
        let candidate = self.directory.join(format!("{paper_id}.pdf"));
        let base = self
            .directory
            .canonicalize()
            .map_err(|_| CommandError::PaperNotFound(paper_id.to_string()))?;
        let resolved = candidate
            .canonicalize()
            .map_err(|_| CommandError::PaperNotFound(paper_id.to_string()))?;
        if !resolved.starts_with(&base) || !resolved.is_file() {
            return Err(CommandError::PaperNotFound(paper_id.to_string()));
        }
        Ok(resolved)
    }
}

fn merge_paper_metadata(papers: &mut [PaperInfo], response: PaperMetadataResponse) {
    if !response.available {
        return;
    }
    for paper in papers.iter_mut() {
        paper.metadata_status = PaperMetadataStatus::Missing;
    }
    for metadata in response.papers {
        let Some(paper) = papers.iter_mut().find(|paper| paper.id == metadata.paper_id) else {
            continue;
        };
        paper.title = Some(metadata.title);
        paper.authors = metadata.authors;
        paper.topics = metadata.topics;
        paper.metadata_status = PaperMetadataStatus::Available;
    }
}

/// Match the Python `_SAFE_ID` regex `^[A-Za-z0-9][A-Za-z0-9._-]*$`.
fn is_safe_id(id: &str) -> bool {
    let mut chars = id.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    id.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_real_metadata_and_marks_unindexed_papers_missing() {
        let mut papers = vec![paper("paper-a"), paper("paper-b")];
        merge_paper_metadata(
            &mut papers,
            PaperMetadataResponse {
                available: true,
                papers: vec![IndexedPaperMetadata {
                    paper_id: "paper-a".to_string(),
                    title: "Stored title".to_string(),
                    authors: vec!["Ada".to_string()],
                    topics: vec!["Verification".to_string()],
                }],
            },
        );

        assert_eq!(papers[0].title.as_deref(), Some("Stored title"));
        assert_eq!(papers[0].authors, ["Ada"]);
        assert_eq!(papers[0].topics, ["Verification"]);
        assert_eq!(papers[0].metadata_status, PaperMetadataStatus::Available);
        assert_eq!(papers[1].title, None);
        assert_eq!(papers[1].metadata_status, PaperMetadataStatus::Missing);
    }

    fn paper(id: &str) -> PaperInfo {
        PaperInfo {
            id: id.to_string(),
            filename: format!("{id}.pdf"),
            title: None,
            authors: Vec::new(),
            topics: Vec::new(),
            metadata_status: PaperMetadataStatus::Unavailable,
            size_bytes: 1,
        }
    }
}
