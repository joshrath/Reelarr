//! Single-job NZB execution: download, verify, (repair), extract and publish.
//!
//! The worker drives one core [`Job`] through
//! `Queued → Downloading → Verifying → [Repairing →] Extracting → ReadyForImport`.
//! Import itself stays with core's [`reelarr_core::jobs::run_import`]; this module
//! supplies [`NzbCleanup`] for the cleanup step that follows a successful import.
//!
//! # On-disk layout
//!
//! Everything for a job lives in `staging_root/<job id>/`:
//!
//! * `files/<file index>/<yEnc name>`: one completed, CRC-verified file per NZB
//!   file. A file appears here only after [`Assembler::finish`] succeeded and was
//!   fsynced, and it is checkpointed in the [`NzbStore`] only after that move is
//!   durable. Files that are fully checkpointed *and* present are reused on
//!   resume, restart and re-queue; anything else for that index is discarded and
//!   downloaded again. No sidecar state file exists.
//! * `.reelarr-assembly-*`: the assembler's private in-progress directories,
//!   removed on drop and swept on the next run after a crash.
//! * `inputs/`: a flat view (hard links, or copies across filesystems) built for
//!   each extraction attempt, because RAR volume sets must share one directory.
//!
//! Publication builds `output_root/.reelarr-publish-<job id>/` and renames it to
//! its final name in one step, so the import side never sees a partial folder.
//!
//! # Integrity and repair
//!
//! yEnc part CRCs (and whole-file CRCs when the poster sent one) gate
//! verification: when every file assembled cleanly, parity is not consulted and
//! PAR2 recovery volumes are never downloaded. When something is missing or
//! damaged the [`ParityBackend`] decides. Damaged files are kept as zero-filled
//! partials under `partial/<index>/` so a later repair has their good blocks.
//! [`Par2Verify`] checks the downloaded files against the release's PAR2 index
//! and reports how many recovery blocks repair would need versus how many the
//! release posted. When repair is possible it downloads the needed recovery
//! volumes, reconstructs damaged files, and places them under `files/<index>/`.
//! Jobs that still cannot be repaired fail with [`FailureCode::RepairFailed`],
//! `retryable: false`. The default [`NoParity`] backend fails those jobs with a
//! message starting with [`REPAIR_UNAVAILABLE`]. Completed files stay on disk
//! so a later re-queue reuses them.
//!
//! # Pause, cancel and restart
//!
//! The controller changes the job state (to `Paused` or `Cancelled`) and then
//! fires the job's [`CancelToken`]. The worker never makes those transitions
//! itself; it stops at the next checkpoint and returns [`Outcome::Stopped`].
//! Nothing is published by a stopped run.
use crate::{
    JobId, NzbDocument, NzbFile, NzbStore, ParseError, ParseLimits, Segment, SegmentKey,
    StoreError,
    assembly::{Assembler, AssemblyError, PartialFile},
    nntp, parse,
    yenc::{BodyEncoding, DecodeLimits},
};
use reelarr_core::jobs::{
    CleanupError, CompletedDownload, DownloadSource, Failure, FailureCode, Job, JobKind, JobState,
    JobStore, JobStoreError, Progress, SourceCleanup,
};
use reelarr_types::MovieId;
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs,
    future::Future,
    io::{self, ErrorKind, Read},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use thiserror::Error;
use tokio::{sync::Notify, task::JoinSet};

/// Stable prefix of the failure message for damaged downloads that could not be
/// repaired because no repair backend is available yet. UIs and future re-queue
/// tooling can match on it.
pub const REPAIR_UNAVAILABLE: &str = "Repair unavailable";

const FILES_DIR: &str = "files";
const INPUTS_DIR: &str = "inputs";
const PARTIAL_DIR: &str = "partial";
const PAR2_VIEW: &str = ".reelarr-par2-view";
const ASSEMBLY_PREFIX: &str = ".reelarr-assembly-";
const RAR_PREFIX: &str = ".reelarr-rar-";
const PUBLISH_PREFIX: &str = ".reelarr-publish-";
const RAR_MAGIC: &[u8] = b"Rar!\x1a\x07";
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

/// Fetches raw article bodies. Implemented for [`nntp::Pool`]; tests and future
/// multi-server routing supply their own.
pub trait ArticleSource: Send + Sync + 'static {
    /// Return the complete dot-stuffed NNTP BODY for one segment.
    fn fetch(&self, segment: &Segment)
    -> impl Future<Output = Result<Vec<u8>, nntp::Error>> + Send;
}
impl ArticleSource for nntp::Pool {
    async fn fetch(&self, segment: &Segment) -> Result<Vec<u8>, nntp::Error> {
        self.body(segment).await.map(nntp::ArticleBody::into_bytes)
    }
}

/// Error from a [`RecoveryPaths`] implementation.
pub type RecoveryPathError = Box<dyn std::error::Error + Send + Sync>;

/// Records where a job's artifacts live, so startup recovery can tell whether
/// a processing stage can be re-entered. reelarr-db backs this with the job
/// row's recovery path; [`NoRecoveryPaths`] is a no-op for tests.
pub trait RecoveryPaths: Send + Sync {
    /// Set (or clear with `None`) the artifact path for a job.
    fn set_recovery_path(
        &self,
        job_id: JobId,
        path: Option<&Path>,
    ) -> impl Future<Output = Result<(), RecoveryPathError>> + Send;
}
/// Records nothing.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoRecoveryPaths;
impl RecoveryPaths for NoRecoveryPaths {
    async fn set_recovery_path(&self, _: JobId, _: Option<&Path>) -> Result<(), RecoveryPathError> {
        Ok(())
    }
}

/// What the worker knows about a damaged download when it asks for repair.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepairRequest {
    /// The job's staging directory (`files/` holds completed files).
    pub job_dir: PathBuf,
    /// NZB file indices that did not assemble cleanly.
    pub damaged_files: Vec<usize>,
    /// NZB file indices of PAR2 recovery volumes that were not downloaded.
    pub recovery_volumes: Vec<usize>,
    /// Articles missing or rejected in this run (zero when re-entered after a
    /// restart, where only file-level damage is known).
    pub bad_articles: u32,
    /// Damaged file indices kept as partials under `partial/<index>/`.
    pub partial_files: Vec<usize>,
    /// Recovery blocks the release posted, summed from volume names
    /// (`name.vol07+08.par2` holds 8). Not yet downloaded.
    pub posted_recovery_blocks: u64,
}

/// What a parity backend found when checking a damaged download.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParityVerdict {
    /// The backend can't verify.
    Unavailable,
    /// No usable PAR2 data among the downloaded files.
    NoParityData,
    /// PAR2 data lacks the description or checksums of some protected file.
    Incomplete,
    /// Every PAR2-protected file is intact; the damage is in unprotected files.
    Valid,
    /// Protected files need `needed` recovery blocks; `available` are on disk.
    Damaged {
        /// Recovery blocks needed.
        needed: u64,
        /// Recovery blocks already downloaded.
        available: u64,
    },
    /// The run was cancelled during verification.
    Cancelled,
    /// Verification itself failed (I/O or a hostile-input budget).
    Error {
        /// Diagnostic, logged and not shown to users.
        message: String,
    },
}

/// Result of a repair attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepairOutcome {
    /// Every damaged file now sits complete under `files/<index>/`.
    Repaired,
    /// Repair was attempted and could not succeed.
    Failed {
        /// User-facing reason.
        message: String,
    },
}

/// Pluggable PAR2 verification/repair (#74, #38).
pub trait ParityBackend: Send + Sync {
    /// Check the damaged download described by `request`. Reads only.
    fn verify(
        &self,
        request: &RepairRequest,
        cancel: &CancelToken,
    ) -> impl Future<Output = ParityVerdict> + Send;
    /// Whether this backend can repair; when false the job fails from
    /// `Verifying` without entering `Repairing`.
    fn can_repair(&self) -> bool;
    /// Repair the damaged files described by `request`.
    fn repair(
        &self,
        request: RepairRequest,
        cancel: &CancelToken,
    ) -> impl Future<Output = RepairOutcome> + Send;
}
/// No repair support yet: damaged downloads fail with [`REPAIR_UNAVAILABLE`].
#[derive(Clone, Copy, Debug, Default)]
pub struct NoParity;
impl ParityBackend for NoParity {
    async fn verify(&self, _: &RepairRequest, _: &CancelToken) -> ParityVerdict {
        ParityVerdict::Unavailable
    }
    fn can_repair(&self) -> bool {
        false
    }
    async fn repair(&self, _: RepairRequest, _: &CancelToken) -> RepairOutcome {
        RepairOutcome::Failed {
            message: "no repair backend".into(),
        }
    }
}

