use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::digest::hex_lower;

pub(crate) fn validate_maximum(maximum_bytes: u64) -> Result<(), AtomicFileError> {
    if maximum_bytes == 0 {
        Err(AtomicFileError::MaximumIsZero)
    } else {
        Ok(())
    }
}

pub(crate) fn read_bounded(path: &Path, maximum_bytes: u64) -> Result<Vec<u8>, AtomicFileError> {
    validate_maximum(maximum_bytes)?;
    let mut file = open_for_read(path)?;
    let size = file.metadata()?.len();
    if size > maximum_bytes {
        return Err(AtomicFileError::SizeLimit {
            attempted: size,
            maximum: maximum_bytes,
        });
    }
    let mut bytes =
        Vec::with_capacity(usize::try_from(size).map_err(|_| AtomicFileError::Counter)?);
    Read::by_ref(&mut file)
        .take(maximum_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    let actual = u64::try_from(bytes.len()).map_err(|_| AtomicFileError::Counter)?;
    if actual > maximum_bytes {
        return Err(AtomicFileError::SizeLimit {
            attempted: actual,
            maximum: maximum_bytes,
        });
    }
    Ok(bytes)
}

pub(crate) fn create(path: &Path, bytes: &[u8]) -> Result<(), AtomicFileError> {
    validate_existing_target(path, false)?;
    if path.try_exists()? {
        return Err(AtomicFileError::Io(std::io::Error::from(
            std::io::ErrorKind::AlreadyExists,
        )));
    }
    let temporary = temporary_path(path)?;
    let result = (|| {
        let mut file = create_private_file(&temporary)?;
        write_synced(&mut file, bytes)?;
        std::fs::hard_link(&temporary, path)?;
        std::fs::remove_file(&temporary)?;
        sync_parent(path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

pub(crate) fn replace(path: &Path, bytes: &[u8]) -> Result<(), AtomicFileError> {
    validate_existing_target(path, false)?;
    let temporary = temporary_path(path)?;
    let result = (|| {
        let mut file = create_private_file(&temporary)?;
        write_synced(&mut file, bytes)?;
        std::fs::rename(&temporary, path)?;
        sync_parent(path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn open_for_read(path: &Path) -> Result<File, AtomicFileError> {
    validate_existing_target(path, true)?;
    let file = File::open(path)?;
    // Re-check symlink status, identity, and mode bits after the open so a swap
    // between validation and open cannot smuggle in an unsafe target (TOCTOU).
    let path_metadata = std::fs::symlink_metadata(path)?;
    if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
        return Err(AtomicFileError::TargetChanged);
    }
    #[cfg(unix)]
    {
        let file_metadata = file.metadata()?;
        if file_metadata.dev() != path_metadata.dev() || file_metadata.ino() != path_metadata.ino()
        {
            return Err(AtomicFileError::TargetChanged);
        }
        if file_metadata.mode() & 0o077 != 0 {
            return Err(AtomicFileError::Permissions {
                found: file_metadata.mode() & 0o777,
            });
        }
    }
    Ok(file)
}

fn validate_existing_target(path: &Path, required: bool) -> Result<(), AtomicFileError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !required => return Ok(()),
        Err(error) => return Err(AtomicFileError::Io(error)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AtomicFileError::NotRegularFile);
    }
    #[cfg(unix)]
    if metadata.mode() & 0o077 != 0 {
        return Err(AtomicFileError::Permissions {
            found: metadata.mode() & 0o777,
        });
    }
    Ok(())
}

fn temporary_path(path: &Path) -> Result<PathBuf, AtomicFileError> {
    let parent = path
        .parent()
        .filter(|value| !value.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .ok_or(AtomicFileError::PathMissingFileName)?;
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(AtomicFileError::Entropy)?;
    Ok(parent.join(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        hex_lower(&random)
    )))
}

fn create_private_file(path: &Path) -> Result<File, AtomicFileError> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    Ok(options.open(path)?)
}

fn write_synced(file: &mut File, bytes: &[u8]) -> Result<(), AtomicFileError> {
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_data()?;
    Ok(())
}

fn sync_parent(path: &Path) -> Result<(), AtomicFileError> {
    let parent = path
        .parent()
        .filter(|value| !value.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    File::open(parent)?.sync_all()?;
    Ok(())
}

pub(crate) fn content_digest(domain: &[u8], bytes: &[u8]) -> Result<String, AtomicFileError> {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(
        u64::try_from(bytes.len())
            .map_err(|_| AtomicFileError::Counter)?
            .to_be_bytes(),
    );
    hasher.update(bytes);
    Ok(hex_lower(&hasher.finalize()))
}

#[derive(Debug, Error)]
pub(crate) enum AtomicFileError {
    #[error("checkpoint I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("operating-system entropy failed: {0}")]
    Entropy(getrandom::Error),
    #[error("checkpoint counter exhausted")]
    Counter,
    #[error("checkpoint would reach {attempted} bytes; maximum is {maximum}")]
    SizeLimit { attempted: u64, maximum: u64 },
    #[error("checkpoint target must be a regular file, not a symlink")]
    NotRegularFile,
    #[error("checkpoint target changed while it was being opened")]
    TargetChanged,
    #[error("checkpoint permissions must deny group and other access; found {found:03o}")]
    Permissions { found: u32 },
    #[error("checkpoint path must name a file")]
    PathMissingFileName,
    #[error("checkpoint maximum bytes must be greater than zero")]
    MaximumIsZero,
}
