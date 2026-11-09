//! Importing completed downloads into the library.
//!
//! [`Importer::import`] takes an [`ImportRequest`] and:
//!
//! 1. resolves the movie (the given `movie_id`, else [`ReleaseMatcher`]),
//! 2. picks the main video from the output, skipping samples and refusing
//!    output with leftover archives (extraction did not finish),
//! 3. renders the destination from a configurable template,
//! 4. hardlinks, copies or moves it in without ever overwriting, and
//! 5. probes the new file and records it in the [`MediaFileStore`].
//!
//! The original download is never touched unless [`TransferMode::Move`] is
//! chosen, and even then only after the destination is verified. Download
//! cleanup belongs to the download client.
//!
//! This module defines its own request and outcome types. The job-level
//! `ImportHandler` contract from `reelarr-core` (#29) is a thin adapter over
//! it.

pub mod handler;
pub mod matcher;
pub mod naming;
pub mod select;
pub mod transfer;

pub use handler::{HandlerOptions, MediaImportHandler};
pub use matcher::{ReleaseMatcher, TitleYearMatcher};
pub use naming::{DEFAULT_TEMPLATE, NamingValues};
pub use transfer::{TransferMode, Transferred};

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use reelarr_core::FailureCode;
use reelarr_types::{
    LibraryRootId, MediaFileId, MediaFileStatus, MediaKind, MediaProbe, Movie, MovieId,
};

use crate::scan::{self, MediaFileStore, NewMediaFile, Prober};
use crate::store::{MovieStore, StoreError};

/// One download to import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportRequest {
    /// Set for every download Reelarr grabbed itself.
    pub movie_id: Option<MovieId>,
    /// Local, already path-mapped folder or file.
    pub output_path: PathBuf,
    /// Release name, used to match when `movie_id` is missing.
    pub release_title: String,
    pub category: Option<String>,
}

/// Result of an import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportOutcome {
    Imported {
        media_file_id: MediaFileId,
        path: PathBuf,
        transferred: Transferred,
    },
    Failed {
        reason: String,
        retryable: bool,
    },
    /// No library movie could be matched; a manual import can resolve it.
    NoMatch,
}

/// Importer settings.
#[derive(Debug, Clone)]
pub struct ImportOptions {
    /// Library folder movies are placed in.
    pub library_path: PathBuf,
    /// Library root id recorded on imported files.
    pub library_root_id: LibraryRootId,
    /// Rename template; see [`naming::render`].
    pub template: String,
    pub mode: TransferMode,
    /// Videos smaller than this are treated as samples.
    pub min_video_bytes: u64,
    /// Carry subtitle sidecars over next to the movie.
    pub import_subtitles: bool,
}

impl ImportOptions {
    pub fn new(library_path: impl Into<PathBuf>, library_root_id: LibraryRootId) -> Self {
        Self {
            library_path: library_path.into(),
            library_root_id,
            template: DEFAULT_TEMPLATE.into(),
            mode: TransferMode::default(),
            min_video_bytes: 50 * 1024 * 1024,
            import_subtitles: true,
        }
    }
}

/// Imports completed downloads.
#[derive(Debug)]
pub struct Importer<M, F, P, R> {
    movies: M,
    files: F,
    prober: P,
    matcher: R,
    options: ImportOptions,
}

impl<M: MovieStore, F: MediaFileStore, P: Prober, R: ReleaseMatcher> Importer<M, F, P, R> {
    pub fn new(movies: M, files: F, prober: P, matcher: R, options: ImportOptions) -> Self {
        Self {
            movies,
            files,
            prober,
            matcher,
            options,
        }
    }

    pub fn files(&self) -> &F {
        &self.files
    }

    pub async fn import(&self, request: &ImportRequest) -> ImportOutcome {
        match self.try_import(request).await {
            Ok(outcome) => outcome,
            Err(failure) => {
                tracing::warn!(
                    release = %request.release_title,
                    reason = %failure.reason,
                    retryable = failure.retryable,
                    "import failed"
                );
                ImportOutcome::Failed {
                    reason: failure.reason,
                    retryable: failure.retryable,
                }
            }
        }
    }

    async fn try_import(&self, request: &ImportRequest) -> Result<ImportOutcome, Failure> {
        let Some(prepared) = self.prepare(request).await? else {
            return Ok(ImportOutcome::NoMatch);
        };
        let dest = self.options.library_path.join(&prepared.rel_dest);
        let transferred = self.transfer_into(&prepared, &dest).await?;
        let media_file_id = self
            .record(&prepared, self.options.library_root_id, &dest)
            .await?;
        Ok(ImportOutcome::Imported {
            media_file_id,
            path: dest,
            transferred,
        })
    }