/// Clean-room Rust PAR2 verification and repair ([`reelarr_par2`]).
///
/// Builds a flat hard-linked view of `files/` and `partial/` inside the job
/// directory, verifies or repairs it, copies repaired files back to
/// `files/<index>/`, and removes the view again.
#[derive(Clone, Debug, Default)]
pub struct Par2Verify {
    /// Hostile-input budgets for the PAR2 parser and verifier.
    pub limits: reelarr_par2::Limits,
}
impl ParityBackend for Par2Verify {
    async fn verify(&self, request: &RepairRequest, cancel: &CancelToken) -> ParityVerdict {
        let (job_dir, limits, cancel) =
            (request.job_dir.clone(), self.limits.clone(), cancel.clone());
        tokio::task::spawn_blocking(move || par2_verify(&job_dir, &limits, cancel.flag()))
            .await
            .unwrap_or_else(|_| ParityVerdict::Error {
                message: "verification task failed".into(),
            })
    }
    fn can_repair(&self) -> bool {
        true
    }
    async fn repair(&self, request: RepairRequest, cancel: &CancelToken) -> RepairOutcome {
        let (job_dir, limits, cancel, damaged) = (
            request.job_dir.clone(),
            self.limits.clone(),
            cancel.clone(),
            request.damaged_files.clone(),
        );
        tokio::task::spawn_blocking(move || par2_repair(&job_dir, &limits, cancel.flag(), &damaged))
            .await
            .unwrap_or_else(|_| RepairOutcome::Failed {
                message: "repair task failed".into(),
            })
    }
}

fn build_par2_view(job_dir: &Path) -> io::Result<PathBuf> {
    let view = job_dir.join(PAR2_VIEW);
    remove_all(&view)?;
    create_private_dir(&view)?;
    let mut names = BTreeSet::new();
    // files/ first, then partial/. The view keeps the original basename so
    // verify_dir can match FileDesc names. If two slots share a basename,
    // the first wins (a completed file over a partial). Using index
    // prefixes would break that name match, and a damaged first 16 KiB
    // cannot fall back to the content hash either.
    for root in [FILES_DIR, PARTIAL_DIR] {
        let entries = match fs::read_dir(job_dir.join(root)) {
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            other => other?,
        };
        for slot in entries {
            let Some(file) = single_file(&slot?.path())? else {
                continue;
            };
            let name = file.file_name().expect("file has a name").to_owned();
            if names.insert(name.clone()) {
                link_or_copy(&file, &view.join(&name))?;
            }
        }
    }
    Ok(view)
}

fn par2_verify(
    job_dir: &Path,
    limits: &reelarr_par2::Limits,
    cancel: &AtomicBool,
) -> ParityVerdict {
    use reelarr_par2::Outcome as P;
    let view = job_dir.join(PAR2_VIEW);
    let result = (|| -> Result<ParityVerdict, reelarr_par2::Error> {
        let view = build_par2_view(job_dir)?;
        let report = reelarr_par2::verify_dir(&view, limits, cancel, &mut |_| {})?;
        Ok(match report.primary().map(|s| s.outcome) {
            None => ParityVerdict::NoParityData,
            Some(P::Valid) => ParityVerdict::Valid,
            Some(P::Incomplete) => ParityVerdict::Incomplete,
            Some(P::Repairable { needed, available } | P::Insufficient { needed, available }) => {
                ParityVerdict::Damaged { needed, available }
            }
        })
    })();
    let _ = remove_all(&view);
    match result {
        Ok(verdict) => verdict,
        Err(reelarr_par2::Error::Cancelled) => ParityVerdict::Cancelled,
        Err(e) => ParityVerdict::Error {
            message: e.to_string(),
        },
    }
}

fn par2_repair(
    job_dir: &Path,
    limits: &reelarr_par2::Limits,
    cancel: &AtomicBool,
    damaged: &[usize],
) -> RepairOutcome {
    use reelarr_par2::RepairStatus as S;
    let view = job_dir.join(PAR2_VIEW);
    let result = (|| -> Result<RepairOutcome, reelarr_par2::Error> {
        let view = build_par2_view(job_dir)?;
        let report = reelarr_par2::repair_dir(&view, limits, cancel, &mut |_| {})?;
        match report.status {
            S::AlreadyValid => Ok(RepairOutcome::Repaired),
            S::Repaired { files } => {
                publish_repaired(job_dir, &view, &files, damaged)?;
                Ok(RepairOutcome::Repaired)
            }
            S::Insufficient { needed, available } => Ok(RepairOutcome::Failed {
                message: format!(
                    "PAR2 repair needs {needed} recovery blocks but only {available} are available"
                ),
            }),
            S::Incomplete => Ok(RepairOutcome::Failed {
                message: "The release's PAR2 data is incomplete, so the files can't be repaired"
                    .into(),
            }),
            S::NoParityData => Ok(RepairOutcome::Failed {
                message: "The release has no usable PAR2 data to repair it".into(),
            }),
        }
    })();
    let _ = remove_all(&view);
    match result {
        Ok(outcome) => outcome,
        Err(reelarr_par2::Error::Cancelled) => RepairOutcome::Failed {
            message: "Repair was cancelled".into(),
        },
        Err(e) => RepairOutcome::Failed {
            message: e.to_string(),
        },
    }
}

fn publish_repaired(
    job_dir: &Path,
    view: &Path,
    files: &[String],
    damaged: &[usize],
) -> io::Result<()> {
    let mut used = HashSet::new();
    for name in files {
        let src = view.join(name);
        let index = match slot_for_basename(job_dir, name)? {
            Some(index) => index,
            None => damaged
                .iter()
                .copied()
                .find(|&i| {
                    !used.contains(&i)
                        && single_file(&job_dir.join(FILES_DIR).join(i.to_string()))
                            .ok()
                            .flatten()
                            .is_none()
                })
                .ok_or_else(|| {
                    io::Error::other(format!("repaired file {name} has no download slot"))
                })?,
        };
        publish_completed(&src, &job_dir.join(FILES_DIR).join(index.to_string()))?;
        let _ = remove_all(&job_dir.join(PARTIAL_DIR).join(index.to_string()));
        used.insert(index);
    }
    Ok(())
}

fn slot_for_basename(job_dir: &Path, name: &str) -> io::Result<Option<usize>> {
    for root in [FILES_DIR, PARTIAL_DIR] {
        let entries = match fs::read_dir(job_dir.join(root)) {
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            other => other?,
        };
        for entry in entries {
            let entry = entry?;
            let Some(file) = single_file(&entry.path())? else {
                continue;
            };
            if file.file_name().and_then(|n| n.to_str()) != Some(name) {
                continue;
            }
            if let Some(index) = entry.file_name().to_str().and_then(|n| n.parse().ok()) {
                return Ok(Some(index));
            }
        }
    }
    Ok(None)
}

