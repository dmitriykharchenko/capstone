//! Persisted research-library selection and operation ownership.

use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, State};
use tauri_plugin_dialog::DialogExt;

use crate::error::CommandError;

const CONFIG_FILE: &str = "library-selection.json";
const DATA_DIRECTORY: &str = ".research-assistant";

#[derive(Clone)]
pub(crate) struct LibraryState {
    inner: Arc<Mutex<LibraryStateInner>>,
    config_file: PathBuf,
}

struct LibraryStateInner {
    library: Option<PathBuf>,
    restore_error: Option<String>,
    active_operations: usize,
}

#[derive(Debug, Deserialize, Serialize)]
struct PersistedLibrary {
    library_dir: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LibrarySelection {
    pub(crate) path: String,
    pub(crate) data_path: String,
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChooseLibraryResponse {
    pub(crate) library: Option<LibrarySelection>,
    pub(crate) changed: bool,
    pub(crate) cancelled: bool,
}

pub(crate) struct LibraryOperation {
    state: LibraryState,
    library: Option<PathBuf>,
}

impl LibraryState {
    pub(crate) fn load(config_directory: PathBuf) -> Self {
        let config_file = config_directory.join(CONFIG_FILE);
        let restored = restore_library(&config_file);
        let (library, restore_error) = match restored {
            Ok(library) => (library, None),
            Err(error) => (None, Some(error)),
        };
        Self {
            inner: Arc::new(Mutex::new(LibraryStateInner {
                library,
                restore_error,
                active_operations: 0,
            })),
            config_file,
        }
    }

    pub(crate) fn begin_operation(&self) -> Result<LibraryOperation, CommandError> {
        let mut inner = self.lock()?;
        if let Some(error) = &inner.restore_error {
            return Err(CommandError::Library(error.clone()));
        }
        inner.active_operations = inner
            .active_operations
            .checked_add(1)
            .ok_or_else(|| CommandError::Library("too many active operations".to_string()))?;
        Ok(LibraryOperation {
            state: self.clone(),
            library: inner.library.clone(),
        })
    }

    fn current(&self) -> Result<Option<LibrarySelection>, CommandError> {
        let inner = self.lock()?;
        if let Some(error) = &inner.restore_error {
            return Err(CommandError::Library(error.clone()));
        }
        inner.library.as_deref().map(selection).transpose()
    }

    fn choose(&self, selected: Option<PathBuf>) -> Result<ChooseLibraryResponse, CommandError> {
        let Some(selected) = selected else {
            return Ok(ChooseLibraryResponse {
                library: self.current()?,
                changed: false,
                cancelled: true,
            });
        };
        let mut inner = self.lock()?;
        if inner.active_operations != 0 {
            let reselects_current = inner.restore_error.is_none()
                && selected
                    .canonicalize()
                    .ok()
                    .as_deref()
                    .zip(inner.library.as_deref())
                    .is_some_and(|(selected, current)| selected == current);
            if reselects_current {
                return Ok(ChooseLibraryResponse {
                    library: inner.library.as_deref().map(selection).transpose()?,
                    changed: false,
                    cancelled: false,
                });
            }
            return Err(CommandError::Library(
                "cannot change libraries while a native or backend operation is active".to_string(),
            ));
        }
        let selected = validate_library(&selected, true).map_err(CommandError::Library)?;
        let changed =
            inner.library.as_deref() != Some(selected.as_path()) || inner.restore_error.is_some();
        if changed {
            persist_library(&self.config_file, &selected).map_err(CommandError::Library)?;
            inner.library = Some(selected.clone());
            inner.restore_error = None;
        }
        Ok(ChooseLibraryResponse {
            library: Some(selection(&selected)?),
            changed,
            cancelled: false,
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, LibraryStateInner>, CommandError> {
        self.inner
            .lock()
            .map_err(|_| CommandError::Library("library state is unavailable".to_string()))
    }
}

impl LibraryOperation {
    pub(crate) fn library_directory(&self) -> Option<&Path> {
        self.library.as_deref()
    }
}

impl Drop for LibraryOperation {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.state.inner.lock() {
            inner.active_operations = inner.active_operations.saturating_sub(1);
        }
    }
}

#[tauri::command]
pub(crate) async fn get_current_library(
    state: State<'_, LibraryState>,
) -> Result<Option<LibrarySelection>, CommandError> {
    state.current()
}

#[tauri::command]
pub(crate) async fn choose_library(
    app: AppHandle,
    state: State<'_, LibraryState>,
) -> Result<ChooseLibraryResponse, CommandError> {
    let selected = app
        .dialog()
        .file()
        .set_title("Choose research library")
        .blocking_pick_folder()
        .map(|path| {
            path.into_path().map_err(|error| {
                CommandError::Library(format!("selected folder is not a local path: {error}"))
            })
        })
        .transpose()?;
    state.choose(selected)
}

fn restore_library(config_file: &Path) -> Result<Option<PathBuf>, String> {
    let bytes = match fs::read(config_file) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "could not read persisted library selection at {}: {error}",
                config_file.display()
            ))
        }
    };
    let persisted: PersistedLibrary = serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "persisted library selection at {} is invalid: {error}",
            config_file.display()
        )
    })?;
    validate_library(&persisted.library_dir, true)
        .map(Some)
        .map_err(|error| format!("persisted library is unavailable: {error}"))
}

