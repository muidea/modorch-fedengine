//! Process-local ownership, in addition to DuckDB's native cross-process lock.
use crate::error::fw;
use duckdb::Connection;
use modorch::FrameworkResult;
use modorch_fedengine_core::EngineError;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

type Identity = (u64, u64);
type Claims = BTreeMap<PathBuf, Option<Identity>>;
fn claims() -> &'static Mutex<Claims> {
    static CLAIMS: OnceLock<Mutex<Claims>> = OnceLock::new();
    CLAIMS.get_or_init(Mutex::default)
}
pub(super) struct FileClaim(PathBuf);
impl Drop for FileClaim {
    fn drop(&mut self) {
        claims()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
    }
}
fn identity(path: &Path) -> FrameworkResult<Option<Identity>> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(fw(EngineError::config(
                "cannot inspect management database file",
            )));
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(Some((metadata.dev(), metadata.ino())))
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        Ok(None)
    }
}

pub(super) fn open(
    path: &str,
    initialize: impl FnOnce() -> FrameworkResult<Connection>,
) -> FrameworkResult<(Connection, Option<FileClaim>)> {
    if path == ":memory:" {
        return initialize().map(|connection| (connection, None));
    }
    let path = std::path::absolute(path).map_err(|_| {
        fw(EngineError::config(
            "cannot resolve management database path",
        ))
    })?;
    let parent = path
        .parent()
        .ok_or_else(|| fw(EngineError::config("invalid management database path")))?;
    // Instance setup owns directory creation. Resolve it before admission so
    // missing parents, '..' and symlinked directories cannot bypass ownership.
    std::fs::create_dir_all(parent)
        .map_err(|_| fw(EngineError::config("cannot create engine directory")))?;
    let canonical = match path.canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => parent
            .canonicalize()
            .map_err(|_| {
                fw(EngineError::config(
                    "cannot resolve management database directory",
                ))
            })?
            .join(
                path.file_name()
                    .ok_or_else(|| fw(EngineError::config("invalid management database path")))?,
            ),
        Err(_) => {
            return Err(fw(EngineError::config(
                "cannot resolve management database file",
            )));
        }
    };
    // Serialize only local opening/registration, never remote attachment or
    // storage recovery. A new file gets its inode before another opener checks.
    let mut current = claims().lock().unwrap_or_else(|e| e.into_inner());
    let existing = identity(&canonical)?;
    if current.contains_key(&canonical)
        || existing.is_some_and(|id| current.values().any(|value| *value == Some(id)))
    {
        return Err(fw(EngineError::config(
            "management database file is already owned by another FedEngine instance",
        )));
    }
    let connection = initialize()?;
    // Opening may create the target of a previously dangling file symlink.
    let canonical = canonical.canonicalize().map_err(|_| {
        fw(EngineError::config(
            "cannot resolve opened management database file",
        ))
    })?;
    current.insert(canonical.clone(), identity(&canonical)?);
    Ok((connection, Some(FileClaim(canonical))))
}
