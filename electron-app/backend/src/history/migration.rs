use super::{HistoryError, HistoryResult, SqliteHistory, schema::*, store::check_cancel};
use crate::runtime::Cancellation;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlite::{Connection, OpenFlags, Value};
use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ImportReport {
    pub imported: bool,
    pub schema_version: u32,
    pub messages: u64,
    pub exchanges: u64,
    pub sources: u64,
    pub paper_statuses: u64,
}

impl SqliteHistory {
    /// Explicit opt-in only. Source remains read-only, destination must be absent
    /// or already carry this exact import fingerprint. Never merges or replaces.
    pub fn import_legacy(
        &self,
        source: &Path,
        cancellation: &Cancellation,
    ) -> HistoryResult<ImportReport> {
        let _guard = self.gate.lock().map_err(|_| HistoryError::Storage)?;
        check_cancel(cancellation)?;
        self.check_path()?;
        let source = checked_source(source)?;
        if source == self.path || source.starts_with(&self.root) {
            return Err(HistoryError::InvalidPath);
        }
        self.prepare_root()?;
        let staging = Staging::new(&self.root)?;
        {
            let source_db =
                Connection::open_with_flags(&source, OpenFlags::new().with_read_only())?;
            source_db.execute("PRAGMA trusted_schema=OFF")?;
            check_schema(&source_db, true)?;
            execute(
                &source_db,
                "VACUUM INTO ?",
                &[staging
                    .path
                    .to_str()
                    .ok_or(HistoryError::InvalidPath)?
                    .into()],
            )?;
        }
        check_cancel(cancellation)?;
        let db = sqlite::open(&staging.path)?;
        db.execute("PRAGMA trusted_schema=OFF; PRAGMA journal_mode=DELETE; PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL")?;
        migrate(&db)?;
        let fingerprint = fingerprint(&db)?;
        let mut report = ImportReport {
            imported: true,
            schema_version: VERSION as u32,
            messages: scalar(&db, "SELECT COUNT(*) FROM messages")? as u64,
            exchanges: scalar(&db, "SELECT COUNT(*) FROM exchanges")? as u64,
            sources: scalar(&db, "SELECT COUNT(*) FROM exchange_sources")? as u64,
            paper_statuses: scalar(&db, "SELECT COUNT(*) FROM paper_index_status")? as u64,
        };
        self.check_path()?;
        if self.path.exists() {
            let existing = self.connection()?;
            let rows = query(
                &existing,
                "SELECT fingerprint FROM app_history_import WHERE id=1",
                &[],
            )?;
            if rows != vec![vec![Value::String(fingerprint)]] {
                return Err(HistoryError::AlreadyExists);
            }
            report.imported = false;
            return Ok(report);
        }
        execute(
            &db,
            "INSERT OR REPLACE INTO app_history_import(id,fingerprint) VALUES(1,?)",
            &[fingerprint.into()],
        )?;
        drop(db);
        self.check_path()?;
        staging.publish(&self.path, cancellation)?;
        Ok(report)
    }
}

fn checked_source(path: &Path) -> HistoryResult<PathBuf> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(HistoryError::InvalidPath);
    }
    let meta = fs::symlink_metadata(path).map_err(|_| HistoryError::InvalidPath)?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(HistoryError::InvalidPath);
    }
    for suffix in ["-wal", "-shm", "-journal"] {
        let sidecar = PathBuf::from(format!(
            "{}{suffix}",
            path.to_str().ok_or(HistoryError::InvalidPath)?
        ));
        if let Ok(meta) = fs::symlink_metadata(sidecar)
            && (!meta.is_file() || meta.file_type().is_symlink())
        {
            return Err(HistoryError::InvalidPath);
        }
    }
    path.canonicalize().map_err(|_| HistoryError::InvalidPath)
}

// Stable logical digest: unaffected by WAL checkpoints, file headers, or VACUUM.
// Include sequence high-water marks so future exchange/message IDs never rewind.
fn fingerprint(db: &Connection) -> HistoryResult<String> {
    let mut hash = Sha256::new();
    for (table, order) in [
        ("messages", "id"),
        ("exchanges", "id"),
        ("exchange_sources", "exchange_id,rank"),
        ("paper_index_status", "paper_id"),
        ("sqlite_sequence", "name"),
    ] {
        hash.update(table.as_bytes());
        for row in query(db, &format!("SELECT * FROM {table} ORDER BY {order}"), &[])? {
            hash.update([0xff]);
            for value in row {
                let bytes = match value {
                    Value::Null => vec![0],
                    Value::Integer(n) => {
                        let mut bytes = vec![1];
                        bytes.extend(n.to_le_bytes());
                        bytes
                    }
                    Value::Float(n) => {
                        let mut bytes = vec![2];
                        bytes.extend(n.to_bits().to_le_bytes());
                        bytes
                    }
                    Value::String(s) => {
                        let mut bytes = vec![3];
                        bytes.extend(s.as_bytes());
                        bytes
                    }
                    Value::Binary(_) => return Err(HistoryError::InvalidSchema),
                };
                hash.update((bytes.len() as u64).to_le_bytes());
                hash.update(bytes);
            }
        }
    }
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

pub(super) struct Staging {
    pub path: PathBuf,
    directory: PathBuf,
}
impl Staging {
    pub fn new(root: &Path) -> HistoryResult<Self> {
        let directory = root.join(format!(".history-stage-{}", uuid::Uuid::new_v4()));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&directory)?;
        let path = directory.join("snapshot.sqlite3");
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(&path)?;
        Ok(Self { path, directory })
    }
    pub fn publish(&self, target: &Path, cancellation: &Cancellation) -> HistoryResult<()> {
        File::open(&self.path)?.sync_all()?;
        publish_no_replace(&self.path, target, cancellation)
    }
}