fn validate_library(path: &Path, create_data_directory: bool) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("selected library must be an absolute path".to_string());
    }
    let root = path.canonicalize().map_err(|error| {
        format!(
            "could not open selected library {}: {error}",
            path.display()
        )
    })?;
    if !root.is_dir() {
        return Err(format!(
            "selected library is not a directory: {}",
            root.display()
        ));
    }
    fs::read_dir(&root).map_err(|error| {
        format!(
            "selected library is not readable {}: {error}",
            root.display()
        )
    })?;

    let data_directory = root.join(DATA_DIRECTORY);
    match fs::symlink_metadata(&data_directory) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && create_data_directory => {
            fs::create_dir(&data_directory).map_err(|error| {
                format!(
                    "could not create library data directory {}: {error}",
                    data_directory.display()
                )
            })?;
        }
        Err(error) => {
            return Err(format!(
                "could not inspect library data directory {}: {error}",
                data_directory.display()
            ))
        }
    }
    let resolved_data = data_directory.canonicalize().map_err(|error| {
        format!(
            "could not open library data directory {}: {error}",
            data_directory.display()
        )
    })?;
    if !resolved_data.starts_with(&root) {
        return Err(format!(
            "library data directory escapes selected library: {}",
            data_directory.display()
        ));
    }
    if !resolved_data.is_dir() {
        return Err(format!(
            "library data path is not a directory: {}",
            data_directory.display()
        ));
    }
    verify_writable(&resolved_data)?;
    Ok(root)
}

fn verify_writable(directory: &Path) -> Result<(), String> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let probe = directory.join(format!(
        ".capstone-write-check-{}-{nonce}",
        std::process::id()
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)?;
        file.write_all(b"library write check")?;
        file.sync_all()
    })();
    let _ = fs::remove_file(&probe);
    result.map_err(|error| {
        format!(
            "library data directory is not writable {}: {error}",
            directory.display()
        )
    })
}

fn persist_library(config_file: &Path, library: &Path) -> Result<(), String> {
    let parent = config_file
        .parent()
        .ok_or_else(|| "library configuration path has no parent directory".to_string())?;
    fs::create_dir_all(parent).map_err(|error| {
        format!(
            "could not create application configuration directory {}: {error}",
            parent.display()
        )
    })?;
    let bytes = serde_json::to_vec_pretty(&PersistedLibrary {
        library_dir: library.to_path_buf(),
    })
    .map_err(|error| format!("could not encode library selection: {error}"))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = parent.join(format!(".{CONFIG_FILE}.{}-{nonce}.tmp", std::process::id()));
    let write_result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, config_file)
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary);
        return Err(format!(
            "could not persist library selection at {}: {error}",
            config_file.display()
        ));
    }
    Ok(())
}