    /// Steps 1 to 3: the movie, the main file and its destination relative
    /// to a library root. Touches nothing. `None` means no movie matched.
    async fn prepare(&self, request: &ImportRequest) -> Result<Option<Prepared>, Failure> {
        // 1. Movie.
        let movie_id = match request.movie_id {
            Some(id) => id,
            None => match self.matcher.match_release(&request.release_title).await? {
                Some(id) => id,
                None => return Ok(None),
            },
        };
        let movie = self.movies.get(movie_id).await?.ok_or_else(|| {
            Failure::permanent(format!("movie {movie_id:?} is not in the library"))
        })?;

        // 2. Main file.
        let output = request.output_path.clone();
        if tokio::fs::symlink_metadata(&output).await.is_err() {
            return Err(Failure::retryable(format!(
                "download output not found: {}",
                output.display()
            )));
        }
        let min = self.options.min_video_bytes;
        let selection = blocking(move || select::select(&output, min)).await?;
        if !selection.leftovers.is_empty() {
            return Err(Failure::permanent(format!(
                "extraction incomplete: {} archive or par2 files left in the download",
                selection.leftovers.len()
            )));
        }
        let (source, kind) = match (selection.main, selection.image) {
            (Some((video, _)), _) => (video, MediaKind::File),
            (None, Some((image, _))) => {
                let check = image.clone();
                if blocking(move || scan::disc::has_disc_signature(&check)).await? {
                    (image, MediaKind::DiscImage)
                } else {
                    return Err(Failure::permanent(format!(
                        "{} is not a recognized disc image",
                        image.display()
                    )));
                }
            }
            (None, None) if !selection.disc_folders.is_empty() => {
                (selection.disc_folders[0].clone(), MediaKind::DiscFolder)
            }
            (None, None) => {
                return Err(Failure::permanent(
                    "no importable video file in the download".into(),
                ));
            }
        };

        // 3. Destination name. Probe the source for the quality label
        // (disc images are never probed).
        let source_probe = if kind.is_disc() {
            None
        } else {
            self.prober.probe(&source).await.ok()
        };
        let values = NamingValues {
            title: movie.metadata.title.clone(),
            year: movie.metadata.year,
            quality: source_probe.as_ref().and_then(naming::quality_label),
            tmdb_id: Some(movie.metadata.external_ids.tmdb_id),
            imdb_id: movie.metadata.external_ids.imdb_id.clone(),
        };
        let mut parts = naming::render(&self.options.template, &values);
        let Some(stem) = parts.pop() else {
            return Err(Failure::permanent(
                "rename template produced an empty name".into(),
            ));
        };
        let file_name = if kind == MediaKind::DiscFolder {
            stem
        } else {
            let ext = source
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("mkv")
                .to_ascii_lowercase();
            format!("{stem}.{ext}")
        };
        let rel_dest = parts
            .iter()
            .fold(PathBuf::new(), |p, c| p.join(c))
            .join(file_name);
        if !rel_dest
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
        {
            return Err(Failure::permanent(format!(
                "rename template produced a path outside the library: {}",
                rel_dest.display()
            )));
        }
        Ok(Some(Prepared {
            movie,
            source,
            kind,
            source_probe,
            rel_dest,
            subtitles: selection.subtitles,
        }))
    }

