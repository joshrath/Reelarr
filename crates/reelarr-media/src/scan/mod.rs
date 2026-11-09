//! Library scanning.
//!
//! [`Scanner::scan_root`] walks a configured root (local disk, network share
//! or manual-import folder), probes new or changed video files with ffprobe
//! and records them in a [`MediaFileStore`]. Files are only re-probed when
//! their size or modification time changes. Files that disappear are marked
//! [`MediaFileStatus::Missing`], never deleted.
//!
//! Network shares do not deliver reliable change notifications, so rescans
//! are polled on a timer per root (see [`run_periodic`]).

pub mod disc;
mod probe;
mod store;

pub use probe::{FfprobeProber, ProbeError, Prober, parse_ffprobe_json};
pub use store::{InMemoryMediaFileStore, MediaFileStore, NewMediaFile};

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::stream::{self, StreamExt};
use reelarr_types::{
    LibraryRoot, LibraryRootId, MediaFile, MediaFileStatus, MediaKind, MediaProbe,
};
use tokio::sync::watch;

use crate::store::StoreError;

/// File extensions treated as video.
pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mkv", "mp4", "m4v", "webm", "avi", "mov", "ts", "m2ts", "wmv", "mpg", "mpeg",
];

/// Suffix of partially copied import files, which scans ignore.
pub const PARTIAL_SUFFIX: &str = ".reelarr-partial";

/// Errors that stop a scan of a whole root.
#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    /// The root is missing, unmounted or not a directory. Nothing is marked
    /// missing.
    #[error("library root {path} is unavailable: {reason}")]
    RootUnavailable { path: String, reason: String },
    /// The root is reachable but contains no video files while the library
    /// knows files in it. This usually means a share's mount point is present
    /// but the share is not mounted, so nothing is marked missing.
    #[error(
        "library root {path} looks empty but has {known} known files; skipping to avoid marking them missing"
    )]
    RootLooksUnmounted { path: String, known: usize },
    #[error("a scan of this root is already running")]
    AlreadyRunning,
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Outcome of scanning one root.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ScanReport {
    pub added: usize,
    pub updated: usize,
    pub unchanged: usize,
    /// Files that had been missing and are back with the same size and time.
    pub restored: usize,
    pub marked_missing: usize,
    /// Files that could not be probed. They are stored without probe data and
    /// retried on the next scan.
    pub probe_failures: Vec<(String, String)>,
    /// Entries that could not be read while walking (permissions, I/O).
    pub walk_errors: Vec<String>,
}

/// Scanner settings.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// How many ffprobe processes may run at once. Keep this low for network
    /// shares.
    pub probe_concurrency: usize,
    /// Longest a whole directory walk may take before the root is reported
    /// unavailable. Guards against a share that hangs mid-scan.
    pub walk_timeout: Duration,
    /// Limits for summing a disc folder's size and for reading an `.iso`
    /// signature. A disc that exceeds them keeps its existing entry.
    pub disc_max_entries: usize,
    pub disc_timeout: Duration,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            probe_concurrency: 2,
            walk_timeout: Duration::from_secs(30 * 60),
            disc_max_entries: 20_000,
            disc_timeout: Duration::from_secs(60),
        }
    }
}

/// Scans library roots into a [`MediaFileStore`].
#[derive(Debug)]
pub struct Scanner<P, S> {
    prober: P,
    store: S,
    options: ScanOptions,
    running: Mutex<HashSet<LibraryRootId>>,
}

#[derive(Debug, Clone)]
struct FoundFile {
    path: PathBuf,
    size_bytes: u64,
    modified_at: i64,
    candidate: Candidate,
}

/// What the walk found, before probing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Candidate {
    Video,
    /// An `.iso`; its signature decides between disc image and unrecognized.
    Iso,
    DiscFolder,
}

impl Candidate {
    /// Whether a stored entry of `kind` is the same sort of thing.
    fn matches(self, kind: MediaKind) -> bool {
        match self {
            Candidate::Video => kind == MediaKind::File,
            Candidate::Iso => {
                matches!(kind, MediaKind::DiscImage | MediaKind::UnrecognizedImage)
            }
            Candidate::DiscFolder => kind == MediaKind::DiscFolder,
        }
    }
}

