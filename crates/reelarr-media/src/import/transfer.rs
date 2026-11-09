//! Safe file transfer into the library.
//!
//! The destination is never overwritten, and the source is only removed in
//! move mode after the destination is verified.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::scan::PARTIAL_SUFFIX;
use crate::scan::disc::{self, FolderLimits};

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

/// An I/O error from transferring a file, tagged by which side failed so
/// callers can map library-dest errors without misclassifying the download.
#[derive(Debug)]
pub enum TransferError {
    /// Opening, stating, reading, or removing the download source.
    Source(io::Error),
    /// Creating directories, linking, writing a partial, or renaming into place.
    Dest(io::Error),
}

impl TransferError {
    pub fn kind(&self) -> io::ErrorKind {
        match self {
            Self::Source(e) | Self::Dest(e) => e.kind(),
        }
    }
}

impl std::fmt::Display for TransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Source(e) | Self::Dest(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for TransferError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Source(e) | Self::Dest(e) => Some(e),
        }
    }
}

/// Transfer `src` to `dest`. Fails with `AlreadyExists` if `dest` exists.
pub fn transfer(src: &Path, dest: &Path, mode: TransferMode) -> Result<Transferred, TransferError> {
    match dest.symlink_metadata() {
        Ok(_) => {
            return Err(TransferError::Dest(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("destination already exists: {}", dest.display()),
            )));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(TransferError::Dest(e)),
    }
    match mode {
        TransferMode::HardlinkOrCopy => match fs::hard_link(src, dest) {
            Ok(()) => Ok(Transferred::Hardlinked),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(TransferError::Dest(e)),
            Err(e) => {
                if e.raw_os_error() == Some(libc::EXDEV) {
                    tracing::debug!("hardlink crossed filesystems (EXDEV), copying instead");
                } else {
                    tracing::debug!(error = %e, "hardlink failed, copying instead");
                }
                safe_copy(src, dest).map(|_| Transferred::Copied)
            }
        },
        TransferMode::Copy => safe_copy(src, dest).map(|_| Transferred::Copied),
        TransferMode::Move => {
            // Link first so an existing destination is never clobbered, then
            // drop the source name.
            match fs::hard_link(src, dest) {
                Ok(()) => {
                    fs::remove_file(src).map_err(TransferError::Source)?;
                    Ok(Transferred::Moved)
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(TransferError::Dest(e)),
                Err(e) => {
                    if e.raw_os_error() == Some(libc::EXDEV) {
                        tracing::debug!("hardlink crossed filesystems (EXDEV), copying instead");
                    }
                    safe_copy(src, dest)?;
                    fs::remove_file(src).map_err(TransferError::Source)?;
                    Ok(Transferred::Moved)
                }
            }
        }
    }
}

/// Copy through a `.reelarr-partial` temporary in the destination folder,
/// fsync, verify the size, then link it into place without overwriting.
/// The temporary is removed on every path.
pub fn safe_copy(src: &Path, dest: &Path) -> Result<(), TransferError> {
    let partial = partial_path(dest);
    let result = (|| {
        let expected = fs::metadata(src).map_err(TransferError::Source)?.len();
        {
            let mut out = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&partial)
                .map_err(TransferError::Dest)?;
            copy_bytes(src, &mut out)?;
            out.sync_all().map_err(TransferError::Dest)?;
        }
        let written = fs::metadata(&partial).map_err(TransferError::Dest)?.len();
        if written != expected {
            return Err(TransferError::Dest(io::Error::other(format!(
                "copy size mismatch: expected {expected} bytes, wrote {written}"
            ))));
        }
        // hard_link fails if dest appeared meanwhile, so nothing is clobbered.
        fs::hard_link(&partial, dest).or_else(|e| {
            if e.kind() == io::ErrorKind::AlreadyExists {
                Err(TransferError::Dest(e))
            } else {
                match dest.symlink_metadata() {
                    Ok(_) => Err(TransferError::Dest(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "destination appeared",
                    ))),
                    Err(meta_err) if meta_err.kind() != io::ErrorKind::NotFound => {
                        Err(TransferError::Dest(meta_err))
                    }
                    Err(_) => {
                        // Filesystems without hardlinks (some SMB mounts).
                        fs::rename(&partial, dest).map_err(TransferError::Dest)
                    }
                }
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

/// Reads the download and writes the library file, so each side's errors
/// stay classified.
fn copy_bytes(src: &Path, out: &mut fs::File) -> Result<(), TransferError> {
    let mut input = fs::File::open(src).map_err(TransferError::Source)?;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = input.read(&mut buf).map_err(TransferError::Source)?;
        if n == 0 {
            return Ok(());
        }
        out.write_all(&buf[..n]).map_err(TransferError::Dest)?;
    }
}

/// Create and delete a `.reelarr-partial` file in `dir` to prove the library
/// root accepts writes (EROFS, EACCES) before a long copy starts. The scanner
/// already ignores names ending in [`PARTIAL_SUFFIX`].
pub fn write_probe(dir: &Path) -> Result<(), TransferError> {
    fs::create_dir_all(dir).map_err(TransferError::Dest)?;
    let probe = dir.join(format!("write-check{PARTIAL_SUFFIX}"));
    let _ = fs::remove_file(&probe);
    let result = (|| {
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
            .map_err(TransferError::Dest)?;
        file.sync_all().map_err(TransferError::Dest)
    })();
    let _ = fs::remove_file(&probe);
    result
}

/// Transfer every regular file under a disc folder `src` into `dest`,
/// preserving relative paths. `src` is either a recognized disc folder
/// (it contains `VIDEO_TS` or `BDMV`) or a bare structure directory; in
/// the latter case sibling disc-structure directories (`AUDIO_TS`,
/// `CERTIFICATE`, `AACS`) are included too. Existing dest files of the
/// same size are skipped so a crash can resume; a different file is an
/// error. In move mode sources are removed only after every dest file
/// is verified.
pub fn transfer_tree(
    src: &Path,
    dest: &Path,
    mode: TransferMode,
) -> Result<Transferred, TransferError> {
    match dest.symlink_metadata() {
        Ok(meta) if !meta.file_type().is_dir() => {
            return Err(TransferError::Dest(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "destination already exists and is not a folder: {}",
                    dest.display()
                ),
            )));
        }
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(dest).map_err(TransferError::Dest)?;
        }
        Err(e) => return Err(TransferError::Dest(e)),
    }

    let files = list_tree_files(src, dest)?;
    if files.is_empty() {
        return Err(TransferError::Source(io::Error::other(format!(
            "disc folder has no files: {}",
            src.display()
        ))));
    }

    let file_mode = match mode {
        TransferMode::Move => TransferMode::HardlinkOrCopy,
        other => other,
    };
    let mut copied = 0usize;
    for (src_file, dest_file) in &files {
        if let Some(parent) = dest_file.parent() {
            fs::create_dir_all(parent).map_err(TransferError::Dest)?;
        }
        match dest_file.symlink_metadata() {
            Ok(_) => {
                let expected = fs::metadata(src_file).map_err(TransferError::Source)?.len();
                let dest_meta = fs::metadata(dest_file).map_err(TransferError::Dest)?;
                if !dest_meta.is_file() || dest_meta.len() != expected {
                    return Err(TransferError::Dest(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("a different file already exists at {}", dest_file.display()),
                    )));
                }
                continue;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(TransferError::Dest(e)),
        }
        match transfer(src_file, dest_file, file_mode)? {
            Transferred::Copied => copied += 1,
            Transferred::Hardlinked | Transferred::Moved => {}
        }
    }

    if mode == TransferMode::Move {
        for (src_file, dest_file) in &files {
            let expected = fs::metadata(src_file).map_err(TransferError::Source)?.len();
            let dest_meta = fs::metadata(dest_file).map_err(TransferError::Dest)?;
            if !dest_meta.is_file() || dest_meta.len() != expected {
                return Err(TransferError::Dest(io::Error::other(format!(
                    "copy size mismatch: expected {expected} bytes at {}",
                    dest_file.display()
                ))));
            }
        }
        remove_tree_sources(&files)?;
        return Ok(Transferred::Moved);
    }
    Ok(if copied > 0 {
        Transferred::Copied
    } else {
        Transferred::Hardlinked
    })
}

/// Total size of the files [`transfer_tree`] would copy from `src`.
pub fn tree_payload_size(src: &Path) -> Result<u64, TransferError> {
    let dummy = PathBuf::from("/");
    let mut total = 0u64;
    for (src_file, _) in list_tree_files(src, &dummy)? {
        total = total.saturating_add(
            fs::metadata(&src_file)
                .map_err(TransferError::Source)?
                .len(),
        );
    }
    Ok(total)
}

/// Whether `dest` is a disc folder whose files add up to `expected_bytes`.
/// `NotFound` and other I/O errors are returned so the caller can treat a
/// vanished share as offline rather than a conflict.
pub fn dest_tree_complete(dest: &Path, expected_bytes: u64) -> io::Result<bool> {
    let meta = dest.symlink_metadata()?;
    if !meta.file_type().is_dir() {
        return Ok(false);
    }
    if disc::disc_folder_format(dest)?.is_none() {
        return Ok(false);
    }
    let limits = FolderLimits {
        max_entries: 20_000,
        deadline: Instant::now() + Duration::from_secs(60),
    };
    let (size, _) = disc::folder_size(dest, limits).map_err(io::Error::other)?;
    Ok(size == expected_bytes)
}

fn list_tree_files(src: &Path, dest: &Path) -> Result<Vec<(PathBuf, PathBuf)>, TransferError> {
    let (walk_root, copy_all) = tree_walk_root(src)?;
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(&walk_root).follow_links(false) {
        let entry = entry.map_err(|e| TransferError::Source(io::Error::other(e)))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = match entry.path().strip_prefix(&walk_root) {
            Ok(rel) => rel,
            Err(_) => continue,
        };
        if !copy_all {
            let first = rel
                .components()
                .next()
                .and_then(|c| c.as_os_str().to_str())
                .unwrap_or("");
            if !disc::is_disc_structure_dir(first) {
                continue;
            }
        }
        files.push((entry.path().to_owned(), dest.join(rel)));
    }
    Ok(files)
}

/// Root to walk and whether every file under it should be copied (`true`)
/// or only disc-structure subdirectories (`false`).
fn tree_walk_root(src: &Path) -> Result<(PathBuf, bool), TransferError> {
    match disc::disc_folder_format(src) {
        Ok(Some(_)) => return Ok((src.to_owned(), true)),
        Ok(None) => {}
        Err(e) => return Err(TransferError::Source(e)),
    }
    let name = src
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    if disc::is_disc_structure_dir(&name)
        && let Some(parent) = src.parent()
    {
        return Ok((parent.to_owned(), false));
    }
    Err(TransferError::Source(io::Error::other(format!(
        "not a disc folder: {}",
        src.display()
    ))))
}

fn remove_tree_sources(files: &[(PathBuf, PathBuf)]) -> Result<(), TransferError> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for (src_file, _) in files {
        fs::remove_file(src_file).map_err(TransferError::Source)?;
        if let Some(parent) = src_file.parent() {
            dirs.push(parent.to_owned());
        }
    }
    dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    dirs.dedup();
    for dir in dirs {
        let _ = fs::remove_dir(dir);
    }
    Ok(())
}

