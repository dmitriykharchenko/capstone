use super::ConfigError;
use std::{
    fs,
    path::{Component, Path, PathBuf},
};

/// Resolve existing ancestors without creating directories; reject traversal.
pub fn validate_root(path: &Path) -> Result<PathBuf, ConfigError> {
    if !path.is_absolute()
        || path.parent().is_none()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(ConfigError::InvalidPath);
    }
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(ConfigError::InvalidPath);
        }
        return path.canonicalize().map_err(|_| ConfigError::InvalidPath);
    }
    let parent = path.parent().ok_or(ConfigError::InvalidPath)?;
    let resolved = if parent.parent().is_none() {
        parent.to_path_buf()
    } else {
        validate_root(parent)?
    };
    Ok(resolved.join(path.file_name().ok_or(ConfigError::InvalidPath)?))
}

/// Writable databases are direct children of an independently owned app root.
/// This deliberately excludes symlinks and hardlinks (including dangling ones).
pub fn validate_database_path(root: &Path, path: &Path) -> Result<PathBuf, ConfigError> {
    let root = validate_root(root)?;
    let parent = validate_root(path.parent().ok_or(ConfigError::InvalidPath)?)?;
    if parent != root || path.file_name().is_none() {
        return Err(ConfigError::InvalidPath);
    }
    let target = parent.join(path.file_name().ok_or(ConfigError::InvalidPath)?);
    match fs::symlink_metadata(&target) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_file() {
                return Err(ConfigError::InvalidPath);
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if meta.nlink() != 1 {
                    return Err(ConfigError::InvalidPath);
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(_) => return Err(ConfigError::InvalidPath),
    }
    Ok(target)
}