/// Result of a directory walk.
#[derive(Debug, Default)]
struct Walked {
    found: Vec<FoundFile>,
    errors: Vec<String>,
    /// Paths seen but not readable this time (e.g. a disc folder that hit a
    /// limit). Their existing entries are left as they are.
    keep: Vec<String>,
}

impl<P: Prober, S: MediaFileStore> Scanner<P, S> {
    pub fn new(prober: P, store: S, options: ScanOptions) -> Self {
        Self {
            prober,
            store,
            options,
            running: Mutex::new(HashSet::new()),
        }
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    pub fn prober(&self) -> &P {
        &self.prober
    }

    /// Scan one root. Only one scan per root runs at a time.
    pub async fn scan_root(&self, root: &LibraryRoot) -> Result<ScanReport, ScanError> {
        let _guard = RunGuard::acquire(&self.running, root.id)?;
        self.scan_root_inner(root).await
    }

    async fn scan_root_inner(&self, root: &LibraryRoot) -> Result<ScanReport, ScanError> {
        let root_path = PathBuf::from(&root.path);
        match tokio::fs::metadata(&root_path).await {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Err(ScanError::RootUnavailable {
                    path: root.path.clone(),
                    reason: "not a directory".into(),
                });
            }
            Err(e) => {
                return Err(ScanError::RootUnavailable {
                    path: root.path.clone(),
                    reason: e.to_string(),
                });
            }
        }

        let walk_root = root_path.clone();
        let limits = (self.options.disc_max_entries, self.options.disc_timeout);
        let walk_task = tokio::task::spawn_blocking(move || walk(&walk_root, limits));
        let started = Instant::now();
        let Walked {
            found,
            errors: walk_errors,
            keep,
        } = match tokio::time::timeout(self.options.walk_timeout, walk_task).await {
            Ok(Ok(walked)) => walked,
            Ok(Err(e)) => {
                return Err(ScanError::RootUnavailable {
                    path: root.path.clone(),
                    reason: format!("walk task failed: {e}"),
                });
            }
            Err(_) => {
                return Err(ScanError::RootUnavailable {
                    path: root.path.clone(),
                    reason: format!(
                        "listing the folder took longer than {}s; the share may be hung",
                        self.options.walk_timeout.as_secs()
                    ),
                });
            }
        };
        tracing::debug!(root = %root.path, elapsed_ms = started.elapsed().as_millis() as u64, entries = found.len(), "library walk finished");

        let known = self.store.list_by_root(root.id).await?;
        let known_present = known
            .iter()
            .filter(|f| f.status == MediaFileStatus::Present)
            .count();
        if found.is_empty() && known_present > 0 {
            return Err(ScanError::RootLooksUnmounted {
                path: root.path.clone(),
                known: known_present,
            });
        }

        let mut report = ScanReport {
            walk_errors,
            ..ScanReport::default()
        };
        let mut known_by_path: HashMap<String, MediaFile> =
            known.into_iter().map(|f| (f.path.clone(), f)).collect();
        for path in &keep {
            known_by_path.remove(path);
        }

        // Decide what needs probing.
        let mut to_probe: Vec<(FoundFile, Option<MediaFile>)> = Vec::new();
        for file in found {
            let key = file.path.to_string_lossy().into_owned();
            match known_by_path.remove(&key) {
                Some(existing)
                    if existing.size_bytes == file.size_bytes
                        && existing.modified_at == file.modified_at
                        && file.candidate.matches(existing.kind)
                        && (existing.probe.is_some() || existing.kind.is_disc()) =>
                {
                    if existing.status != MediaFileStatus::Present {
                        let mut restored = existing;
                        restored.status = MediaFileStatus::Present;
                        self.store.update(&restored).await?;
                        report.restored += 1;
                    } else {
                        report.unchanged += 1;
                    }
                }
                other => to_probe.push((file, other)),
            }
        }

        // Probe with bounded concurrency.
        let prober = &self.prober;
        let disc_timeout = self.options.disc_timeout;
        let probed: Vec<_> = stream::iter(to_probe)
            .map(|(file, existing)| async move {
                let result = identify(prober, &file, disc_timeout).await;
                (file, existing, result)
            })
            .buffer_unordered(self.options.probe_concurrency.max(1))
            .collect()
            .await;

        for (file, existing, result) in probed {
            let path = file.path.to_string_lossy().into_owned();
            let (kind, probe): (MediaKind, Option<MediaProbe>) = match result {
                Ok(identified) => identified,
                Err(e) if file.candidate == Candidate::Iso => {
                    // Couldn't read the image header; leave any existing
                    // entry alone and retry next scan.
                    tracing::warn!(path = %path, error = %e, "disc image check failed");
                    report.probe_failures.push((path, e.to_string()));
                    continue;
                }
                Err(e) => {
                    tracing::warn!(path = %path, error = %e, "probe failed");
                    report.probe_failures.push((path.clone(), e.to_string()));
                    (MediaKind::File, None)
                }
            };
            match existing {
                Some(mut existing) => {
                    existing.size_bytes = file.size_bytes;
                    existing.modified_at = file.modified_at;
                    existing.status = MediaFileStatus::Present;
                    existing.probe = probe;
                    existing.kind = kind;
                    self.store.update(&existing).await?;
                    report.updated += 1;
                }
                None => {
                    self.store
                        .insert(NewMediaFile {
                            movie_id: None,
                            root_id: root.id,
                            path,
                            size_bytes: file.size_bytes,
                            modified_at: file.modified_at,
                            probe,
                            kind,
                        })
                        .await?;
                    report.added += 1;
                }
            }
        }

        // Anything left was not found on disk.
        let now = unix_now();
        for (_, mut gone) in known_by_path {
            if gone.status == MediaFileStatus::Present {
                gone.status = MediaFileStatus::Missing { since: now };
                self.store.update(&gone).await?;
                report.marked_missing += 1;
            }
        }

        tracing::info!(
            root = %root.path,
            added = report.added,
            updated = report.updated,
            unchanged = report.unchanged,
            restored = report.restored,
            missing = report.marked_missing,
            probe_failures = report.probe_failures.len(),
            "library scan finished"
        );
        Ok(report)
    }
}

