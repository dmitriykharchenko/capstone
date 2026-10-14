//! Tauri backend for the capstone research assistant.
//!
//! Papers listing and PDF serving are handled natively (`papers`). Chat,
//! indexing, graph, evaluation and orchestration commands are delegated to the
//! Python CLI (`python -m src.cli`) through `bridge`.

mod bridge;
mod chat;
mod error;
mod evaluation;
mod indexing;
mod orchestration;
mod papers;
#[cfg(all(test, unix))]
mod test_support;

use error::CommandError;

/// Resolve the repository root that contains the Python `src` package.
///
/// In development the Tauri process runs from `app/src-tauri`, so the root is
/// two levels up. An explicit `CAPSTONE_PROJECT_ROOT` override wins when set.
pub(crate) fn project_root() -> std::path::PathBuf {
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

/// Run a blocking command body on Tauri's blocking thread pool so a slow Python
/// subprocess or filesystem read never stalls the UI event loop. Every command
/// stays `async` and awaits the result, so its invoke promise (and any streaming
/// `Channel`) settles only once the work finishes.
pub(crate) async fn run_blocking<T, F>(work: F) -> Result<T, CommandError>
where
    F: FnOnce() -> Result<T, CommandError> + Send + 'static,
    T: Send + 'static,
{
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|error| CommandError::Bridge(format!("background task failed: {error}")))?
}

/// Start the Python backend once at launch and report whether it is usable.
fn check_python_backend() {
    std::thread::spawn(|| {
        match bridge::call(&project_root(), serde_json::json!({ "command": "health" })) {
            Ok(health) => eprintln!(
                "python backend ready: chat_mode={} search_enabled={}",
                health["chat_mode"], health["search_enabled"]
            ),
            Err(error) => eprintln!("python backend failed to start: {error}"),
        }
    });
}

/// Build and run the Tauri application, registering every command handler.
pub fn run() {
    tauri::Builder::default()
        .setup(|_| {
            check_python_backend();
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            papers::list_papers,
            papers::read_pdf,
            chat::chat_stream,
            orchestration::orchestration_start,
            orchestration::orchestration_resume,
            orchestration::orchestration_pending,
            orchestration::orchestration_discard,
            orchestration::orchestration_trace,
            chat::list_sessions,
            chat::get_messages,
            chat::search,
            papers::search_papers,
            indexing::index_papers_stream,
            indexing::paper_statuses,
            indexing::bm25_status,
            indexing::reset_all_indices,
            evaluation::evaluate_stream,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
