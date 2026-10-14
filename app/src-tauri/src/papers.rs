//! Native papers listing and PDF serving.
//!
//! Ported from the retired Python `papers.py`: a paper's id is its filename
//! stem, which must be a safe token so it can never escape the papers
//! directory when resolved. Indexed title, author, and topic metadata is loaded
//! read-only through the Python bridge; no PDF parsing is performed.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::State;

use crate::error::CommandError;
use crate::library::LibraryState;
use crate::{bridge, project_root, run_blocking};

fn repository(library_directory: Option<&Path>) -> PaperRepository {
    PaperRepository::new(papers_directory(&project_root(), library_directory))
}

#[tauri::command]
pub(crate) async fn list_papers(
    state: State<'_, LibraryState>,
) -> Result<Vec<PaperInfo>, CommandError> {
    let operation = state.begin_operation()?;
    let root = project_root();
    run_blocking(move || {
        let library_directory = operation.library_directory();
        let mut papers = repository(library_directory).list_papers()?;
        let paper_ids = papers.iter().map(|paper| &paper.id).collect::<Vec<_>>();
        let request = serde_json::json!({
            "command": "paper_metadata",
            "paper_ids": paper_ids,
        });
        if let Ok(response) = bridge::call(&root, library_directory, request) {
            if let Ok(metadata) = serde_json::from_value::<PaperMetadataResponse>(response) {
                merge_paper_metadata(&mut papers, metadata);
            }
        }
        Ok(papers)
    })
    .await
}

#[tauri::command]
pub(crate) async fn read_pdf(
    paper_id: String,
    state: State<'_, LibraryState>,
) -> Result<PdfData, CommandError> {
    let operation = state.begin_operation()?;
    run_blocking(move || repository(operation.library_directory()).read_pdf(&paper_id)).await
}

#[tauri::command]
pub(crate) async fn search_papers(
    query: String,
    limit: Option<u32>,
    state: State<'_, LibraryState>,
) -> Result<Value, CommandError> {
    let operation = state.begin_operation()?;
    let root = project_root();
    run_blocking(move || {
        let mut request = serde_json::json!({ "command": "search_papers", "query": query });
        if let Some(limit) = limit {
            request["limit"] = serde_json::json!(limit);
        }
        bridge::call(&root, operation.library_directory(), request)
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

/// Resolve the papers directory, honoring packaged data-root overrides.
pub fn papers_directory(project_root: &Path, library_directory: Option<&Path>) -> PathBuf {
    let environment_library = std::env::var_os("CAPSTONE_LIBRARY_DIR");
    let papers_override = std::env::var_os("CAPSTONE_PAPERS_DIR");
    let data_override = std::env::var_os("CAPSTONE_DATA_DIR");
    let packaged_data_directory = crate::bundled_data_directory();
    papers_directory_with(
        project_root,
        library_directory,
        environment_library.as_deref(),
        papers_override.as_deref(),
        data_override.as_deref(),
        packaged_data_directory.as_deref(),
    )
}

fn papers_directory_with(
    project_root: &Path,
    library_directory: Option<&Path>,
    environment_library: Option<&OsStr>,
    papers_override: Option<&OsStr>,
    data_override: Option<&OsStr>,
    packaged_data_directory: Option<&Path>,
) -> PathBuf {
    if let Some(directory) = library_directory {
        return directory.to_path_buf();
    }
    if let Some(directory) = nonblank_path(environment_library) {
        return directory;
    }
    if let Some(directory) = nonblank_path(papers_override) {
        return directory;
    }
    if let Some(directory) = packaged_data_directory {
        return nonblank_path(data_override)
            .unwrap_or_else(|| directory.to_path_buf())
            .join("ResearchPapers");
    }
    project_root.join("ResearchPapers")
}

fn nonblank_path(value: Option<&OsStr>) -> Option<PathBuf> {
    value
        .filter(|value| !value.to_string_lossy().trim().is_empty())
        .map(PathBuf::from)
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
        let Some(paper) = papers
            .iter_mut()
            .find(|paper| paper.id == metadata.paper_id)
        else {
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
    use std::ffi::OsStr;

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

    #[test]
    fn packaged_papers_follow_explicit_data_root_after_papers_override() {
        let root = Path::new("/isolated/source");
        let app_data = Path::new("/isolated/Application Support/Capstone");

        assert_eq!(
            papers_directory_with(
                root,
                None,
                None,
                Some(OsStr::new("/external/papers")),
                Some(OsStr::new("/external/data")),
                Some(app_data),
            ),
            PathBuf::from("/external/papers")
        );
        assert_eq!(
            papers_directory_with(
                root,
                None,
                None,
                None,
                Some(OsStr::new("/external/data")),
                Some(app_data),
            ),
            PathBuf::from("/external/data/ResearchPapers")
        );
    }

    #[test]
    fn selected_library_is_authoritative_over_legacy_and_packaged_paths() {
        let root = Path::new("/isolated/source");
        let selected = Path::new("/selected/Papers — 研究");
        assert_eq!(
            papers_directory_with(
                root,
                Some(selected),
                Some(OsStr::new("/environment/library")),
                Some(OsStr::new("/legacy/papers")),
                Some(OsStr::new("/legacy/data")),
                Some(Path::new("/packaged/data")),
            ),
            selected
        );
    }

    #[test]
    fn packaged_blank_or_unset_data_root_falls_back_to_application_support() {
        let root = Path::new("/isolated/source");
        let app_data = Path::new("/isolated/Application Support/Capstone");

        for data_override in [None, Some(OsStr::new("   "))] {
            assert_eq!(
                papers_directory_with(
                    root,
                    None,
                    None,
                    Some(OsStr::new("  ")),
                    data_override,
                    Some(app_data),
                ),
                PathBuf::from("/isolated/Application Support/Capstone/ResearchPapers")
            );
        }
        assert_eq!(
            papers_directory_with(
                root,
                None,
                None,
                None,
                Some(OsStr::new("/ignored/data")),
                None,
            ),
            PathBuf::from("/isolated/source/ResearchPapers")
        );
    }

    #[test]
    fn environment_library_precedes_legacy_paths_without_persisted_selection() {
        assert_eq!(
            papers_directory_with(
                Path::new("/source"),
                None,
                Some(OsStr::new("/environment/Papers — 研究")),
                Some(OsStr::new("/legacy/papers")),
                Some(OsStr::new("/legacy/data")),
                Some(Path::new("/packaged/data")),
            ),
            PathBuf::from("/environment/Papers — 研究")
        );
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