pub(crate) fn partial_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(PARTIAL_SUFFIX);
    dest.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

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

    fn bluray_tree(root: &Path) -> PathBuf {
        let src = root.join("Movie.2010.BluRay");
        fs::create_dir_all(src.join("BDMV/STREAM")).unwrap();
        fs::write(src.join("BDMV/index.bdmv"), b"idx").unwrap();
        fs::write(src.join("BDMV/STREAM/00001.m2ts"), b"stream-bytes").unwrap();
        src
    }

    #[test]
    fn tree_copy_preserves_layout_and_does_not_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let src = bluray_tree(dir.path());
        let dest = dir.path().join("library/Movie (2010)");
        assert_eq!(
            transfer_tree(&src, &dest, TransferMode::Copy).unwrap(),
            Transferred::Copied
        );
        assert_eq!(fs::read(dest.join("BDMV/index.bdmv")).unwrap(), b"idx");
        assert_eq!(
            fs::read(dest.join("BDMV/STREAM/00001.m2ts")).unwrap(),
            b"stream-bytes"
        );
        assert!(src.join("BDMV/index.bdmv").exists(), "source kept");
        assert_eq!(tree_payload_size(&src).unwrap(), 3 + 12);
        assert!(dest_tree_complete(&dest, 15).unwrap());

        fs::write(dest.join("BDMV/index.bdmv"), b"nope").unwrap();
        let err = transfer_tree(&src, &dest, TransferMode::Copy).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(dest.join("BDMV/index.bdmv")).unwrap(), b"nope");
    }

    #[test]
    fn tree_move_removes_sources_after_verify() {
        let dir = tempfile::tempdir().unwrap();
        let src = bluray_tree(dir.path());
        let dest = dir.path().join("library/Movie (2010)");
        assert_eq!(
            transfer_tree(&src, &dest, TransferMode::Move).unwrap(),
            Transferred::Moved
        );
        assert!(dest_tree_complete(&dest, 15).unwrap());
        assert!(!src.join("BDMV/index.bdmv").exists());
        assert!(!src.join("BDMV/STREAM/00001.m2ts").exists());
    }

    #[test]
    fn bare_video_ts_includes_audio_ts_sibling() {
        let dir = tempfile::tempdir().unwrap();
        let video_ts = dir.path().join("VIDEO_TS");
        fs::create_dir_all(&video_ts).unwrap();
        fs::write(video_ts.join("VIDEO_TS.IFO"), b"ifo").unwrap();
        fs::create_dir_all(dir.path().join("AUDIO_TS")).unwrap();
        fs::write(dir.path().join("AUDIO_TS/audio.bin"), b"aud").unwrap();
        fs::write(dir.path().join("cover.jpg"), b"art").unwrap();
        let dest = dir.path().join("library/Movie");
        transfer_tree(&video_ts, &dest, TransferMode::Copy).unwrap();
        assert!(dest.join("VIDEO_TS/VIDEO_TS.IFO").exists());
        assert!(dest.join("AUDIO_TS/audio.bin").exists());
        assert!(!dest.join("cover.jpg").exists());
    }

    #[test]
    fn write_probe_creates_and_removes_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("library");
        write_probe(&folder).unwrap();
        assert!(folder.is_dir());
        assert!(!folder.join(format!("write-check{PARTIAL_SUFFIX}")).exists());
    }

    #[test]
    fn hardlink_falls_back_to_copy_across_devices() {
        let shm = Path::new("/dev/shm");
        if !shm.is_dir() {
            return;
        }
        let src_dir = match tempfile::tempdir_in(shm) {
            Ok(dir) => dir,
            Err(_) => return,
        };
        let dest_dir = tempfile::tempdir().unwrap();
        if fs::metadata(src_dir.path()).unwrap().dev()
            == fs::metadata(dest_dir.path()).unwrap().dev()
        {
            return;
        }
        let src = src_dir.path().join("src.mkv");
        fs::write(&src, b"cross-device").unwrap();
        let dest = dest_dir.path().join("dest.mkv");
        assert_eq!(
            transfer(&src, &dest, TransferMode::HardlinkOrCopy).unwrap(),
            Transferred::Copied
        );
        assert_eq!(fs::read(&dest).unwrap(), b"cross-device");
        assert!(src.exists(), "source kept after copy fallback");
        assert_ne!(
            fs::metadata(&src).unwrap().ino(),
            fs::metadata(&dest).unwrap().ino()
        );
    }
}
