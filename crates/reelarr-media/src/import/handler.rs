//! The `reelarr-core` [`ImportHandler`] for completed downloads (#29).
//!
//! Each call is safe to repeat for the same job. A per-job ledger row is
//! written before any file is touched, so after a crash the next call can
//! tell its own half-finished transfer from someone else's file:
//!
//! - done: the same `Imported` result again,
//! - pending and the destination is ours: finish recording it,
//! - pending and the destination is missing: transfer again,
//! - the destination exists and is not ours: a terminal conflict.
//!
//! The library root is the movie's existing root, else the configured
//! default, else the only library root. It never falls back to another root:
//! an offline root is a retryable failure, not a reason to scatter a movie.

use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use reelarr_core::{CompletedDownload, FailureCode, ImportHandler, ImportResult};
use reelarr_db::Database;
use reelarr_types::{LibraryRoot, LibraryRootId, LibraryRootKind, MediaFileStatus, MovieId};

use super::{
    DEFAULT_TEMPLATE, Failure, ImportOptions, ImportRequest, Importer, Prepared, ReleaseMatcher,
    TransferMode, blocking, transfer::partial_path,
};
use crate::db::{
    LedgerEntry, LedgerState, SqliteImportLedger, SqliteLibraryRootStore, SqliteMediaFileStore,
    SqliteMovieStore,
};
use crate::health::{Checked, HealthChecker};
use crate::scan::{MediaFileStore, Prober};

/// Settings for [`MediaImportHandler`].
#[derive(Debug, Clone)]
pub struct HandlerOptions {
    /// Library root for new movies when there are several.
    pub default_root: Option<LibraryRootId>,
    /// Rename template; see [`super::naming::render`].
    pub template: String,
    pub mode: TransferMode,
    /// Videos smaller than this are treated as samples.
    pub min_video_bytes: u64,
    pub import_subtitles: bool,
}

impl Default for HandlerOptions {
    fn default() -> Self {
        Self {
            default_root: None,
            template: DEFAULT_TEMPLATE.into(),
            mode: TransferMode::default(),
            min_video_bytes: 50 * 1024 * 1024,
            import_subtitles: true,
        }
    }
}

/// Imports completed downloads into the library roots stored in the
/// database. Inject it into the job runner as `Arc<dyn ImportHandler>`.
pub struct MediaImportHandler<P, R> {
    importer: Importer<SqliteMovieStore, SqliteMediaFileStore, P, R>,
    roots: SqliteLibraryRootStore,
    ledger: SqliteImportLedger,
    health: HealthChecker,
    default_root: Option<LibraryRootId>,
}

impl<P: Prober, R: ReleaseMatcher> MediaImportHandler<P, R> {
    pub fn new(db: &Database, prober: P, matcher: R, options: HandlerOptions) -> Self {
        // The handler picks a root per import; these two fields are unused.
        let mut import = ImportOptions::new(PathBuf::new(), LibraryRootId(0));
        import.template = options.template;
        import.mode = options.mode;
        import.min_video_bytes = options.min_video_bytes;
        import.import_subtitles = options.import_subtitles;
        Self {
            importer: Importer::new(
                SqliteMovieStore::new(db),
                SqliteMediaFileStore::new(db),
                prober,
                matcher,
                import,
            ),
            roots: SqliteLibraryRootStore::new(db),
            ledger: SqliteImportLedger::new(db),
            health: HealthChecker::default(),
            default_root: options.default_root,
        }
    }

    /// Replaces the health checker, e.g. for a shorter timeout.
    pub fn with_health_checker(mut self, health: HealthChecker) -> Self {
        self.health = health;
        self
    }