fn selection(path: &Path) -> Result<LibrarySelection, CommandError> {
    let path_string = path.to_str().ok_or_else(|| {
        CommandError::Library("selected library path is not valid Unicode".to_string())
    })?;
    let data_path = path.join(DATA_DIRECTORY);
    let data_string = data_path.to_str().ok_or_else(|| {
        CommandError::Library("library data path is not valid Unicode".to_string())
    })?;
    Ok(LibrarySelection {
        path: path_string.to_string(),
        data_path: data_string.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let suffix = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "capstone-library-selection-{}-{suffix}",
                std::process::id()
            ));
            fs::create_dir(&root).unwrap();
            Self { root }
        }

        fn library(&self, name: &str) -> PathBuf {
            let path = self.root.join(name);
            fs::create_dir(&path).unwrap();
            path
        }

        fn state(&self) -> LibraryState {
            LibraryState::load(self.root.join("app config"))
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn persists_and_restores_canonical_unicode_library() {
        let fixture = Fixture::new();
        let library = fixture.library("Papers — café 研究");
        let state = fixture.state();

        let chosen = state.choose(Some(library.clone())).unwrap();
        let canonical = library.canonicalize().unwrap();

        assert!(chosen.changed);
        assert!(!chosen.cancelled);
        assert_eq!(
            chosen.library.unwrap().path,
            canonical.to_str().unwrap().to_string()
        );
        assert!(canonical.join(DATA_DIRECTORY).is_dir());

        let restarted = fixture.state();
        let restored = restarted.current().unwrap().unwrap();
        assert_eq!(restored.path, canonical.to_str().unwrap());
        let persisted: PersistedLibrary = serde_json::from_slice(
            &fs::read(fixture.root.join("app config").join(CONFIG_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(persisted.library_dir, canonical);
    }

    #[test]
    fn cancellation_failure_and_same_folder_leave_selection_unchanged() {
        let fixture = Fixture::new();
        let library = fixture.library("first");
        let state = fixture.state();
        let selected = state.choose(Some(library.clone())).unwrap().library;

        let cancelled = state.choose(None).unwrap();
        assert!(cancelled.cancelled);
        assert!(!cancelled.changed);
        assert_eq!(cancelled.library, selected);

        let missing = fixture.root.join("missing");
        assert!(state.choose(Some(missing)).is_err());
        assert_eq!(state.current().unwrap(), selected);

        let reused = state.choose(Some(library)).unwrap();
        assert!(!reused.changed);
        assert!(!reused.cancelled);
        assert_eq!(reused.library, selected);
    }

    #[test]
    fn active_operation_rejects_switch_then_new_operations_use_successful_switch() {
        let fixture = Fixture::new();
        let first = fixture.library("first");
        let second = fixture.library("second");
        let canonical_first = first.canonicalize().unwrap();
        let canonical_second = second.canonicalize().unwrap();
        let state = fixture.state();
        state.choose(Some(first.clone())).unwrap();
        let operation = state.begin_operation().unwrap();

        let reused = state.choose(Some(first.clone())).unwrap();
        assert!(!reused.changed);

        let error = state.choose(Some(second.clone())).unwrap_err();
        assert!(error.to_string().contains("operation is active"));
        assert_eq!(
            operation.library_directory(),
            Some(canonical_first.as_path())
        );
        assert_eq!(
            state.current().unwrap().unwrap().path,
            canonical_first.to_str().unwrap()
        );

        drop(operation);
        assert!(state.choose(Some(second.clone())).unwrap().changed);
        let next = state.begin_operation().unwrap();
        assert_eq!(next.library_directory(), Some(canonical_second.as_path()));
    }

    #[test]
    fn invalid_persisted_pointer_blocks_fallback_until_replaced() {
        let fixture = Fixture::new();
        let config_directory = fixture.root.join("app config");
        fs::create_dir(&config_directory).unwrap();
        let missing = fixture.root.join("missing library");
        fs::write(
            config_directory.join(CONFIG_FILE),
            serde_json::to_vec(&PersistedLibrary {
                library_dir: missing,
            })
            .unwrap(),
        )
        .unwrap();
        let state = LibraryState::load(config_directory);

        assert!(state
            .current()
            .unwrap_err()
            .to_string()
            .contains("persisted"));
        assert!(state
            .begin_operation()
            .err()
            .unwrap()
            .to_string()
            .contains("persisted"));

        let replacement = fixture.library("replacement");
        let canonical_replacement = replacement.canonicalize().unwrap();
        assert!(state.choose(Some(replacement.clone())).unwrap().changed);
        assert_eq!(
            state.begin_operation().unwrap().library_directory(),
            Some(canonical_replacement.as_path())
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_data_directory_symlink_escape() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let library = fixture.library("library");
        let outside = fixture.library("outside");
        symlink(&outside, library.join(DATA_DIRECTORY)).unwrap();

        let error = fixture.state().choose(Some(library)).unwrap_err();

        assert!(error.to_string().contains("escapes selected library"));
        assert!(outside.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn unwritable_persisted_data_directory_blocks_operations() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = Fixture::new();
        let library = fixture.library("library");
        let state = fixture.state();
        state.choose(Some(library.clone())).unwrap();
        let data_directory = library.join(DATA_DIRECTORY);
        fs::set_permissions(&data_directory, fs::Permissions::from_mode(0o500)).unwrap();

        let restarted = fixture.state();
        let message = restarted.current().unwrap_err().to_string();

        fs::set_permissions(&data_directory, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(message.contains("persisted library is unavailable"));
        assert!(message.contains("not writable"));
    }
}