/// Cooperative stop signal shared by the controller and one worker run.
#[derive(Clone, Debug, Default)]
pub struct CancelToken {
    inner: Arc<CancelInner>,
}
#[derive(Debug, Default)]
struct CancelInner {
    flag: AtomicBool,
    notify: Notify,
}
impl CancelToken {
    /// New, untriggered token.
    pub fn new() -> Self {
        Self::default()
    }
    /// Ask the worker to stop at its next checkpoint.
    pub fn cancel(&self) {
        self.inner.flag.store(true, Ordering::SeqCst);
        self.inner.notify.notify_waiters();
    }
    /// Whether [`Self::cancel`] was called.
    pub fn is_cancelled(&self) -> bool {
        self.inner.flag.load(Ordering::SeqCst)
    }
    fn flag(&self) -> &AtomicBool {
        &self.inner.flag
    }
    /// Resolve once cancelled.
    pub async fn cancelled(&self) {
        let notified = self.inner.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

/// Worker settings. `staging_root` and `output_root` must be Reelarr-owned
/// directories; nothing else should write into them.
#[derive(Clone, Debug)]
pub struct WorkerConfig {
    /// Parent of per-job staging directories.
    pub staging_root: PathBuf,
    /// Where finished downloads are published for import.
    pub output_root: PathBuf,
    /// Articles fetched concurrently per job (at least 1).
    pub max_concurrent_articles: usize,
    /// yEnc and assembly ceilings.
    pub decode_limits: DecodeLimits,
    /// RAR extraction ceilings.
    pub rar_limits: reelarr_rar::Limits,
    /// NZB parsing ceilings used by [`NzbWorker::enqueue`].
    pub parse_limits: ParseLimits,
}
impl WorkerConfig {
    /// Defaults with 8 concurrent articles.
    pub fn new(staging_root: impl Into<PathBuf>, output_root: impl Into<PathBuf>) -> Self {
        Self {
            staging_root: staging_root.into(),
            output_root: output_root.into(),
            max_concurrent_articles: 8,
            decode_limits: DecodeLimits::default(),
            rar_limits: reelarr_rar::Limits::default(),
            parse_limits: ParseLimits::default(),
        }
    }
}

/// How a run ended. Job failures are reported here (and in the job's state),
/// not as [`WorkerError`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Published and in `ReadyForImport`, carrying the `movie_id` given at
    /// enqueue; pass it to `run_import` as is.
    ReadyForImport(CompletedDownload),
    /// The job moved to `Failed` with this failure.
    Failed(Failure),
    /// Cancelled or paused by the controller; no state change was made.
    Stopped,
}

/// Infrastructure failures: the job store, NZB store or recovery-path store
/// failed, or the job was in a state this worker doesn't run.
#[derive(Debug, Error)]
pub enum WorkerError {
    /// Job storage failed.
    #[error(transparent)]
    Jobs(#[from] JobStoreError),
    /// NZB storage failed.
    #[error(transparent)]
    Nzb(#[from] StoreError),
    /// Recording the recovery path failed.
    #[error("recording the recovery path failed: {0}")]
    Recovery(RecoveryPathError),
    /// The job is not an NZB job in a runnable state.
    #[error("job is not runnable by the NZB worker (kind {kind:?}, state {state:?})")]
    NotRunnable {
        /// Job kind.
        kind: JobKind,
        /// Job state.
        state: JobState,
    },
    /// A blocking task panicked.
    #[error("worker task failed")]
    Join,
}

/// Why [`NzbWorker::enqueue`] did not create a job.
#[derive(Debug, Error)]
pub enum EnqueueError {
    /// The NZB is malformed or exceeds parsing limits; no job was created.
    #[error(transparent)]
    InvalidNzb(#[from] ParseError),
    /// Storage failed.
    #[error(transparent)]
    Worker(#[from] WorkerError),
}

/// Runs NZB jobs. One instance can serve many jobs; call [`Self::run`] once
/// per job at a time (the controller owns scheduling).
pub struct NzbWorker<S, A, P = NoParity, R = NoRecoveryPaths> {
    jobs: Arc<dyn JobStore>,
    nzbs: S,
    articles: Arc<A>,
    parity: P,
    recovery: R,
    config: WorkerConfig,
}

/// Per-file download result.
enum FileResult {
    Complete,
    Damaged { bad_articles: u32 },
}
/// Early exit from a stage.
enum Stop {
    Cancelled,
    Fail(Failure),
    Error(WorkerError),
}
impl From<WorkerError> for Stop {
    fn from(e: WorkerError) -> Self {
        Self::Error(e)
    }
}
impl From<JobStoreError> for Stop {
    fn from(e: JobStoreError) -> Self {
        Self::Error(e.into())
    }
}
impl From<io::Error> for Stop {
    fn from(e: io::Error) -> Self {
        Self::Fail(io_failure(&e))
    }
}
impl From<AssemblyError> for Stop {
    fn from(e: AssemblyError) -> Self {
        match e {
            AssemblyError::Io(e) => e.into(),
            AssemblyError::Store(e) => e.into(),
            AssemblyError::Metadata => Self::Fail(Failure {
                code: FailureCode::InvalidNzb,
                message: "The NZB's file and segment layout is inconsistent".into(),
                retryable: false,
            }),
            other => Self::Fail(Failure {
                code: FailureCode::Other,
                message: format!("Assembling a downloaded file failed: {other}"),
                retryable: true,
            }),
        }
    }
}
impl From<StoreError> for Stop {
    fn from(e: StoreError) -> Self {
        Self::Error(e.into())
    }
}

struct Damage {
    files: Vec<usize>,
    bad_articles: u32,
}

impl<S, A> NzbWorker<S, A>
where
    S: NzbStore,
    A: ArticleSource,
{
    /// Worker with no repair backend and no recovery-path recording.
    pub fn new(jobs: Arc<dyn JobStore>, nzbs: S, articles: A, config: WorkerConfig) -> Self {
        Self {
            jobs,
            nzbs,
            articles: Arc::new(articles),
            parity: NoParity,
            recovery: NoRecoveryPaths,
            config,
        }
    }
}

impl<S, A, P, R> NzbWorker<S, A, P, R>
where
    S: NzbStore,
    A: ArticleSource,
    P: ParityBackend,
    R: RecoveryPaths,
{
    /// Replace the repair backend.
    pub fn with_parity<P2: ParityBackend>(self, parity: P2) -> NzbWorker<S, A, P2, R> {
        NzbWorker {
            jobs: self.jobs,
            nzbs: self.nzbs,
            articles: self.articles,
            parity,
            recovery: self.recovery,
            config: self.config,
        }
    }
    /// Replace the recovery-path recorder.
    pub fn with_recovery_paths<R2: RecoveryPaths>(self, recovery: R2) -> NzbWorker<S, A, P, R2> {
        NzbWorker {
            jobs: self.jobs,
            nzbs: self.nzbs,
            articles: self.articles,
            parity: self.parity,
            recovery,
            config: self.config,
        }
    }
    /// Cleanup handler to register for [`DownloadSource::BuiltinNzb`].
    pub fn cleanup(&self) -> NzbCleanup {
        NzbCleanup {
            staging_root: self.config.staging_root.clone(),
            output_root: self.config.output_root.clone(),
        }
    }

    /// Parse and store an NZB and create its `Queued` job. The NZB's own title
    /// wins over `fallback_title`; `category` wins over the NZB's category.
    /// `movie_id` is stored with the NZB and returned on the completed download.
    ///
    /// Admission is ordered so a runnable job never exists without its NZB: the
    /// document is stored under the new job's ID first, and removed again if the
    /// job insert fails.
    pub async fn enqueue(
        &self,
        nzb: impl Read,
        fallback_title: &str,
        category: Option<String>,
        movie_id: Option<MovieId>,
    ) -> Result<Job, EnqueueError> {
        let document = parse(nzb, self.config.parse_limits)?;
        let title = document
            .title
            .clone()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| fallback_title.to_owned());
        let mut job = Job::new(JobKind::Nzb, title);
        job.category = category.or_else(|| document.category.clone());
        let id = job.id;
        self.nzbs
            .insert(id, document, movie_id)
            .await
            .map_err(WorkerError::from)?;
        match self.jobs.insert(job).await {
            Ok(job) => Ok(job),
            Err(e) => {
                if let Err(cleanup) = self.nzbs.remove(id).await {
                    tracing::warn!(job = %id, error = %cleanup, "could not remove NZB of a failed admission");
                }
                Err(WorkerError::from(e).into())
            }
        }
    }

    /// Drive one job as far as `ReadyForImport`. Re-enters `Verifying`,
    /// `Repairing` and `Extracting` after a restart using the files on disk.
    pub async fn run(&self, id: JobId, cancel: &CancelToken) -> Result<Outcome, WorkerError> {
        let job = self.jobs.get(id).await?;
        let not_runnable = WorkerError::NotRunnable {
            kind: job.kind,
            state: job.state,
        };
        if job.kind != JobKind::Nzb {
            return Err(not_runnable);
        }
        let stored = self.nzbs.load(id).await?.ok_or(StoreError::NotFound)?;
        let movie_id = stored.movie_id;
        let document = stored.document;
        let result = match job.state {
            JobState::Queued | JobState::Downloading => {
                self.stage_download(&job, &document, &stored.completed, cancel)
                    .await
            }
            JobState::Verifying | JobState::Repairing => {
                let checkpoints = stored.completed;
                self.stage_verify(&job, &document, &checkpoints, None, cancel)
                    .await
            }
            JobState::Extracting => self.stage_extract(&job, cancel).await,
            _ => return Err(not_runnable),
        };
        match result {
            Ok(download) => Ok(Outcome::ReadyForImport(CompletedDownload {
                movie_id,
                ..download
            })),
            Err(Stop::Cancelled) => Ok(Outcome::Stopped),
            Err(Stop::Error(e)) => Err(e),
            Err(Stop::Fail(failure)) => {
                match self
                    .advance(id, JobState::Failed, Some(failure.clone()))
                    .await
                {
                    Ok(()) => Ok(Outcome::Failed(failure)),
                    Err(Stop::Cancelled) => Ok(Outcome::Stopped),
                    Err(Stop::Error(e)) => Err(e),
                    Err(Stop::Fail(_)) => unreachable!("advance never fails a job"),
                }
            }
        }
    }

    /// Remove a job's staging directory and any unfinished publication, e.g.
    /// after the job is cancelled or deleted. Idempotent.
    pub async fn discard(&self, id: JobId) -> io::Result<()> {
        let staging = self.job_dir(id);
        let publish = self.publish_dir(id);
        blocking(move || {
            remove_all(&staging)?;
            remove_all(&publish)
        })
        .await
        .unwrap_or_else(|_| Err(io::Error::other("discard task failed")))
    }

    fn job_dir(&self, id: JobId) -> PathBuf {
        self.config.staging_root.join(id.to_string())
    }
    fn publish_dir(&self, id: JobId) -> PathBuf {
        self.config
            .output_root
            .join(format!("{PUBLISH_PREFIX}{id}"))
    }

    /// Transition, treating a controller pause/cancel race as a stop.
    async fn advance(&self, id: JobId, to: JobState, failure: Option<Failure>) -> Result<(), Stop> {
        match self.jobs.transition(id, to, failure).await {
            Ok(_) => Ok(()),
            Err(JobStoreError::Transition(e)) => {
                let job = self.jobs.get(id).await?;
                if matches!(job.state, JobState::Paused | JobState::Cancelled) {
                    Err(Stop::Cancelled)
                } else {
                    Err(Stop::Error(JobStoreError::Transition(e).into()))
                }
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn record_path(&self, id: JobId, path: Option<&Path>) -> Result<(), Stop> {
        self.recovery
            .set_recovery_path(id, path)
            .await
            .map_err(|e| Stop::Error(WorkerError::Recovery(e)))
    }

    async fn stage_download(
        &self,
        job: &Job,
        document: &NzbDocument,
        checkpoints: &BTreeSet<SegmentKey>,
        cancel: &CancelToken,
    ) -> Result<CompletedDownload, Stop> {
        if job.state == JobState::Queued {
            self.advance(job.id, JobState::Downloading, None).await?;
        }
        let job_dir = self.job_dir(job.id);
        let wanted: Vec<usize> = (0..document.files.len())
            .filter(|&i| !is_recovery_volume(&document.files[i].subject))
            .collect();
        let reusable = {
            let dir = job_dir.clone();
            let complete: Vec<usize> = wanted
                .iter()
                .copied()
                .filter(|&i| file_checkpointed(checkpoints, i, &document.files[i]))
                .collect();
            run_blocking(move || prepare_job_dir(&dir, &complete)).await?
        };
        // Clear checkpoints for anything that is not reusable on disk.
        let stale: Vec<(SegmentKey, bool)> = checkpoints
            .iter()
            .filter(|k| !reusable.contains(&k.file_index))
            .map(|k| (*k, false))
            .collect();
        if !stale.is_empty() {
            self.nzbs.checkpoint(job.id, &stale).await?;
        }
        self.record_path(job.id, Some(&job_dir)).await?;

        let bytes_total: u64 = wanted
            .iter()
            .flat_map(|&i| &document.files[i].segments)
            .map(|s| s.bytes)
            .sum();
        let mut meter = Meter {
            id: job.id,
            done: reusable
                .iter()
                .flat_map(|&i| &document.files[i].segments)
                .map(|s| s.bytes)
                .sum(),
            total: bytes_total,
            started: Instant::now(),
            started_at: 0,
            last: None,
            bad: 0,
        };
        meter.started_at = meter.done;
        let mut damage = Damage {
            files: Vec::new(),
            bad_articles: 0,
        };
        for &index in &wanted {
            if reusable.contains(&index) {
                continue;
            }
            if cancel.is_cancelled() {
                return Err(Stop::Cancelled);
            }
            match self
                .download_file(
                    job.id,
                    &job_dir,
                    index,
                    &document.files[index],
                    &mut meter,
                    cancel,
                )
                .await?
            {
                FileResult::Complete => {}
                FileResult::Damaged { bad_articles } => {
                    damage.files.push(index);
                    damage.bad_articles += bad_articles;
                }
            }
        }
        meter.flush(&*self.jobs).await?;
        if cancel.is_cancelled() {
            return Err(Stop::Cancelled);
        }
        self.advance(job.id, JobState::Verifying, None).await?;
        let checkpoints = self
            .nzbs
            .load(job.id)
            .await?
            .ok_or(StoreError::NotFound)?
            .completed;
        self.stage_verify(job, document, &checkpoints, Some(damage), cancel)
            .await
    }

    async fn download_file(
        &self,
        id: JobId,
        job_dir: &Path,
        index: usize,
        file: &NzbFile,
        meter: &mut Meter,
        cancel: &CancelToken,
    ) -> Result<FileResult, Stop> {
        let mut assembler = {
            let (dir, file, limits) = (job_dir.to_owned(), file.clone(), self.config.decode_limits);
            Some(run_blocking(move || Assembler::new(&dir, &file, limits)).await?)
        };
        let mut pending = file.segments.iter();
        let mut in_flight = JoinSet::new();
        let limit = self.config.max_concurrent_articles.max(1);
        let mut bad = 0u32;
        let mut fatal = None;
        loop {
            while fatal.is_none() && in_flight.len() < limit {
                let Some(segment) = pending.next() else { break };
                let (source, segment) = (self.articles.clone(), segment.clone());
                in_flight.spawn(async move {
                    let body = source.fetch(&segment).await;
                    (segment, body)
                });
            }
            let next = tokio::select! {
                biased;
                () = cancel.cancelled() => None,
                next = in_flight.join_next() => Some(next),
            };
            let Some(next) = next else {
                in_flight.abort_all();
                discard_assembler(assembler.take()).await;
                return Err(Stop::Cancelled);
            };
            let Some(joined) = next else { break };
            let (segment, body) = joined.map_err(|_| WorkerError::Join)?;
            let before = bad;
            match body {
                Ok(body) => {
                    let mut asm = assembler.take().expect("assembler present");
                    let number = segment.number;
                    let (asm, added) = blocking(move || {
                        let r = asm.add_part(number, body.as_slice(), BodyEncoding::DotStuffed);
                        (asm, r)
                    })
                    .await
                    .map_err(|_| WorkerError::Join)?;
                    assembler = Some(asm);
                    match added {
                        Ok(_) => {}
                        Err(AssemblyError::Io(e)) => fatal = Some(Stop::Fail(io_failure(&e))),
                        Err(AssemblyError::Store(e)) => return Err(e.into()),
                        Err(_) => bad += 1,
                    }
                }
                Err(nntp::Error::NotFound) => bad += 1,
                Err(e) => fatal = Some(nntp_stop(&e)),
            }
            if fatal.is_some() {
                in_flight.abort_all();
                break;
            }
            meter.bad += bad - before;
            meter.done += segment.bytes;
            meter.tick(&*self.jobs).await?;
        }
        if let Some(stop) = fatal {
            discard_assembler(assembler.take()).await;
            return Err(stop);
        }
        let asm = assembler.take().expect("assembler present");
        if bad > 0 {
            let dir = job_dir.to_owned();
            match blocking(move || stash_partial(asm, &dir, index)).await? {
                Ok(()) => {}
                Err(e) => {
                    tracing::warn!(job = %id, file = index, error = %e, "could not keep partial file")
                }
            }
            return Ok(FileResult::Damaged { bad_articles: bad });
        }
        let target = job_dir.join(FILES_DIR).join(index.to_string());
        let stale = job_dir.join(PARTIAL_DIR).join(index.to_string());
        let dir = job_dir.to_owned();
        let complete = run_blocking(move || -> Result<bool, Stop> {
            match asm.finish_or_partial() {
                Ok(Ok(completed)) => {
                    publish_completed(completed.path(), &target)?;
                    remove_all(&stale)?;
                    Ok(true)
                }
                Ok(Err(partial)) => {
                    if let Some(partial) = partial
                        && let Err(e) = stash_kept(partial, &dir, index)
                    {
                        tracing::warn!(
                            job = %id,
                            file = index,
                            error = %e,
                            "could not keep partial file after a CRC or coverage failure"
                        );
                    }
                    Ok(false)
                }
                Err(e @ (AssemblyError::Io(_) | AssemblyError::Store(_))) => Err(e.into()),
                Err(_) => Ok(false),
            }
        })
        .await?;
        if !complete {
            return Ok(FileResult::Damaged { bad_articles: 0 });
        }
        let updates: Vec<_> = file
            .segments
            .iter()
            .map(|s| {
                (
                    SegmentKey {
                        file_index: index,
                        number: s.number,
                    },
                    true,
                )
            })
            .collect();
        self.nzbs.checkpoint(id, &updates).await?;
        Ok(FileResult::Complete)
    }

    async fn download_recovery_volumes(
        &self,
        job: &Job,
        document: &NzbDocument,
        request: &RepairRequest,
        needed: u64,
        mut have: u64,
        cancel: &CancelToken,
    ) -> Result<u64, Stop> {
        let vol_bytes = request
            .recovery_volumes
            .iter()
            .flat_map(|&i| &document.files[i].segments)
            .map(|s| s.bytes)
            .sum();
        let mut meter = Meter {
            id: job.id,
            done: 0,
            total: vol_bytes,
            started: Instant::now(),
            started_at: 0,
            last: None,
            bad: 0,
        };
        let job_dir = self.job_dir(job.id);
        for &index in &request.recovery_volumes {
            if have >= needed {
                break;
            }
            if cancel.is_cancelled() {
                return Err(Stop::Cancelled);
            }
            match self
                .download_file(
                    job.id,
                    &job_dir,
                    index,
                    &document.files[index],
                    &mut meter,
                    cancel,
                )
                .await?
            {
                FileResult::Complete => {
                    have = have.saturating_add(
                        recovery_blocks(&document.files[index].subject).unwrap_or(0),
                    );
                }
                FileResult::Damaged { .. } => {}
            }
        }
        meter.flush(&*self.jobs).await?;
        Ok(have)
    }

    async fn stage_verify(
        &self,
        job: &Job,
        document: &NzbDocument,
        checkpoints: &BTreeSet<SegmentKey>,
        damage: Option<Damage>,
        cancel: &CancelToken,
    ) -> Result<CompletedDownload, Stop> {
        let job_dir = self.job_dir(job.id);
        let wanted: Vec<usize> = (0..document.files.len())
            .filter(|&i| !is_recovery_volume(&document.files[i].subject))
            .collect();
        let present = {
            let dir = job_dir.clone();
            let complete: Vec<usize> = wanted
                .iter()
                .copied()
                .filter(|&i| file_checkpointed(checkpoints, i, &document.files[i]))
                .collect();
            run_blocking(move || present_files(&dir, &complete)).await?
        };
        let damaged: Vec<usize> = wanted
            .iter()
            .copied()
            .filter(|i| !present.contains(i))
            .collect();
        let bad_articles = damage.map_or(0, |d| {
            debug_assert!(d.files.iter().all(|f| damaged.contains(f)));
            d.bad_articles
        });
        if !damaged.is_empty() {
            let partial_files = {
                let (dir, damaged) = (job_dir.clone(), damaged.clone());
                run_blocking(move || present_partials(&dir, &damaged)).await?
            };
            let request = RepairRequest {
                job_dir: job_dir.clone(),
                damaged_files: damaged,
                recovery_volumes: (0..document.files.len())
                    .filter(|&i| is_recovery_volume(&document.files[i].subject))
                    .collect(),
                bad_articles,
                partial_files,
                posted_recovery_blocks: document
                    .files
                    .iter()
                    .filter_map(|f| recovery_blocks(&f.subject))
                    .fold(0u64, u64::saturating_add),
            };
            let mut verdict = self.parity.verify(&request, cancel).await;
            let repairable = match &verdict {
                ParityVerdict::Cancelled => return Err(Stop::Cancelled),
                ParityVerdict::Error { message } => {
                    tracing::warn!(job = %job.id, error = %message, "PAR2 verification failed");
                    false
                }
                ParityVerdict::Damaged { needed, available } => {
                    *needed <= available.saturating_add(request.posted_recovery_blocks)
                }
                _ => false,
            };
            if !(repairable && self.parity.can_repair()) {
                let message = repair_message(&verdict, &request, wanted.len());
                return Err(Stop::Fail(Failure {
                    code: FailureCode::RepairFailed,
                    message,
                    retryable: false,
                }));
            }
            if let ParityVerdict::Damaged { needed, available } = &verdict
                && *needed > *available
            {
                self.download_recovery_volumes(
                    job, document, &request, *needed, *available, cancel,
                )
                .await?;
                verdict = self.parity.verify(&request, cancel).await;
                match &verdict {
                    ParityVerdict::Cancelled => return Err(Stop::Cancelled),
                    ParityVerdict::Damaged { needed, available } if *needed <= *available => {}
                    ParityVerdict::Error { message } => {
                        tracing::warn!(
                            job = %job.id,
                            error = %message,
                            "PAR2 verification failed after downloading recovery volumes"
                        );
                        return Err(Stop::Fail(Failure {
                            code: FailureCode::RepairFailed,
                            message: repair_message(&verdict, &request, wanted.len()),
                            retryable: false,
                        }));
                    }
                    _ => {
                        return Err(Stop::Fail(Failure {
                            code: FailureCode::RepairFailed,
                            message: repair_message(&verdict, &request, wanted.len()),
                            retryable: false,
                        }));
                    }
                }
            }
            if job.state != JobState::Repairing {
                self.advance(job.id, JobState::Repairing, None).await?;
            }
            match self.parity.repair(request, cancel).await {
                RepairOutcome::Repaired => {}
                RepairOutcome::Failed { message } => {
                    if cancel.is_cancelled() {
                        return Err(Stop::Cancelled);
                    }
                    return Err(Stop::Fail(Failure {
                        code: FailureCode::RepairFailed,
                        message,
                        retryable: false,
                    }));
                }
            }
        }
        if cancel.is_cancelled() {
            return Err(Stop::Cancelled);
        }
        self.advance(job.id, JobState::Extracting, None).await?;
        self.stage_extract(job, cancel).await
    }

    async fn stage_extract(
        &self,
        job: &Job,
        cancel: &CancelToken,
    ) -> Result<CompletedDownload, Stop> {
        let job_dir = self.job_dir(job.id);
        let publish = self.publish_dir(job.id);
        let output_root = self.config.output_root.clone();
        let title = job.title.clone();
        let limits = self.config.rar_limits.clone();
        let flag = cancel.clone();
        let id = job.id;
        let final_path = run_blocking(move || {
            extract_and_publish(&job_dir, &publish, &output_root, &title, id, limits, &flag)
        })
        .await?;
        self.record_path(job.id, Some(&final_path)).await?;
        if let Err(stop) = self.advance(job.id, JobState::ReadyForImport, None).await {
            // Undo publication so a stopped or failed run leaves nothing to import.
            let path = final_path.clone();
            let _ = blocking(move || remove_all(&path)).await;
            return Err(stop);
        }
        Ok(CompletedDownload {
            job_id: job.id,
            movie_id: None,
            output_path: final_path,
            category: job.category.clone(),
            source: DownloadSource::BuiltinNzb,
            release_title: job.title.clone(),
        })
    }
}

/// Removes a job's staging directory and published folder after import.
/// Register it for [`DownloadSource::BuiltinNzb`]. Idempotent, and it only
/// removes a published folder that sits directly inside `output_root`.
#[derive(Clone, Debug)]
pub struct NzbCleanup {
    staging_root: PathBuf,
    output_root: PathBuf,
}
impl NzbCleanup {
    /// Cleanup for the given roots (normally from [`NzbWorker::cleanup`]).
    pub fn new(staging_root: impl Into<PathBuf>, output_root: impl Into<PathBuf>) -> Self {
        Self {
            staging_root: staging_root.into(),
            output_root: output_root.into(),
        }
    }
}
#[async_trait::async_trait]
impl SourceCleanup for NzbCleanup {
    async fn on_imported(&self, job_id: JobId, output_path: &Path) -> Result<(), CleanupError> {
        let staging = self.staging_root.join(job_id.to_string());
        let root = self.output_root.clone();
        let output = output_path.to_owned();
        let result = blocking(move || -> io::Result<()> {
            remove_all(&staging)?;
            if output.exists() {
                let root = fs::canonicalize(&root)?;
                let parent = output
                    .parent()
                    .map(fs::canonicalize)
                    .transpose()?
                    .unwrap_or_default();
                if parent != root || output.file_name().is_none() {
                    return Err(io::Error::other(
                        "refusing to remove a folder outside the NZB output root",
                    ));
                }
                remove_all(&output)?;
            }
            Ok(())
        })
        .await
        .unwrap_or_else(|_| Err(io::Error::other("cleanup task failed")));
        result.map_err(|e| CleanupError(Box::new(e)))
    }
}

struct Meter {
    id: JobId,
    done: u64,
    total: u64,
    started: Instant,
    started_at: u64,
    last: Option<Instant>,
    bad: u32,
}
impl Meter {
    async fn tick(&mut self, jobs: &dyn JobStore) -> Result<(), JobStoreError> {
        if self.last.is_some_and(|t| t.elapsed() < PROGRESS_EVERY) {
            return Ok(());
        }
        self.flush(jobs).await
    }
    async fn flush(&mut self, jobs: &dyn JobStore) -> Result<(), JobStoreError> {
        self.last = Some(Instant::now());
        let elapsed = self.started.elapsed().as_secs_f64();
        let run_bytes = self.done.saturating_sub(self.started_at);
        let speed = if elapsed > 0.0 {
            (run_bytes as f64 / elapsed) as u64
        } else {
            0
        };
        let done = self.done.min(self.total);
        let eta = (speed > 0).then(|| (self.total - done) / speed);
        let progress = Progress {
            bytes_done: done,
            bytes_total: self.total,
            speed_bps: speed,
            eta_secs: eta,
            stage_fraction: None,
            articles_missing: self.bad,
        };
        match jobs.update_progress(self.id, progress).await {
            // A controller cancel can make the job terminal mid-run.
            Ok(_) | Err(JobStoreError::TerminalProgress) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, WorkerError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| WorkerError::Join)
}
async fn run_blocking<T: Send + 'static, E: Into<Stop> + Send + 'static>(
    f: impl FnOnce() -> Result<T, E> + Send + 'static,
) -> Result<T, Stop> {
    blocking(f).await?.map_err(Into::into)
}
async fn discard_assembler(assembler: Option<Assembler>) {
    if let Some(assembler) = assembler {
        // Drop removes partial output; that is blocking file I/O.
        let _ = blocking(move || drop(assembler)).await;
    }
}

fn io_failure(e: &io::Error) -> Failure {
    if e.kind() == ErrorKind::StorageFull {
        Failure {
            code: FailureCode::DiskFull,
            message: "Not enough disk space to finish this download".into(),
            retryable: true,
        }
    } else {
        Failure {
            code: FailureCode::Other,
            message: format!("A disk operation failed ({:?})", e.kind()),
            retryable: true,
        }
    }
}
fn nntp_stop(e: &nntp::Error) -> Stop {
    use nntp::Error as E;
    let (code, message, retryable) = match e {
        // Pool shutdown: leave the job as-is; restart recovery re-queues it.
        E::Closed => return Stop::Cancelled,
        E::Authentication { .. } => (
            FailureCode::ServerAuth,
            "The Usenet server rejected the login. Check the server's username and password"
                .to_owned(),
            true,
        ),
        E::Config(_) => (
            FailureCode::Other,
            format!("The Usenet server settings are invalid: {e}"),
            false,
        ),
        E::Tls | E::TrustRoots => (
            FailureCode::Other,
            "Couldn't make a trusted TLS connection to the Usenet server".to_owned(),
            true,
        ),
        _ => (
            FailureCode::Other,
            format!("The Usenet server is unavailable: {e}"),
            true,
        ),
    };
    Stop::Fail(Failure {
        code,
        message,
        retryable,
    })
}
fn rar_stop(e: reelarr_rar::Error) -> Stop {
    use reelarr_rar::Error as E;
    let failure = |code, message: String, retryable| {
        Stop::Fail(Failure {
            code,
            message,
            retryable,
        })
    };
    match e {
        E::Cancelled => Stop::Cancelled,
        E::Unsupported { reason } if reason.contains("encrypt") => failure(
            FailureCode::PasswordRequired,
            "This download's archive is encrypted. Password-protected archives aren't supported yet"
                .into(),
            false,
        ),
        E::Unsupported { reason } => failure(
            FailureCode::ExtractFailed,
            format!(
                "This download's archive uses an unsupported RAR feature ({reason}). \
                 Reelarr currently extracts only uncompressed (stored) RAR archives"
            ),
            false,
        ),
        E::Io(e) if e.kind() == ErrorKind::StorageFull => Stop::Fail(io_failure(&e)),
        E::Io(e) => failure(
            FailureCode::ExtractFailed,
            format!("Archive extraction hit a disk error ({:?})", e.kind()),
            true,
        ),
        other => failure(
            FailureCode::ExtractFailed,
            format!("Archive extraction failed: {other}"),
            false,
        ),
    }
}
fn extract_failure(message: &str) -> Stop {
    Stop::Fail(Failure {
        code: FailureCode::ExtractFailed,
        message: message.into(),
        retryable: false,
    })
}

fn repair_message(verdict: &ParityVerdict, request: &RepairRequest, total: usize) -> String {
    let articles = match request.bad_articles {
        0 => String::new(),
        1 => " (1 article missing or damaged)".into(),
        n => format!(" ({n} articles missing or damaged)"),
    };
    let damaged = request.damaged_files.len();
    let head =
        format!("{REPAIR_UNAVAILABLE}: {damaged} of {total} files are incomplete{articles}.");
    let kept = "Completed files are kept and reused on retry";
    let blocks = |n: u64| match n {
        1 => "1 recovery block".to_owned(),
        n => format!("{n} recovery blocks"),
    };
    match verdict {
        ParityVerdict::Damaged { needed, available } => {
            let have = available.saturating_add(request.posted_recovery_blocks);
            if *needed <= have {
                format!(
                    "{head} PAR2 says repair needs {} and the release has {have}, so it can be \
                     repaired once Reelarr supports repair. Retry this job then, or try another \
                     release. {kept}",
                    blocks(*needed)
                )
            } else {
                format!(
                    "{head} PAR2 says repair needs {} but the release only has {have}, so it \
                     can't be repaired. Try another release",
                    blocks(*needed)
                )
            }
        }
        ParityVerdict::NoParityData => format!(
            "{head} The release has no usable PAR2 data to check or repair it. Try another \
             release. {kept}"
        ),
        ParityVerdict::Incomplete => format!(
            "{head} The release's PAR2 data is incomplete, so the damage can't be measured. Try \
             another release. {kept}"
        ),
        ParityVerdict::Valid => format!(
            "{head} PAR2 reports the protected files intact, but Reelarr can't finish a release \
             with incomplete unprotected files yet. Try another release or retry this job. {kept}"
        ),
        ParityVerdict::Unavailable | ParityVerdict::Cancelled | ParityVerdict::Error { .. } => {
            format!(
                "{head} Reelarr can't repair downloads yet, so try another release or retry this \
                 job once repair support lands. {kept}"
            )
        }
    }
}

/// Keep a damaged file's validated bytes under `partial/<index>/` for repair.
fn stash_partial(asm: Assembler, job_dir: &Path, index: usize) -> io::Result<()> {
    match asm.keep_partial() {
        Ok(Some(partial)) => stash_kept(partial, job_dir, index),
        Ok(None) => Ok(()),
        Err(AssemblyError::Io(e)) => Err(e),
        Err(e) => Err(io::Error::other(e.to_string())),
    }
}

fn stash_kept(partial: PartialFile, job_dir: &Path, index: usize) -> io::Result<()> {
    let slot = job_dir.join(PARTIAL_DIR).join(index.to_string());
    remove_all(&slot)?;
    let private = partial.path.parent().expect("assembly dir").to_owned();
    let result = (|| {
        create_private_dir(&slot)?;
        let name = partial.path.file_name().expect("file has a name");
        fs::rename(&partial.path, slot.join(name))?;
        sync_dir(&slot)
    })();
    let _ = remove_all(&private);
    result
}

/// Damaged files that have a kept partial.
fn present_partials(dir: &Path, damaged: &[usize]) -> io::Result<Vec<usize>> {
    let mut found = Vec::new();
    for &i in damaged {
        if single_file(&dir.join(PARTIAL_DIR).join(i.to_string()))?.is_some() {
            found.push(i);
        }
    }
    Ok(found)
}

/// PAR2 recovery volumes (`name.vol07+08.par2`) are skipped during the initial
/// download and fetched later if verification says repair needs them. The
/// index `.par2` file is small and kept.
fn is_recovery_volume(subject: &str) -> bool {
    recovery_blocks(subject).is_some()
}
/// Recovery blocks a volume holds, from its name: `vol07+08` holds 8. The
/// rarer `vol07-08` spelling is recognised but counted as 0.
fn recovery_blocks(subject: &str) -> Option<u64> {
    let lower = subject.to_ascii_lowercase();
    lower.match_indices(".vol").find_map(|(i, _)| {
        let rest = &lower.as_bytes()[i + 4..];
        let digits = |b: &[u8]| b.iter().take_while(|c| c.is_ascii_digit()).count();
        let a = digits(rest);
        let sep = *rest.get(a)?;
        if a == 0 || !matches!(sep, b'+' | b'-') {
            return None;
        }
        let rest = &rest[a + 1..];
        let b = digits(rest);
        if b == 0 || !rest[b..].starts_with(b".par2") {
            return None;
        }
        if sep == b'-' {
            return Some(0);
        }
        std::str::from_utf8(&rest[..b])
            .ok()?
            .parse()
            .ok()
            .or(Some(u64::MAX))
    })
}

fn file_checkpointed(checkpoints: &BTreeSet<SegmentKey>, index: usize, file: &NzbFile) -> bool {
    file.segments.iter().all(|s| {
        checkpoints.contains(&SegmentKey {
            file_index: index,
            number: s.number,
        })
    })
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}
fn sync_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    fs::File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
fn remove_all(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
        Ok(m) if m.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
    }
}
/// The one non-empty regular file inside `files/<index>/`, if that is all it holds.
fn single_file(dir: &Path) -> io::Result<Option<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        other => other?,
    };
    let mut found = None;
    for entry in entries {
        let entry = entry?;
        // Never follow a symlink: only a real regular file can be reused.
        let meta = fs::symlink_metadata(entry.path())?;
        if found.is_some() || !meta.is_file() || meta.len() == 0 {
            return Ok(None);
        }
        found = Some(entry.path());
    }
    Ok(found)
}

/// Create the staging layout, sweep crash leftovers and return the file indices
/// whose completed output can be reused. Anything else in `files/` is removed.
fn prepare_job_dir(dir: &Path, complete: &[usize]) -> io::Result<BTreeSet<usize>> {
    let files = dir.join(FILES_DIR);
    create_private_dir(&files)?;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(ASSEMBLY_PREFIX)
            || name.starts_with(RAR_PREFIX)
            || name == INPUTS_DIR
            || name == PAR2_VIEW
        {
            remove_all(&entry.path())?;
        }
    }
    let mut reusable = BTreeSet::new();
    for entry in fs::read_dir(&files)? {
        let entry = entry?;
        let index = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<usize>().ok())
            .filter(|i| {
                complete.contains(i) && i.to_string() == entry.file_name().to_string_lossy()
            });
        match index {
            Some(i) if single_file(&entry.path())?.is_some() => {
                reusable.insert(i);
            }
            _ => remove_all(&entry.path())?,
        }
    }
    Ok(reusable)
}
fn present_files(dir: &Path, complete: &[usize]) -> io::Result<BTreeSet<usize>> {
    let mut present = BTreeSet::new();
    for &i in complete {
        if single_file(&dir.join(FILES_DIR).join(i.to_string()))?.is_some() {
            present.insert(i);
        }
    }
    Ok(present)
}
/// Move a finished assembly into `files/<index>/` durably.
fn publish_completed(source: &Path, target: &Path) -> io::Result<()> {
    remove_all(target)?;
    create_private_dir(target)?;
    let name = source
        .file_name()
        .ok_or_else(|| io::Error::other("assembled file has no name"))?;
    fs::rename(source, target.join(name))?;
    if let Some(assembly_dir) = source.parent() {
        let _ = fs::remove_dir(assembly_dir);
    }
    sync_dir(target)?;
    if let Some(files) = target.parent() {
        sync_dir(files)?;
        if let Some(job) = files.parent() {
            sync_dir(job)?;
        }
    }
    Ok(())
}

fn link_or_copy(source: &Path, target: &Path) -> io::Result<()> {
    if fs::hard_link(source, target).is_ok() {
        return Ok(());
    }
    fs::copy(source, target)?;
    fs::File::open(target)?.sync_all()
}
fn copy_tree(source: &Path, target: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(source)?;
    if meta.is_dir() {
        fs::create_dir(target)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_tree(&entry.path(), &target.join(entry.file_name()))?;
        }
        Ok(())
    } else if meta.is_file() {
        fs::copy(source, target)?;
        fs::File::open(target)?.sync_all()
    } else {
        Err(io::Error::other(
            "unexpected special file in extracted output",
        ))
    }
}
/// Rename, falling back to copy-then-delete across filesystems.
fn move_tree(source: &Path, target: &Path) -> io::Result<()> {
    match fs::rename(source, target) {
        Err(e) if e.kind() == ErrorKind::CrossesDevices => {
            copy_tree(source, target)?;
            remove_all(source)
        }
        other => other,
    }
}

/// Characters other than these become `_`; leading/trailing dots and spaces
/// are trimmed so the folder can't be hidden or confused with staging.
fn folder_name(title: &str) -> String {
    let mapped: String = title
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || " ._-()[]&,'+".contains(c) {
                c
            } else {
                '_'
            }
        })
        .take(150)
        .collect();
    let trimmed = mapped.trim_matches(|c| c == '.' || c == ' ');
    if trimmed.is_empty() {
        "download".into()
    } else {
        trimmed.into()
    }
}
fn final_path(root: &Path, title: &str, id: JobId) -> PathBuf {
    let base = folder_name(title);
    let id = id.to_string();
    [
        base.clone(),
        format!("{base} ({})", &id[..8]),
        format!("{base} ({id})"),
    ]
    .into_iter()
    .map(|n| root.join(n))
    .find(|p| fs::symlink_metadata(p).is_err())
    .unwrap_or_else(|| root.join(format!("{base} ({id}) {}", std::process::id())))
}