    /// Step 4: transfer the main file and its subtitles to `dest`.
    async fn transfer_into(
        &self,
        prepared: &Prepared,
        dest: &Path,
    ) -> Result<Transferred, Failure> {
        let mode = self.options.mode;
        let dest_dir = dest.parent().map(Path::to_owned).unwrap_or_default();
        let (src, dst) = (prepared.source.clone(), dest.to_owned());
        let tree = prepared.kind == MediaKind::DiscFolder;
        let transferred = tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&dest_dir).map_err(transfer::TransferError::Dest)?;
            transfer::write_probe(&dest_dir)?;
            if tree {
                transfer::transfer_tree(&src, &dst, mode)
            } else {
                transfer::transfer(&src, &dst, mode)
            }
        })
        .await
        .map_err(|e| Failure::retryable(format!("import task failed: {e}")))?
        .map_err(Failure::from)?;
        self.transfer_subtitles(prepared, dest).await;
        Ok(transferred)
    }

    async fn transfer_subtitles(&self, prepared: &Prepared, dest: &Path) {
        if !self.options.import_subtitles || prepared.kind.is_disc() {
            return;
        }
        let subs = prepared.subtitles.clone();
        let (src, dst) = (prepared.source.clone(), dest.to_owned());
        let mode = self.options.mode;
        if let Ok(copied) = blocking(move || Ok(copy_subtitles(&src, &dst, &subs, mode))).await
            && copied > 0
        {
            tracing::debug!(copied, "imported subtitle sidecars");
        }
    }

    /// Step 5: probe the imported file and record it under `root_id`.
    async fn record(
        &self,
        prepared: &Prepared,
        root_id: LibraryRootId,
        dest: &Path,
    ) -> Result<MediaFileId, Failure> {
        let kind = prepared.kind;
        let probe = if kind.is_disc() {
            None
        } else {
            self.prober
                .probe(dest)
                .await
                .ok()
                .or_else(|| prepared.source_probe.clone())
        };
        self.record_file(prepared.movie.id, root_id, dest, kind, probe)
            .await
    }

    async fn record_file(
        &self,
        movie_id: MovieId,
        root_id: LibraryRootId,
        dest: &Path,
        kind: MediaKind,
        probe: Option<MediaProbe>,
    ) -> Result<MediaFileId, Failure> {
        let dest_buf = dest.to_owned();
        let folder = kind == MediaKind::DiscFolder;
        let (size_bytes, modified_at) = blocking(move || {
            if folder {
                let limits = scan::disc::FolderLimits {
                    max_entries: 20_000,
                    deadline: std::time::Instant::now() + std::time::Duration::from_secs(60),
                };
                scan::disc::folder_size(&dest_buf, limits).map_err(std::io::Error::other)
            } else {
                let meta = std::fs::metadata(&dest_buf)?;
                let modified_at = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                Ok((meta.len(), modified_at))
            }
        })
        .await?;
        let dest_str = dest.to_string_lossy().into_owned();
        let media_file_id = match self.files.get_by_path(&dest_str).await? {
            Some(mut existing) => {
                existing.movie_id = Some(movie_id);
                existing.root_id = root_id;
                existing.size_bytes = size_bytes;
                existing.modified_at = modified_at;
                existing.status = MediaFileStatus::Present;
                existing.probe = probe;
                existing.kind = kind;
                self.files.update(&existing).await?;
                existing.id
            }
            None => {
                self.files
                    .insert(NewMediaFile {
                        movie_id: Some(movie_id),
                        root_id,
                        path: dest_str,
                        size_bytes,
                        modified_at,
                        probe,
                        kind,
                    })
                    .await?
                    .id
            }
        };
        tracing::info!(?movie_id, dest = %dest.display(), "imported movie");
        Ok(media_file_id)
    }
}

/// A download ready to transfer: everything decided, nothing touched.
#[derive(Debug, Clone)]
struct Prepared {
    movie: Movie,
    source: PathBuf,
    kind: MediaKind,
    source_probe: Option<MediaProbe>,
    /// Destination relative to the library root; only normal components.
    rel_dest: PathBuf,
    subtitles: Vec<PathBuf>,
}

/// Copy or link subtitle sidecars, keeping language suffixes such as `.en`
/// or `.en.forced`. Best effort: failures are logged and skipped.
fn copy_subtitles(source: &Path, dest: &Path, subs: &[PathBuf], mode: TransferMode) -> usize {
    let src_stem = source.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let dest_stem = dest.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let Some(dir) = dest.parent() else { return 0 };
    let mut copied = 0;
    for sub in subs {
        let Some(name) = sub.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // Keep the part after the video's stem, e.g. ".en.srt"; otherwise
        // just the extension.
        let suffix = name
            .strip_prefix(src_stem)
            .filter(|s| s.starts_with('.'))
            .map(str::to_owned)
            .unwrap_or_else(|| {
                format!(
                    ".{}",
                    sub.extension().and_then(|e| e.to_str()).unwrap_or("srt")
                )
            });
        let target = dir.join(format!(
            "{dest_stem}{}",
            naming::sanitize_component(&suffix)
        ));
        // Sidecars are small; never move them so the download stays intact.
        let sub_mode = match mode {
            TransferMode::Move => TransferMode::Copy,
            other => other,
        };
        match transfer::transfer(sub, &target, sub_mode) {
            Ok(_) => copied += 1,
            Err(e) => tracing::debug!(sub = %sub.display(), error = %e, "skipped subtitle"),
        }
    }
    copied
}

#[derive(Debug)]
struct Failure {
    reason: String,
    retryable: bool,
    code: Option<FailureCode>,
}

impl Failure {
    fn permanent(reason: String) -> Self {
        Self {
            reason,
            retryable: false,
            code: None,
        }
    }
    fn retryable(reason: String) -> Self {
        Self {
            reason,
            retryable: true,
            code: None,
        }
    }
    fn with_code(mut self, code: FailureCode) -> Self {
        self.code = Some(code);
        self
    }
}

impl From<StoreError> for Failure {
    fn from(e: StoreError) -> Self {
        Self::retryable(e.to_string())
    }
}

impl From<std::io::Error> for Failure {
    /// Used for download-side and other non-library I/O. AlreadyExists here
    /// is not a library destination conflict; those go through
    /// [`Failure::from_library_io`] via [`transfer::TransferError::Dest`].
    fn from(e: std::io::Error) -> Self {
        Self::retryable(format!("file operation failed: {e}"))
    }
}