    async fn run(&self, download: &CompletedDownload) -> Result<ImportResult, Failure> {
        let job = download.job_id.to_string();
        let mode = self.importer.options.mode;
        let entry = self.ledger.get(&job).await?;

        if let Some(entry) = &entry {
            if entry.state == LedgerState::Done
                && let Some(id) = entry.media_file_id
            {
                return Ok(ImportResult::Imported { media_file_id: id });
            }
            // A move that finished before the crash leaves no source for
            // `prepare` to find, so finish it from the ledger alone.
            if entry.mode == TransferMode::Move
                && !exists(Path::new(&entry.source_path)).await
                && size_of(Path::new(&entry.dest_path)).await == Some(entry.size_bytes)
            {
                let dest = PathBuf::from(&entry.dest_path);
                let root = self.root(entry.root_id).await?;
                self.check_health(&root).await?;
                let probe = if entry.kind.is_disc() {
                    None
                } else {
                    self.importer.prober.probe(&dest).await.ok()
                };
                let id = self
                    .importer
                    .record_file(entry.movie_id, entry.root_id, &dest, entry.kind, probe)
                    .await?;
                self.ledger.mark_done(&job, id).await?;
                return Ok(ImportResult::Imported { media_file_id: id });
            }
        }

        let request = ImportRequest {
            movie_id: download.movie_id,
            output_path: download.output_path.clone(),
            release_title: download.release_title.clone(),
            category: download.category.clone(),
        };
        let Some(prepared) = self.importer.prepare(&request).await? else {
            return Ok(ImportResult::NoMatch);
        };

        // Reuse the recorded plan for the same movie and source, so a retry
        // lands where the first attempt did.
        let planned = entry.filter(|e| {
            e.movie_id == prepared.movie.id && Path::new(&e.source_path) == prepared.source
        });
        let (root, dest) = match planned {
            Some(e) => (self.root(e.root_id).await?, PathBuf::from(e.dest_path)),
            None => {
                let root = self.choose_root(prepared.movie.id).await?;
                let dest = Path::new(&root.path).join(&prepared.rel_dest);
                (root, dest)
            }
        };
        self.check_health(&root).await?;

        let size_bytes = tokio::fs::metadata(&prepared.source)
            .await
            .map_err(Failure::from)?
            .len();
        self.ledger
            .put_pending(&LedgerEntry {
                job_id: job.clone(),
                state: LedgerState::Pending,
                movie_id: prepared.movie.id,
                root_id: root.id,
                kind: prepared.kind,
                source_path: prepared.source.to_string_lossy().into_owned(),
                dest_path: dest.to_string_lossy().into_owned(),
                mode,
                size_bytes,
                media_file_id: None,
            })
            .await?;

        self.place(&prepared, &dest, mode).await?;
        let id = self.importer.record(&prepared, root.id, &dest).await?;
        self.ledger.mark_done(&job, id).await?;
        Ok(ImportResult::Imported { media_file_id: id })
    }

    /// Gets the file to `dest`: transfers it, or accepts a destination our
    /// own earlier attempt already wrote.
    async fn place(
        &self,
        prepared: &Prepared,
        dest: &Path,
        mode: TransferMode,
    ) -> Result<(), Failure> {
        let partial = partial_path(dest);
        let _ = tokio::fs::remove_file(&partial).await;
        if tokio::fs::symlink_metadata(dest).await.is_err() {
            self.importer.transfer_into(prepared, dest).await?;
            return Ok(());
        }
        let (src, dst) = (prepared.source.clone(), dest.to_owned());
        if !blocking(move || is_ours(&src, &dst)).await? {
            return Err(Failure::permanent(format!(
                "a different file already exists at {}",
                dest.display()
            ))
            .with_code(FailureCode::DestinationConflict));
        }
        if mode == TransferMode::Move {
            // A cross-filesystem move that copied but didn't delete yet.
            let _ = tokio::fs::remove_file(&prepared.source).await;
        }
        self.importer.transfer_subtitles(prepared, dest).await;
        Ok(())
    }