/// Rescan `root` on its configured interval until `shutdown` turns true.
///
/// Runs one scan immediately. Returns right away if the root has periodic
/// rescans disabled. Scan errors are logged and the loop keeps going, so a
/// share that is offline is picked up again once it is back.
pub async fn run_periodic<P: Prober, S: MediaFileStore>(
    scanner: &Scanner<P, S>,
    root: &LibraryRoot,
    mut shutdown: watch::Receiver<bool>,
) {
    let Some(secs) = root.rescan_interval_secs.filter(|s| *s > 0) else {
        return;
    };
    let interval = Duration::from_secs(secs);
    loop {
        if *shutdown.borrow() {
            return;
        }
        if let Err(e) = scanner.scan_root(root).await {
            tracing::warn!(root = %root.path, error = %e, "periodic scan failed");
        }
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return;
                }
            }
        }
    }
}

struct RunGuard<'a> {
    running: &'a Mutex<HashSet<LibraryRootId>>,
    id: LibraryRootId,
}

impl<'a> RunGuard<'a> {
    fn acquire(
        running: &'a Mutex<HashSet<LibraryRootId>>,
        id: LibraryRootId,
    ) -> Result<Self, ScanError> {
        let mut set = running
            .lock()
            .map_err(|_| StoreError::Backend("scanner lock poisoned".into()))?;
        if !set.insert(id) {
            return Err(ScanError::AlreadyRunning);
        }
        Ok(Self { running, id })
    }
}

impl Drop for RunGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut set) = self.running.lock() {
            set.remove(&self.id);
        }
    }
}

/// Whether a file name is one the scanner may consider at all: not hidden
/// and not a partial import.
fn is_scannable_name(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    !(name.starts_with('.') || name.ends_with(PARTIAL_SUFFIX))
}

