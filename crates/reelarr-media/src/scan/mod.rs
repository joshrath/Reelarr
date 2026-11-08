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

mod probe;
mod store;

pub use probe::{FfprobeProber, ProbeError, Prober, parse_ffprobe_json};
pub use store::{InMemoryMediaFileStore, MediaFileStore, NewMediaFile};

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::stream::{self, StreamExt};
use reelarr_types::{LibraryRoot, LibraryRootId, MediaFile, MediaFileStatus, MediaProbe};
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
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            probe_concurrency: 2,
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
        let (found, walk_errors) = tokio::task::spawn_blocking(move || walk(&walk_root))
            .await
            .map_err(|e| ScanError::RootUnavailable {
                path: root.path.clone(),
                reason: format!("walk task failed: {e}"),
            })?;

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

        // Decide what needs probing.
        let mut to_probe: Vec<(FoundFile, Option<MediaFile>)> = Vec::new();
        for file in found {
            let key = file.path.to_string_lossy().into_owned();
            match known_by_path.remove(&key) {
                Some(existing)
                    if existing.size_bytes == file.size_bytes
                        && existing.modified_at == file.modified_at
                        && existing.probe.is_some() =>
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
        let probed: Vec<_> = stream::iter(to_probe)
            .map(|(file, existing)| async move {
                let result = prober.probe(&file.path).await;
                (file, existing, result)
            })
            .buffer_unordered(self.options.probe_concurrency.max(1))
            .collect()
            .await;

        for (file, existing, result) in probed {
            let path = file.path.to_string_lossy().into_owned();
            let probe: Option<MediaProbe> = match result {
                Ok(probe) => Some(probe),
                Err(e) => {
                    tracing::warn!(path = %path, error = %e, "probe failed");
                    report.probe_failures.push((path.clone(), e.to_string()));
                    None
                }
            };
            match existing {
                Some(mut existing) => {
                    existing.size_bytes = file.size_bytes;
                    existing.modified_at = file.modified_at;
                    existing.status = MediaFileStatus::Present;
                    existing.probe = probe;
                    self.store.update(&existing).await?;
                    report.updated += 1;
                }
                None => {
                    self.store
                        .insert(NewMediaFile {
                            root_id: root.id,
                            path,
                            size_bytes: file.size_bytes,
                            modified_at: file.modified_at,
                            probe,
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

/// Returns true for files the scanner should consider.
pub fn is_video_file(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name.starts_with('.') || name.ends_with(PARTIAL_SUFFIX) {
        return false;
    }
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| VIDEO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

fn walk(root: &Path) -> (Vec<FoundFile>, Vec<String>) {
    let mut found = Vec::new();
    let mut errors = Vec::new();
    let walker = walkdir::WalkDir::new(root)
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
    for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                errors.push(e.to_string());
                continue;
            }
        };
        if !entry.file_type().is_file() || !is_video_file(entry.path()) {
            continue;
        }
        match entry.metadata() {
            Ok(meta) => found.push(FoundFile {
                path: entry.into_path(),
                size_bytes: meta.len(),
                modified_at: meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0),
            }),
            Err(e) => errors.push(e.to_string()),
        }
    }
    (found, errors)
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
