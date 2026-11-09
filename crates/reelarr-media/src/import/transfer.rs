//! Safe file transfer into the library.
//!
//! The destination is never overwritten, and the source is only removed in
//! move mode after the destination is verified.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::scan::PARTIAL_SUFFIX;

/// How files get into the library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferMode {
    /// Hardlink when source and library share a filesystem, otherwise copy.
    /// The source is kept. This is the default.
    #[default]
    HardlinkOrCopy,
    /// Always copy; the source is kept.
    Copy,
    /// Rename on the same filesystem, otherwise copy then delete the source.
    Move,
}

/// How a file was transferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transferred {
    Hardlinked,
    Copied,
    Moved,
}

/// Transfer `src` to `dest`. Fails with `AlreadyExists` if `dest` exists.
pub fn transfer(src: &Path, dest: &Path, mode: TransferMode) -> io::Result<Transferred> {
    if dest.symlink_metadata().is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("destination already exists: {}", dest.display()),
        ));
    }
    match mode {
        TransferMode::HardlinkOrCopy => match fs::hard_link(src, dest) {
            Ok(()) => Ok(Transferred::Hardlinked),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(e),
            Err(e) => {
                tracing::debug!(error = %e, "hardlink failed, copying instead");
                safe_copy(src, dest).map(|_| Transferred::Copied)
            }
        },
        TransferMode::Copy => safe_copy(src, dest).map(|_| Transferred::Copied),
        TransferMode::Move => {
            // Link first so an existing destination is never clobbered, then
            // drop the source name.
            match fs::hard_link(src, dest) {
                Ok(()) => {
                    fs::remove_file(src)?;
                    Ok(Transferred::Moved)
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(e),
                Err(_) => {
                    safe_copy(src, dest)?;
                    fs::remove_file(src)?;
                    Ok(Transferred::Moved)
                }
            }
        }
    }
}

/// Copy through a `.reelarr-partial` temporary in the destination folder,
/// fsync, verify the size, then link it into place without overwriting.
/// The temporary is removed on every path.
pub fn safe_copy(src: &Path, dest: &Path) -> io::Result<()> {
    let partial = partial_path(dest);
    let result = (|| {
        let expected = fs::metadata(src)?.len();
        {
            let mut out = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&partial)?;
            let mut input = fs::File::open(src)?;
            io::copy(&mut input, &mut out)?;
            out.sync_all()?;
        }
        let written = fs::metadata(&partial)?.len();
        if written != expected {
            return Err(io::Error::other(format!(
                "copy size mismatch: expected {expected} bytes, wrote {written}"
            )));
        }
        // hard_link fails if dest appeared meanwhile, so nothing is clobbered.
        fs::hard_link(&partial, dest).or_else(|e| {
            if e.kind() == io::ErrorKind::AlreadyExists {
                Err(e)
            } else if dest.symlink_metadata().is_ok() {
                Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "destination appeared",
                ))
            } else {
                // Filesystems without hardlinks (some SMB mounts).
                fs::rename(&partial, dest)
            }
        })?;
        if let Some(Ok(d)) = dest.parent().map(fs::File::open) {
            let _ = d.sync_all();
        }
        Ok(())
    })();
    let _ = fs::remove_file(&partial);
    result
}

pub(crate) fn partial_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(PARTIAL_SUFFIX);
    dest.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_and_collisions() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.mkv");
        fs::write(&src, b"video").unwrap();

        let linked = dir.path().join("linked.mkv");
        assert_eq!(
            transfer(&src, &linked, TransferMode::HardlinkOrCopy).unwrap(),
            Transferred::Hardlinked
        );
        let copied = dir.path().join("copied.mkv");
        assert_eq!(
            transfer(&src, &copied, TransferMode::Copy).unwrap(),
            Transferred::Copied
        );
        assert_eq!(fs::read(&copied).unwrap(), b"video");
        assert!(!partial_path(&copied).exists());

        // Never overwrite.
        fs::write(&copied, b"other").unwrap();
        let err = transfer(&src, &copied, TransferMode::Copy).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&copied).unwrap(), b"other");
        assert!(src.exists(), "source kept");

        let moved = dir.path().join("moved.mkv");
        assert_eq!(
            transfer(&src, &moved, TransferMode::Move).unwrap(),
            Transferred::Moved
        );
        assert!(!src.exists());
        assert_eq!(fs::read(&moved).unwrap(), b"video");
    }

    #[test]
    fn failed_copy_leaves_no_partial_and_keeps_source() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.mkv");
        fs::write(&src, b"video").unwrap();
        let dest = dir.path().join("no-such-dir").join("dest.mkv");
        assert!(safe_copy(&src, &dest).is_err());
        assert!(src.exists());
        assert!(!partial_path(&dest).exists());
    }
}
