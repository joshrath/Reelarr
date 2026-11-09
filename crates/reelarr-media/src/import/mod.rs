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

pub mod matcher;
pub mod naming;
pub mod select;
pub mod transfer;

pub use matcher::{ReleaseMatcher, TitleYearMatcher};
pub use naming::{DEFAULT_TEMPLATE, NamingValues};
pub use transfer::{TransferMode, Transferred};

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use reelarr_types::{LibraryRootId, MediaFileId, MediaFileStatus, MediaKind, MovieId};

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
        // 1. Movie.
        let movie_id = match request.movie_id {
            Some(id) => id,
            None => match self.matcher.match_release(&request.release_title).await? {
                Some(id) => id,
                None => return Ok(ImportOutcome::NoMatch),
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
                return Err(Failure::permanent(format!(
                    "{} is a DVD/Blu-ray folder rip, which can't be imported yet",
                    selection.disc_folders[0].display()
                )));
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
        let ext = source
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("mkv")
            .to_ascii_lowercase();
        let dest_dir = parts
            .iter()
            .fold(self.options.library_path.clone(), |p, c| p.join(c));
        let dest = dest_dir.join(format!("{stem}.{ext}"));

        // 4. Transfer.
        let mode = self.options.mode;
        let (src, dst, dir) = (source.clone(), dest.clone(), dest_dir.clone());
        let transferred = blocking(move || {
            std::fs::create_dir_all(&dir)?;
            transfer::transfer(&src, &dst, mode)
        })
        .await?;

        if self.options.import_subtitles && !kind.is_disc() {
            let subs = selection.subtitles.clone();
            let (src, dst) = (source.clone(), dest.clone());
            let copied = blocking(move || Ok(copy_subtitles(&src, &dst, &subs, mode))).await?;
            if copied > 0 {
                tracing::debug!(copied, "imported subtitle sidecars");
            }
        }

        // 5. Record.
        let probe = if kind.is_disc() {
            None
        } else {
            self.prober.probe(&dest).await.ok().or(source_probe)
        };
        let meta = tokio::fs::metadata(&dest)
            .await
            .map_err(|e| Failure::retryable(format!("cannot read imported file: {e}")))?;
        let modified_at = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let dest_str = dest.to_string_lossy().into_owned();
        let media_file_id = match self.files.get_by_path(&dest_str).await? {
            Some(mut existing) => {
                existing.movie_id = Some(movie.id);
                existing.size_bytes = meta.len();
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
                        movie_id: Some(movie.id),
                        root_id: self.options.library_root_id,
                        path: dest_str,
                        size_bytes: meta.len(),
                        modified_at,
                        probe,
                        kind,
                    })
                    .await?
                    .id
            }
        };
        tracing::info!(
            movie = %movie.metadata.title,
            dest = %dest.display(),
            ?transferred,
            "imported movie"
        );
        Ok(ImportOutcome::Imported {
            media_file_id,
            path: dest,
            transferred,
        })
    }
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
}

impl Failure {
    fn permanent(reason: String) -> Self {
        Self {
            reason,
            retryable: false,
        }
    }
    fn retryable(reason: String) -> Self {
        Self {
            reason,
            retryable: true,
        }
    }
}

impl From<StoreError> for Failure {
    fn from(e: StoreError) -> Self {
        Self::retryable(e.to_string())
    }
}

impl From<std::io::Error> for Failure {
    fn from(e: std::io::Error) -> Self {
        match e.kind() {
            std::io::ErrorKind::AlreadyExists => {
                Self::permanent(format!("not overwriting existing library file: {e}"))
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
