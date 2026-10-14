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
mod library;
mod orchestration;
mod papers;
#[cfg(all(test, unix))]
mod test_support;

use error::CommandError;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tauri::Manager;

const BUNDLED_BACKEND_RELATIVE_PATH: &str = "backend/macos-arm64/capstone-cli/capstone-cli";

#[derive(Clone, Debug)]
pub(crate) struct BackendInvocation {
    pub(crate) executable: PathBuf,
    pub(crate) working_directory: PathBuf,
    pub(crate) frozen: bool,
    pub(crate) data_directory: Option<PathBuf>,
    pub(crate) library_directory: Option<PathBuf>,
}

impl BackendInvocation {
    fn development(root: &Path) -> Self {
        let executable = std::env::var_os("CAPSTONE_PYTHON")
            .filter(|value| !value.to_string_lossy().trim().is_empty())
            .unwrap_or_else(|| "python3".into());
        Self {
            executable: PathBuf::from(executable),
            working_directory: root.to_path_buf(),
            frozen: false,
            data_directory: None,
            library_directory: None,
        }
    }

    fn bundled(resource_directory: &Path, data_directory: &Path) -> Result<Self, String> {
        let executable = resource_directory.join(BUNDLED_BACKEND_RELATIVE_PATH);
        if !executable.is_file() {
            return Err(format!(
                "bundled backend is missing: {}",
                executable.display()
            ));
        }
        let working_directory = executable
            .parent()
            .ok_or_else(|| "bundled backend has no parent directory".to_string())?
            .to_path_buf();
        Ok(Self {
            executable,
            working_directory,
            frozen: true,
            data_directory: Some(data_directory.to_path_buf()),
            library_directory: None,
        })
    }
}

static BUNDLED_BACKEND: OnceLock<BackendInvocation> = OnceLock::new();

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

pub(crate) fn backend_invocation(
    development_root: &Path,
    library_directory: Option<&Path>,
) -> BackendInvocation {
    let mut backend = BUNDLED_BACKEND
        .get()
        .cloned()
        .unwrap_or_else(|| BackendInvocation::development(development_root));
    backend.library_directory = library_directory.map(Path::to_path_buf);
    backend
}

pub(crate) fn bundled_data_directory() -> Option<PathBuf> {
    BUNDLED_BACKEND
        .get()
        .and_then(|backend| backend.data_directory.clone())
}

fn is_macos_app_resource_directory(resource_directory: &Path) -> bool {
    resource_directory
        .file_name()
        .and_then(|name| name.to_str())
        == Some("Resources")
        && resource_directory
            .parent()
            .and_then(|directory| directory.file_name())
            .and_then(|name| name.to_str())
            == Some("Contents")
}

fn configure_packaged_backend(app: &tauri::AppHandle) -> Result<(), String> {
    let resource_directory = app
        .path()
        .resource_dir()
        .map_err(|error| format!("could not resolve application resources: {error}"))?;
    if !is_macos_app_resource_directory(&resource_directory) {
        return Ok(());
    }
    let data_directory = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("could not resolve application data directory: {error}"))?;
    let backend = BackendInvocation::bundled(&resource_directory, &data_directory)?;
    BUNDLED_BACKEND
        .set(backend)
        .map_err(|_| "bundled backend was configured more than once".to_string())
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
fn check_python_backend(state: library::LibraryState) {
    std::thread::spawn(move || {
        let operation = match state.begin_operation() {
            Ok(operation) => operation,
            Err(error) => {
                eprintln!("python backend startup blocked: {error}");
                return;
            }
        };
        match bridge::call(
            &project_root(),
            operation.library_directory(),
            serde_json::json!({ "command": "health" }),
        ) {
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
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            configure_packaged_backend(&app.handle()).map_err(std::io::Error::other)?;
            let config_directory = app.path().app_config_dir().map_err(std::io::Error::other)?;
            let library_state = library::LibraryState::load(config_directory);
            app.manage(library_state.clone());
            check_python_backend(library_state);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            papers::list_papers,
            papers::read_pdf,
            library::get_current_library,
            library::choose_library,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn bundled_backend_uses_resource_and_application_support_paths() {
        let root =
            std::env::temp_dir().join(format!("capstone-bundled-backend-{}", std::process::id()));
        let resource_directory = root.join("Research assistant.app/Contents/Resources");
        let executable = resource_directory.join(BUNDLED_BACKEND_RELATIVE_PATH);
        fs::create_dir_all(executable.parent().unwrap()).unwrap();
        fs::write(&executable, b"fixture").unwrap();
        let data_directory =
            root.join("Library/Application Support/com.capstone.research-assistant");

        let backend = BackendInvocation::bundled(&resource_directory, &data_directory).unwrap();

        assert!(backend.frozen);
        assert_eq!(backend.executable, executable);
        assert_eq!(backend.working_directory, executable.parent().unwrap());
        assert_eq!(
            backend.data_directory.as_deref(),
            Some(data_directory.as_path())
        );
        assert!(backend.library_directory.is_none());
        assert!(is_macos_app_resource_directory(&resource_directory));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn development_backend_keeps_the_source_root() {
        let root = Path::new("/isolated/source-checkout");
        let backend = BackendInvocation::development(root);
        assert!(!backend.frozen);
        assert_eq!(backend.working_directory, root);
        assert!(backend.data_directory.is_none());
        assert!(backend.library_directory.is_none());
    }
}
