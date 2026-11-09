use reelarr_core::auth::{AuthService, InMemoryAuthService};
use reelarr_db::{
    Database, SqliteJobStore, SqliteNntpStore, SqliteNzbStore,
    secrets::{SecretCipher, XChaChaSecretCipher},
};
use reelarr_media::db::{DbMediaLookup, SqliteLibraryRootStore};
use reelarr_media::health::HealthChecker;
use reelarr_nzb::{nntp::RootCertStore, worker::WorkerConfig};
use reelarr_stream::MediaLookup;
use std::{path::PathBuf, sync::Arc};

use crate::downloads::{NzbRunnerFactory, PoolConnector, Supervisor, SupervisorConfig};

/// Container default for [`DownloadOptions::root`].
pub const DEFAULT_DOWNLOADS_ROOT: &str = "/downloads/reelarr";

/// Where the built-in downloader works and how much it runs at once.
#[derive(Clone)]
pub struct DownloadOptions {
    /// Reelarr-owned folder: `incomplete/` holds per-job staging, `complete/`
    /// the published downloads waiting for import.
    pub root: PathBuf,
    pub scheduler: SupervisorConfig,
    /// Trust anchors for NNTP TLS (test-connection and the download pools).
    pub trust: NntpTrust,
}

impl DownloadOptions {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            scheduler: SupervisorConfig::default(),
            trust: NntpTrust::Native,
        }
    }

    pub fn worker_config(&self) -> WorkerConfig {
        WorkerConfig::new(self.root.join("incomplete"), self.root.join("complete"))
    }
}

/// Which certificate authorities an NNTP connection trusts. Hostname and chain
/// verification are always on; there is no option to disable them.
#[derive(Clone)]
pub enum NntpTrust {
    /// The operating system's trust store (loaded per pool, off the async path).
    Native,
    /// An explicit set of trust anchors (private provider CA, or tests).
    Roots(Arc<RootCertStore>),
}

/// Shared application state; clones share database, jobs, authentication and media lookup services.
#[derive(Clone)]
pub struct AppState {
    pub db: Database,
    pub jobs: Arc<SqliteJobStore>,
    pub auth: Arc<dyn AuthService>,
    /// Finds media files for playback.
    pub media: Arc<dyn MediaLookup>,
    /// Library folders and their last health check.
    pub library_roots: SqliteLibraryRootStore,
    pub root_health: HealthChecker,
    /// The single NNTP provider; its password is encrypted at rest.
    pub nntp: SqliteNntpStore,
    /// Trust anchors for NNTP TLS test-connection.
    pub nntp_trust: NntpTrust,
    /// Download scheduler and its NZB worker. The binary starts it before
    /// binding; [`Supervisor::start`] runs restart recovery first.
    pub downloads: Supervisor,
}

impl AppState {
    /// State whose secrets use a random, process-only key: stored NNTP passwords
    /// will not decrypt after a restart. For tests and throwaway use; the server
    /// binary uses [`AppState::with_cipher`] with the persistent key.
    pub fn new(db: Database) -> Self {
        let cipher = XChaChaSecretCipher::ephemeral().expect("operating system randomness");
        Self::with_cipher(db, Arc::new(cipher))
    }

    /// State using the startup-loaded secret cipher (`REELARR_SECRET_KEY` or
    /// the 0600 `secret.key` in the data directory).
    pub fn with_cipher(db: Database, cipher: Arc<dyn SecretCipher>) -> Self {
        Self::with_options(db, cipher, DownloadOptions::new(DEFAULT_DOWNLOADS_ROOT))
    }

    /// State with explicit download folders and scheduling limits. Nothing
    /// runs until `downloads.start()`.
    pub fn with_options(
        db: Database,
        cipher: Arc<dyn SecretCipher>,
        downloads: DownloadOptions,
    ) -> Self {
        let jobs = Arc::new(SqliteJobStore::new(&db));
        let nzbs = SqliteNzbStore::new(&db);
        let nntp = SqliteNntpStore::new(&db, cipher);
        let factory = NzbRunnerFactory::new(
            jobs.clone(),
            nzbs.clone(),
            PoolConnector {
                trust: downloads.trust.clone(),
            },
            downloads.worker_config(),
            downloads.scheduler.max_concurrent_jobs,
        );
        let supervisor = Supervisor::new(
            jobs.clone(),
            nzbs,
            nntp.clone(),
            Arc::new(factory),
            downloads.scheduler.clone(),
        );
        Self {
            jobs,
            auth: Arc::new(InMemoryAuthService::default()),
            media: Arc::new(DbMediaLookup::new(&db)),
            library_roots: SqliteLibraryRootStore::new(&db),
            root_health: HealthChecker::default(),
            nntp,
            nntp_trust: downloads.trust,
            downloads: supervisor,
            db,
        }
    }
}