/// Returns true for files the scanner should consider.
pub fn is_video_file(path: &Path) -> bool {
    if !is_scannable_name(path) {
        return false;
    }
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| VIDEO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Probes a video, or classifies a disc rip without probing it.
async fn identify<P: Prober>(
    prober: &P,
    file: &FoundFile,
    disc_timeout: Duration,
) -> Result<(MediaKind, Option<MediaProbe>), ProbeError> {
    match file.candidate {
        Candidate::Video => Ok((MediaKind::File, Some(prober.probe(&file.path).await?))),
        Candidate::DiscFolder => Ok((MediaKind::DiscFolder, None)),
        Candidate::Iso => {
            let path = file.path.clone();
            let check = tokio::task::spawn_blocking(move || disc::has_disc_signature(&path));
            match tokio::time::timeout(disc_timeout, check).await {
                Ok(Ok(Ok(true))) => Ok((MediaKind::DiscImage, None)),
                Ok(Ok(Ok(false))) => Ok((MediaKind::UnrecognizedImage, None)),
                Ok(Ok(Err(e))) => Err(ProbeError::Failed(format!("cannot read image: {e}"))),
                Ok(Err(e)) => Err(ProbeError::Failed(format!("image check failed: {e}"))),
                Err(_) => Err(ProbeError::Failed(
                    "reading the image header timed out".into(),
                )),
            }
        }
    }
}

fn modified_secs(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Walks a root. Disc folders are recorded as one entry each and not
/// descended into; files inside disc-structure folders (`BDMV/STREAM/*.m2ts`,
/// `VIDEO_TS/*.VOB`) are never treated as separate videos.
fn walk(root: &Path, (disc_max_entries, disc_timeout): (usize, Duration)) -> Walked {
    let mut walked = Walked::default();
    let mut it = walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            e.depth() == 0
                || !e
                    .file_name()
                    .to_str()
                    .map(|n| n.starts_with('.'))
                    .unwrap_or(false)
        });
    while let Some(entry) = it.next() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                walked.errors.push(e.to_string());
                continue;
            }
        };
        if entry.file_type().is_dir() {
            if entry.depth() == 0 {
                continue;
            }
            let name = entry.file_name().to_string_lossy();
            if disc::is_disc_structure_dir(&name) {
                // A disc structure whose parent wasn't recognized as a disc
                // (incomplete rip). Never scan its segments as movies.
                walked.errors.push(format!(
                    "{} looks like part of an incomplete disc rip; skipped",
                    entry.path().display()
                ));
                it.skip_current_dir();
                continue;
            }
            match disc::disc_folder_format(entry.path()) {
                Ok(Some(_)) => {
                    let path = entry.path().to_owned();
                    let limits = disc::FolderLimits {
                        max_entries: disc_max_entries,
                        deadline: Instant::now() + disc_timeout,
                    };
                    match disc::folder_size(&path, limits) {
                        Ok((size_bytes, modified_at)) => walked.found.push(FoundFile {
                            path,
                            size_bytes,
                            modified_at,
                            candidate: Candidate::DiscFolder,
                        }),
                        Err(e) => {
                            walked.errors.push(e);
                            walked.keep.push(path.to_string_lossy().into_owned());
                        }
                    }
                    it.skip_current_dir();
                }
                Ok(None) => {}
                Err(e) => walked
                    .errors
                    .push(format!("{}: {e}", entry.path().display())),
            }
            continue;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let candidate = if is_video_file(entry.path()) {
            Candidate::Video
        } else if disc::is_iso_path(entry.path()) && is_scannable_name(entry.path()) {
            Candidate::Iso
        } else {
            continue;
        };
        match entry.metadata() {
            Ok(meta) => walked.found.push(FoundFile {
                size_bytes: meta.len(),
                modified_at: modified_secs(&meta),
                path: entry.into_path(),
                candidate,
            }),
            Err(e) => walked.errors.push(e.to_string()),
        }
    }
    walked
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_file_filter() {
        assert!(is_video_file(Path::new("/m/Movie (2001)/movie.MKV")));
        assert!(is_video_file(Path::new("a.mp4")));
        assert!(!is_video_file(Path::new("a.nfo")));
        assert!(!is_video_file(Path::new("a.srt")));
        assert!(!is_video_file(Path::new(".hidden.mkv")));
        assert!(!is_video_file(Path::new("a.mkv.reelarr-partial")));
        assert!(!is_video_file(Path::new("noext")));
    }
}
