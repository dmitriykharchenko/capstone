//! Native papers listing and PDF serving.
//!
//! Ported from the retired Python `papers.py`: a paper's id is its filename
//! stem, which must be a safe token so it can never escape the papers
//! directory when resolved. Titles are derived heuristically from filenames;
//! no PDF parsing is performed.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::CommandError;

/// Metadata describing one available paper.
#[derive(Debug, Clone, Serialize)]
pub struct PaperInfo {
    pub id: String,
    pub filename: String,
    pub title: String,
    pub size_bytes: u64,
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
                title: derive_title(&stem),
                id: stem,
                filename,
                size_bytes,
            });
        }
        papers.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(papers)
    }

    /// Return metadata for `paper_id` or a `PaperNotFound` error.
    pub fn get_paper(&self, paper_id: &str) -> Result<PaperInfo, CommandError> {
        let path = self.resolve_pdf_path(paper_id)?;
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        let size_bytes = std::fs::metadata(&path)?.len();
        Ok(PaperInfo {
            id: paper_id.to_string(),
            filename,
            title: derive_title(paper_id),
            size_bytes,
        })
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

/// Produce a human-readable title from a filename stem.
fn derive_title(paper_id: &str) -> String {
    let lowered = paper_id.to_ascii_lowercase();
    let stripped = strip_dblp_prefix(paper_id, &lowered);
    let cleaned = stripped.replace(['_', '-'], " ");
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        paper_id.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Strip a leading `DBLP[_-](conf|journals)[_-]` prefix, case-insensitively.
fn strip_dblp_prefix(original: &str, lowered: &str) -> String {
    for kind in ["conf", "journals"] {
        for sep_a in ["_", "-", ""] {
            for sep_b in ["_", "-"] {
                let prefix = format!("dblp{sep_a}{kind}{sep_b}");
                if lowered.starts_with(&prefix) {
                    return original[prefix.len()..].to_string();
                }
            }
        }
    }
    original.to_string()
}