#[derive(Default)]
struct Plan {
    sets: Vec<Vec<PathBuf>>,
    plain: Vec<PathBuf>,
}
fn new_style_volume(name: &str) -> Option<(&str, u32)> {
    let stem = name.strip_suffix(".rar")?;
    let (base, digits) = stem.rsplit_once(".part")?;
    (!base.is_empty() && !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse().ok().map(|n| (base, n)))
        .flatten()
}
fn old_style_continuation(name: &str) -> Option<(&str, u32)> {
    let (base, ext) = name.rsplit_once('.')?;
    let b = ext.as_bytes();
    if base.is_empty() || b.len() != 3 || !(b'r'..=b'z').contains(&b[0]) {
        return None;
    }
    let n: u32 = ext[1..]
        .parse()
        .ok()
        .filter(|_| b[1..].iter().all(u8::is_ascii_digit))?;
    Some((base, u32::from(b[0] - b'r') * 100 + n + 1))
}
fn has_rar_magic(path: &Path) -> io::Result<bool> {
    let mut head = [0; RAR_MAGIC.len()];
    let mut file = fs::File::open(path)?;
    let mut read = 0;
    while read < head.len() {
        match file.read(&mut head[read..])? {
            0 => return Ok(false),
            n => read += n,
        }
    }
    Ok(head == RAR_MAGIC)
}
/// Group a flat input directory into RAR volume sets and plain payload files.
fn plan_inputs(inputs: &Path) -> Result<Plan, Stop> {
    let mut names: Vec<String> = Vec::new();
    for entry in fs::read_dir(inputs)? {
        let name = entry?.file_name();
        names.push(
            name.into_string()
                .map_err(|_| extract_failure("A downloaded file name is not valid UTF-8"))?,
        );
    }
    names.sort();
    let mut new_sets: BTreeMap<String, BTreeMap<u32, PathBuf>> = BTreeMap::new();
    let mut old_sets: BTreeMap<String, BTreeMap<u32, PathBuf>> = BTreeMap::new();
    let mut maybe_old = Vec::new();
    let mut plan = Plan::default();
    for name in &names {
        let lower = name.to_ascii_lowercase();
        if [".par2", ".sfv", ".nzb", ".srr"]
            .iter()
            .any(|e| lower.ends_with(e))
        {
            continue;
        }
        let path = inputs.join(name);
        if let Some((base, n)) = new_style_volume(name) {
            new_sets.entry(base.into()).or_default().insert(n, path);
        } else if let Some(base) = name.strip_suffix(".rar") {
            old_sets.entry(base.into()).or_default().insert(0, path);
        } else if let Some((base, n)) = old_style_continuation(name) {
            maybe_old.push((base.to_owned(), n, path));
        } else {
            plan.plain.push(path);
        }
    }
    for (base, n, path) in maybe_old {
        match old_sets.get_mut(&base) {
            Some(set) => {
                set.insert(n, path);
            }
            None => plan.plain.push(path),
        }
    }
    for path in &plan.plain {
        if has_rar_magic(path)? {
            return Err(extract_failure(
                "This download contains a RAR volume with an unrecognized or obfuscated name, \
                 which isn't supported yet",
            ));
        }
    }
    plan.sets = new_sets
        .into_values()
        .chain(old_sets.into_values())
        .map(|set| set.into_values().collect())
        .collect();
    Ok(plan)
}