// Atomic no-clobber rename avoids a crash window with two hard links. Pin both
// directories and never follow a last-component symlink during publication.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn publish_no_replace(
    source: &Path,
    target: &Path,
    cancellation: &Cancellation,
) -> HistoryResult<()> {
    use std::{
        ffi::CString,
        os::{
            fd::AsRawFd,
            unix::{ffi::OsStrExt, fs::OpenOptionsExt},
        },
    };
    let open_dir = |p: &Path| {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(p)
    };
    let from = open_dir(source.parent().ok_or(HistoryError::InvalidPath)?)?;
    let to = open_dir(target.parent().ok_or(HistoryError::InvalidPath)?)?;
    let source_name = CString::new(
        source
            .file_name()
            .ok_or(HistoryError::InvalidPath)?
            .as_bytes(),
    )
    .map_err(|_| HistoryError::InvalidPath)?;
    let target_name = CString::new(
        target
            .file_name()
            .ok_or(HistoryError::InvalidPath)?
            .as_bytes(),
    )
    .map_err(|_| HistoryError::InvalidPath)?;
    let _permit = super::store::seal(cancellation)?;
    // SAFETY: owned directory descriptors and NUL-terminated names stay alive.
    #[cfg(target_os = "macos")]
    let result = unsafe {
        libc::renameatx_np(
            from.as_raw_fd(),
            source_name.as_ptr(),
            to.as_raw_fd(),
            target_name.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::renameat2(
            from.as_raw_fd(),
            source_name.as_ptr(),
            to.as_raw_fd(),
            target_name.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result != 0 {
        return Err(
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::AlreadyExists {
                HistoryError::AlreadyExists
            } else {
                HistoryError::Storage
            },
        );
    }
    to.sync_all()?;
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn publish_no_replace(_: &Path, _: &Path, _: &Cancellation) -> HistoryResult<()> {
    // Do not silently fall back to overwrite-capable or crash-unsafe publication.
    Err(HistoryError::Storage)
}
impl Drop for Staging {
    fn drop(&mut self) {
        // Only our private random staging files; never source or destination data.
        for suffix in ["", "-journal", "-wal", "-shm"] {
            let _ = fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
        let _ = fs::remove_dir(&self.directory);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_publication_never_exposes_destination() {
        let root = tempfile::tempdir().unwrap();
        let stage = Staging::new(root.path()).unwrap();
        fs::write(&stage.path, b"complete snapshot").unwrap();
        let cancellation = Cancellation::default();
        assert!(cancellation.cancel());
        let target = root.path().join("history.sqlite3");
        assert_eq!(
            stage.publish(&target, &cancellation),
            Err(HistoryError::Cancelled)
        );
        assert!(!target.exists());
        drop(stage);
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn publication_is_sticky_no_clobber_and_preserves_unrelated_recovery_files() {
        let root = tempfile::tempdir().unwrap();
        let unrelated = root.path().join(".history-stage-unrelated");
        fs::create_dir(&unrelated).unwrap();
        fs::write(unrelated.join("keep"), b"not ours").unwrap();
        let target = root.path().join("history.sqlite3");
        let stage = Staging::new(root.path()).unwrap();
        fs::write(&stage.path, b"complete snapshot").unwrap();
        let cancellation = Cancellation::default();
        stage.publish(&target, &cancellation).unwrap();
        assert!(!cancellation.cancel());
        assert!(!stage.path.exists());
        let other = Staging::new(root.path()).unwrap();
        fs::write(&other.path, b"replacement").unwrap();
        let cancellation = Cancellation::default();
        assert_eq!(
            other.publish(&target, &cancellation),
            Err(HistoryError::AlreadyExists)
        );
        assert!(!cancellation.cancel());
        drop(stage);
        drop(other);
        assert_eq!(fs::read(target).unwrap(), b"complete snapshot");
        assert_eq!(fs::read(unrelated.join("keep")).unwrap(), b"not ours");
    }
}