    async fn root(&self, id: LibraryRootId) -> Result<LibraryRoot, Failure> {
        self.roots.get(id).await?.ok_or_else(|| {
            Failure::permanent(format!("library folder {id:?} no longer exists"))
                .with_code(FailureCode::NoDefaultRoot)
        })
    }

    async fn choose_root(&self, movie_id: MovieId) -> Result<LibraryRoot, Failure> {
        let roots: Vec<LibraryRoot> = self
            .roots
            .list()
            .await?
            .into_iter()
            .filter(|r| r.kind == LibraryRootKind::Library)
            .collect();
        if roots.is_empty() {
            return Err(Failure::permanent("no library folder is configured".into())
                .with_code(FailureCode::NoDefaultRoot));
        }
        let existing = self.importer.files.list_by_movie(movie_id).await?;
        if let Some(root) = existing
            .iter()
            .filter(|f| f.status == MediaFileStatus::Present)
            .find_map(|f| roots.iter().find(|r| r.id == f.root_id))
        {
            return Ok(root.clone());
        }
        if let Some(default) = self.default_root {
            return roots.into_iter().find(|r| r.id == default).ok_or_else(|| {
                Failure::permanent("the default import folder is not a library folder".into())
                    .with_code(FailureCode::NoDefaultRoot)
            });
        }
        if roots.len() == 1 {
            return Ok(roots.into_iter().next().expect("one root"));
        }
        Err(Failure::permanent(
            "several library folders and none is set as the import default".into(),
        )
        .with_code(FailureCode::NoDefaultRoot))
    }

    async fn check_health(&self, root: &LibraryRoot) -> Result<(), Failure> {
        let known = self
            .importer
            .files
            .list_by_root(root.id)
            .await?
            .iter()
            .filter(|f| f.status == MediaFileStatus::Present)
            .count();
        match self.health.check(root, known).await {
            Checked::Online => Ok(()),
            Checked::Offline(reason) | Checked::PermissionDenied(reason) => {
                Err(Failure::retryable(format!(
                    "library folder {} is unavailable: {reason}",
                    root.path
                ))
                .with_code(FailureCode::RootOffline))
            }
        }
    }
}

#[async_trait]
impl<P: Prober + 'static, R: ReleaseMatcher + 'static> ImportHandler for MediaImportHandler<P, R> {
    async fn import(&self, download: CompletedDownload) -> ImportResult {
        match self.run(&download).await {
            Ok(result) => result,
            Err(failure) => {
                tracing::warn!(
                    job = %download.job_id,
                    reason = %failure.reason,
                    retryable = failure.retryable,
                    "import failed"
                );
                ImportResult::Failed {
                    reason: failure.reason,
                    retryable: failure.retryable,
                    code: failure.code,
                }
            }
        }
    }
}

async fn exists(path: &Path) -> bool {
    tokio::fs::symlink_metadata(path).await.is_ok()
}

async fn size_of(path: &Path) -> Option<u64> {
    tokio::fs::metadata(path).await.ok().map(|m| m.len())
}

/// Whether `dest` is the transfer of `src`: the same inode (a hardlink), or
/// the same size and bytes (a finished copy).
fn is_ours(src: &Path, dest: &Path) -> std::io::Result<bool> {
    let (a, b) = (std::fs::metadata(src)?, std::fs::metadata(dest)?);
    if a.dev() == b.dev() && a.ino() == b.ino() {
        return Ok(true);
    }
    if a.len() != b.len() || !b.is_file() {
        return Ok(false);
    }
    let (mut fa, mut fb) = (std::fs::File::open(src)?, std::fs::File::open(dest)?);
    let (mut ba, mut bb) = (vec![0u8; 1 << 16], vec![0u8; 1 << 16]);
    loop {
        let n = read_full(&mut fa, &mut ba)?;
        if n != read_full(&mut fb, &mut bb)? || ba[..n] != bb[..n] {
            return Ok(false);
        }
        if n == 0 {
            return Ok(true);
        }
    }
}

fn read_full(file: &mut std::fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}
