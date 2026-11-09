//! Library root health checks.
//!
//! Network shares go away: a NAS reboots, an NFS mount goes stale, an SMB
//! share is mounted after Reelarr starts. Before a scan, each root is checked
//! on a blocking thread with a timeout. An unhealthy root is skipped (its
//! files keep their rows and status) and marked offline so the UI and the
//! player can say so. The next healthy check brings it back.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reelarr_types::{LibraryRoot, LibraryRootId, MediaFileStatus, RootHealth};

use crate::db::SqliteLibraryRootStore;
use crate::scan::{MediaFileStore, Prober, ScanError, ScanReport, Scanner};
use crate::store::StoreError;

/// Default time allowed for one health check.
pub const DEFAULT_CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// Result of checking one root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checked {
    Online,
    Offline(String),
    PermissionDenied(String),
}

impl Checked {
    /// Converts to a stored [`RootHealth`], with `now` as the offline time.
    pub fn into_health(self, now: i64) -> RootHealth {
        match self {
            Checked::Online => RootHealth::Online,
            Checked::Offline(reason) => RootHealth::Offline { since: now, reason },
            Checked::PermissionDenied(reason) => RootHealth::PermissionDenied { reason },
        }
    }
}

/// Checks root folders without ever blocking the async runtime.
///
/// A check of a hung NFS mount can block in the kernel indefinitely. The
/// checker gives up after its timeout, and while that thread is still stuck
/// further checks of the same root report offline at once instead of
/// starting more stuck threads.
#[derive(Debug, Clone)]
pub struct HealthChecker {
    timeout: Duration,
    in_flight: Arc<Mutex<HashSet<LibraryRootId>>>,
}

impl Default for HealthChecker {
    fn default() -> Self {
        Self::new(DEFAULT_CHECK_TIMEOUT)
    }
}

struct InFlight {
    id: LibraryRootId,
    set: Arc<Mutex<HashSet<LibraryRootId>>>,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if let Ok(mut set) = self.set.lock() {
            set.remove(&self.id);
        }
    }
}

impl HealthChecker {
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            in_flight: Arc::default(),
        }
    }

    /// Checks `root`. `known_files` is how many present files the library
    /// knows under it; an empty folder with known files is treated as an
    /// unmounted share.
    pub async fn check(&self, root: &LibraryRoot, known_files: usize) -> Checked {
        let guard = {
            let Ok(mut set) = self.in_flight.lock() else {
                return Checked::Offline("health checker lock poisoned".into());
            };
            if !set.insert(root.id) {
                return Checked::Offline(
                    "an earlier check of this folder has not finished; the share may be hung"
                        .into(),
                );
            }
            InFlight {
                id: root.id,
                set: self.in_flight.clone(),
            }
        };
        let path = PathBuf::from(&root.path);
        let handle = tokio::task::spawn_blocking(move || {
            let _guard = guard;
            check_path(&path, known_files)
        });
        match tokio::time::timeout(self.timeout, handle).await {
            Ok(Ok(checked)) => checked,
            Ok(Err(e)) => Checked::Offline(format!("health check failed: {e}")),
            Err(_) => Checked::Offline(format!(
                "folder did not respond within {}s; the share may be hung or disconnected",
                self.timeout.as_secs()
            )),
        }
    }
}

/// Synchronous check of one folder. Public for tests and tools.
pub fn check_path(path: &Path, known_files: usize) -> Checked {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) => return classify(&e),
    };
    if !meta.is_dir() {
        return Checked::Offline("path exists but is not a folder".into());
    }
    let mut entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(e) => return classify(&e),
    };
    match entries.next() {
        Some(Err(e)) => classify(&e),
        None if known_files > 0 => Checked::Offline(format!(
            "folder is empty but the library knows {known_files} files in it; the share is probably not mounted"
        )),
        _ => Checked::Online,
    }
}

// Linux errno values; Reelarr's container is Linux.
const EIO: i32 = 5;
const ENOTCONN: i32 = 107;
const ETIMEDOUT: i32 = 110;
const EHOSTDOWN: i32 = 112;
const EHOSTUNREACH: i32 = 113;
const ESTALE: i32 = 116;

/// Maps an I/O error from a root folder to a user-facing health result.
pub fn classify(e: &io::Error) -> Checked {
    if e.kind() == io::ErrorKind::PermissionDenied {
        return Checked::PermissionDenied(
            "Reelarr is not allowed to read this folder; check the share's permissions and the container's user (PUID/PGID)".into(),
        );
    }
    if e.kind() == io::ErrorKind::NotFound {
        return Checked::Offline("folder not found; is the share mounted?".into());
    }
    let reason = match e.raw_os_error() {
        Some(ESTALE) => "stale network file handle; remount the share",
        Some(ENOTCONN) => "the network share is disconnected",
        Some(EIO) => "I/O error reading the folder",
        Some(EHOSTDOWN | EHOSTUNREACH) => "the file server is unreachable",
        Some(ETIMEDOUT) => "the file server timed out",
        _ => return Checked::Offline(format!("folder could not be read: {e}")),
    };
    Checked::Offline(format!("{reason} ({e})"))
}

/// What happened when a root was scanned through [`scan_checked`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootScan {
    Scanned(ScanReport),
    /// The root was unhealthy; nothing was scanned or marked missing.
    Skipped(RootHealth),
    /// Another scan of the root was already running.
    AlreadyRunning,
}

/// Checks a root's health, records it, and scans it only if it is healthy.
pub async fn scan_checked<P, S>(
    scanner: &Scanner<P, S>,
    roots: &SqliteLibraryRootStore,
    checker: &HealthChecker,
    root: &LibraryRoot,
) -> Result<RootScan, StoreError>
where
    P: Prober,
    S: MediaFileStore,
{
    let known = scanner
        .store()
        .list_by_root(root.id)
        .await?
        .iter()
        .filter(|f| f.status == MediaFileStatus::Present)
        .count();
    let now = crate::db::now_secs();
    let checked = checker.check(root, known).await;
    if checked != Checked::Online {
        let health = checked.into_health(now);
        tracing::warn!(root = %root.path, ?health, "library root unavailable; skipping scan");
        roots.set_health(root.id, &health, now).await?;
        return Ok(RootScan::Skipped(health));
    }
    match scanner.scan_root(root).await {
        Ok(report) => {
            roots
                .set_health(root.id, &RootHealth::Online, crate::db::now_secs())
                .await?;
            Ok(RootScan::Scanned(report))
        }
        Err(ScanError::AlreadyRunning) => Ok(RootScan::AlreadyRunning),
        Err(ScanError::Store(e)) => Err(e),
        Err(e @ (ScanError::RootUnavailable { .. } | ScanError::RootLooksUnmounted { .. })) => {
            let health = Checked::Offline(e.to_string()).into_health(now);
            roots.set_health(root.id, &health, now).await?;
            Ok(RootScan::Skipped(health))
        }
    }
}