impl From<transfer::TransferError> for Failure {
    fn from(e: transfer::TransferError) -> Self {
        match e {
            transfer::TransferError::Source(e) => Self::from(e),
            transfer::TransferError::Dest(e) => Self::from_library_io(e),
        }
    }
}

impl Failure {
    /// Maps errors from writing into a library root: disk full and a vanished
    /// share are worth retrying; a read-only or forbidden library and an
    /// existing destination are not.
    fn from_library_io(e: std::io::Error) -> Self {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            let reason = if e.to_string().contains(crate::scan::PARTIAL_SUFFIX) {
                format!(
                    "leftover {} file from a previous import: {e}",
                    crate::scan::PARTIAL_SUFFIX
                )
            } else {
                format!("not overwriting existing library file: {e}")
            };
            return Self::permanent(reason).with_code(FailureCode::DestinationConflict);
        }
        match e.raw_os_error() {
            Some(code) if code == libc::ENOSPC => {
                Self::retryable(format!("library is full: {e}")).with_code(FailureCode::DiskFull)
            }
            Some(code) if code == libc::EROFS || code == libc::EACCES || code == libc::EPERM => {
                Self::permanent(format!("library refused the write: {e}"))
                    .with_code(FailureCode::LibraryPermissionDenied)
            }
            Some(code)
                if code == libc::ESTALE
                    || code == libc::ENOTCONN
                    || code == libc::EIO
                    || code == libc::EHOSTDOWN
                    || code == libc::EHOSTUNREACH
                    || code == libc::ETIMEDOUT =>
            {
                Self::retryable(format!("library share is unavailable: {e}"))
                    .with_code(FailureCode::RootOffline)
            }
            _ => Self::retryable(format!("file operation failed: {e}")),
        }
    }
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> std::io::Result<T> + Send + 'static,
) -> Result<T, Failure> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Failure::retryable(format!("import task failed: {e}")))?
        .map_err(Failure::from)
}

#[cfg(test)]
mod errno_tests {
    use super::*;

    #[test]
    fn download_errors_are_not_mapped_as_library_failures() {
        let denied = Failure::from(std::io::Error::from_raw_os_error(libc::EACCES));
        assert!(denied.retryable);
        assert_eq!(denied.code, None);
        let io = Failure::from(std::io::Error::from_raw_os_error(libc::EIO));
        assert!(io.retryable);
        assert_eq!(io.code, None);
        let exists = Failure::from(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "download path",
        ));
        assert!(exists.retryable);
        assert_eq!(exists.code, None);
        let src_denied = Failure::from(transfer::TransferError::Source(
            std::io::Error::from_raw_os_error(libc::EACCES),
        ));
        assert!(src_denied.retryable);
        assert_eq!(src_denied.code, None);
        let dest_denied = Failure::from(transfer::TransferError::Dest(
            std::io::Error::from_raw_os_error(libc::EACCES),
        ));
        assert!(!dest_denied.retryable);
        assert_eq!(dest_denied.code, Some(FailureCode::LibraryPermissionDenied));
        let dest_io = Failure::from(transfer::TransferError::Dest(
            std::io::Error::from_raw_os_error(libc::EIO),
        ));
        assert!(dest_io.retryable);
        assert_eq!(dest_io.code, Some(FailureCode::RootOffline));
        let leftover = Failure::from(transfer::TransferError::Dest(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "leftover partial file already exists: dest{}",
                crate::scan::PARTIAL_SUFFIX
            ),
        )));
        assert!(!leftover.retryable);
        assert_eq!(leftover.code, Some(FailureCode::DestinationConflict));
        assert!(
            leftover.reason.contains(crate::scan::PARTIAL_SUFFIX),
            "{}",
            leftover.reason
        );
        assert!(
            leftover.reason.contains("previous import"),
            "{}",
            leftover.reason
        );
        assert!(
            !leftover
                .reason
                .contains("not overwriting existing library file"),
            "{}",
            leftover.reason
        );
    }

    #[test]
    fn library_writes_map_errnos() {
        let denied = Failure::from_library_io(std::io::Error::from_raw_os_error(libc::EACCES));
        assert!(!denied.retryable);
        assert_eq!(denied.code, Some(FailureCode::LibraryPermissionDenied));
        let full = Failure::from_library_io(std::io::Error::from_raw_os_error(libc::ENOSPC));
        assert!(full.retryable);
        assert_eq!(full.code, Some(FailureCode::DiskFull));
        let stale = Failure::from_library_io(std::io::Error::from_raw_os_error(libc::ESTALE));
        assert!(stale.retryable);
        assert_eq!(stale.code, Some(FailureCode::RootOffline));
    }
}
