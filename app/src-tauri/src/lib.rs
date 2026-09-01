//! Tauri backend for the capstone research assistant.
//!
//! Papers listing and PDF serving are handled natively (filesystem with a
//! safe-id path-traversal guard). Chat, sessions, messages, search, and health
//! are delegated to the Python CLI bridge (`python -m src.cli`) by piping one
//! JSON request on stdin and parsing one JSON response on stdout.

mod bridge;
mod error;
mod papers;

use error::CommandError;
use papers::{PaperInfo, PaperRepository, PdfData};
use serde_json::Value;

/// Resolve the repository root that contains the Python `src` package.
///
/// In development the Tauri process runs from `app/src-tauri`, so the root is
/// two levels up. An explicit `CAPSTONE_PROJECT_ROOT` override wins when set.
fn project_root() -> std::path::PathBuf {
    if let Ok(explicit) = std::env::var("CAPSTONE_PROJECT_ROOT") {
        if !explicit.trim().is_empty() {
            return std::path::PathBuf::from(explicit);
        }
    }
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .unwrap_or(manifest)
}

fn repository() -> PaperRepository {
    PaperRepository::new(papers::papers_directory(&project_root()))
}

#[tauri::command]
fn list_papers() -> Result<Vec<PaperInfo>, CommandError> {
    repository().list_papers()
}

#[tauri::command]
fn get_paper(paper_id: String) -> Result<PaperInfo, CommandError> {
    repository().get_paper(&paper_id)
}

#[tauri::command]
fn read_pdf(paper_id: String) -> Result<PdfData, CommandError> {
    repository().read_pdf(&paper_id)
}

#[tauri::command]
fn health() -> Result<Value, CommandError> {
    bridge::call(&project_root(), serde_json::json!({ "command": "health" }))
}

#[tauri::command]
fn chat(session_id: String, message: String) -> Result<Value, CommandError> {
    bridge::call(
        &project_root(),
        serde_json::json!({
            "command": "chat",
            "session_id": session_id,
            "message": message,
        }),
    )
}

#[tauri::command]
fn list_sessions() -> Result<Value, CommandError> {
    bridge::call(&project_root(), serde_json::json!({ "command": "sessions" }))
}

#[tauri::command]
fn get_messages(session_id: String) -> Result<Value, CommandError> {
    bridge::call(
        &project_root(),
        serde_json::json!({ "command": "messages", "session_id": session_id }),
    )
}

#[tauri::command]
fn search(query: String, limit: Option<u32>, session_id: Option<String>) -> Result<Value, CommandError> {
    let mut request = serde_json::json!({ "command": "search", "query": query });
    if let Some(limit) = limit {
        request["limit"] = serde_json::json!(limit);
    }
    if let Some(session_id) = session_id {
        request["session_id"] = serde_json::json!(session_id);
    }
    bridge::call(&project_root(), request)
}

/// Build and run the Tauri application, registering every command handler.
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            list_papers,
            get_paper,
            read_pdf,
            health,
            chat,
            list_sessions,
            get_messages,
            search,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