/// Claim a top-level output name, case-insensitively.
fn claim(seen: &mut HashSet<String>, name: &std::ffi::OsStr) -> Result<(), Stop> {
    let key = name.to_string_lossy().to_lowercase();
    if seen.insert(key) {
        Ok(())
    } else {
        Err(extract_failure(
            "Two files in this download would have the same name",
        ))
    }
}

fn extract_and_publish(
    job_dir: &Path,
    publish: &Path,
    output_root: &Path,
    title: &str,
    id: JobId,
    limits: reelarr_rar::Limits,
    cancel: &CancelToken,
) -> Result<PathBuf, Stop> {
    // Sweep leftovers from an interrupted attempt.
    for entry in fs::read_dir(job_dir)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with(RAR_PREFIX) {
            remove_all(&entry.path())?;
        }
    }
    let inputs = job_dir.join(INPUTS_DIR);
    remove_all(&inputs)?;
    remove_all(publish)?;
    create_private_dir(&inputs)?;
    let mut seen = HashSet::new();
    let mut indices: Vec<usize> = fs::read_dir(job_dir.join(FILES_DIR))?
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
        .collect();
    indices.sort_unstable();
    for index in indices {
        let dir = job_dir.join(FILES_DIR).join(index.to_string());
        let Some(file) = single_file(&dir)? else {
            continue;
        };
        let name = file.file_name().expect("file has a name");
        claim(&mut seen, name)?;
        link_or_copy(&file, &inputs.join(name))?;
    }
    let plan = plan_inputs(&inputs)?;

    fs::create_dir_all(output_root)?;
    create_private_dir(publish)?;
    let result = (|| -> Result<(), Stop> {
        let mut published = HashSet::new();
        for set in plan.sets {
            if cancel.is_cancelled() {
                return Err(Stop::Cancelled);
            }
            let extraction = reelarr_rar::VolumeSet::open(&set, limits.clone())
                .and_then(|v| v.extract(job_dir, cancel.flag(), |_| {}))
                .map_err(rar_stop)?;
            let dir = extraction.keep();
            for entry in fs::read_dir(&dir)? {
                let entry = entry?;
                claim(&mut published, &entry.file_name())?;
                move_tree(&entry.path(), &publish.join(entry.file_name()))?;
            }
            remove_all(&dir)?;
        }
        for path in plan.plain {
            let name = path.file_name().expect("file has a name");
            claim(&mut published, name)?;
            link_or_copy(&path, &publish.join(name))?;
        }
        if published.is_empty() {
            return Err(extract_failure(
                "This download contained nothing to import (only repair or checksum files)",
            ));
        }
        if cancel.is_cancelled() {
            return Err(Stop::Cancelled);
        }
        sync_dir(publish)?;
        Ok(())
    })();
    if let Err(stop) = result {
        let _ = remove_all(publish);
        let _ = remove_all(&inputs);
        return Err(stop);
    }
    let target = final_path(output_root, title, id);
    if let Err(e) = fs::rename(publish, &target) {
        // Don't leave a half-published folder in the output root.
        let _ = remove_all(publish);
        let _ = remove_all(&inputs);
        return Err(e.into());
    }
    sync_dir(output_root)?;
    let _ = remove_all(&inputs);
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn single_file_ignores_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside.mkv");
        fs::write(&outside, b"data").unwrap();
        let slot = dir.path().join("files/0");
        fs::create_dir_all(&slot).unwrap();
        std::os::unix::fs::symlink(&outside, slot.join("Movie.mkv")).unwrap();
        assert_eq!(single_file(&slot).unwrap(), None);
        fs::remove_file(slot.join("Movie.mkv")).unwrap();
        fs::write(slot.join("Movie.mkv"), b"data").unwrap();
        assert_eq!(single_file(&slot).unwrap(), Some(slot.join("Movie.mkv")));
    }

    #[test]
    fn recovery_volume_names() {
        for s in [
            "\"Movie.vol00+01.par2\" yEnc (1/3)",
            "x.VOL127+128.PAR2",
            "a.vol3-4.par2",
        ] {
            assert!(is_recovery_volume(s), "{s}");
        }
        for s in [
            "\"Movie.par2\" yEnc (1/1)",
            "volume.vol.par2",
            "a.vol+1.par2",
            "a.vol1+.par2",
            "a.vol1+2.rar",
            ".vol",
        ] {
            assert!(!is_recovery_volume(s), "{s}");
        }
    }
    #[test]
    fn folder_names_are_safe() {
        assert_eq!(folder_name("Movie (2024) 1080p"), "Movie (2024) 1080p");
        assert_eq!(folder_name("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(folder_name("..."), "download");
        assert_eq!(folder_name(".reelarr-publish-x"), "reelarr-publish-x");
        assert_eq!(folder_name("a\\b:c*d\0"), "a_b_c_d_");
        assert_eq!(folder_name(&"é".repeat(400)).chars().count(), 150);
    }
    #[test]
    fn rar_volume_names() {
        assert_eq!(new_style_volume("a.part01.rar"), Some(("a", 1)));
        assert_eq!(new_style_volume("a.b.part10.rar"), Some(("a.b", 10)));
        assert_eq!(new_style_volume("a.partx.rar"), None);
        assert_eq!(new_style_volume(".part1.rar"), None);
        assert_eq!(old_style_continuation("a.r00"), Some(("a", 1)));
        assert_eq!(old_style_continuation("a.s05"), Some(("a", 106)));
        assert_eq!(old_style_continuation("a.q00"), None);
        assert_eq!(old_style_continuation("a.r0x"), None);
        assert_eq!(old_style_continuation("a.r+1"), None);
    }
    fn request(bad_articles: u32, damaged: usize, posted: u64) -> RepairRequest {
        RepairRequest {
            job_dir: PathBuf::new(),
            damaged_files: (0..damaged).collect(),
            recovery_volumes: Vec::new(),
            bad_articles,
            partial_files: Vec::new(),
            posted_recovery_blocks: posted,
        }
    }
    #[test]
    fn repair_message_is_actionable() {
        let m = repair_message(
            &ParityVerdict::Damaged {
                needed: 2,
                available: 0,
            },
            &request(3, 1, 4),
            4,
        );
        assert!(m.starts_with("Repair unavailable: 1 of 4 files are incomplete (3 articles"));
        assert!(m.contains("needs 2 recovery blocks and the release has 4"));
        let insufficient = repair_message(
            &ParityVerdict::Damaged {
                needed: 5,
                available: 0,
            },
            &request(0, 2, 1),
            2,
        );
        assert!(insufficient.contains("2 of 2 files are incomplete. "));
        assert!(insufficient.contains("needs 5 recovery blocks but the release only has 1"));
        let none = repair_message(&ParityVerdict::Unavailable, &request(0, 1, 0), 1);
        assert!(none.contains("can't repair downloads yet"));
    }
    #[test]
    fn recovery_volume_block_counts() {
        assert_eq!(recovery_blocks("Movie.vol07+08.par2"), Some(8));
        assert_eq!(recovery_blocks("Movie.vol00+01.par2"), Some(1));
        assert_eq!(recovery_blocks("Movie.vol07-08.par2"), Some(0));
        assert_eq!(recovery_blocks("Movie.par2"), None);
    }
    #[tokio::test]
    async fn cancel_token_wakes_waiters() {
        let token = CancelToken::new();
        let waiter = {
            let t = token.clone();
            tokio::spawn(async move { t.cancelled().await })
        };
        tokio::task::yield_now().await;
        token.cancel();
        waiter.await.unwrap();
        token.cancelled().await;
        assert!(token.is_cancelled());
    }
}
