use reelarr_core::auth::{AuthService, InMemoryAuthService};
use reelarr_db::{
    Database, SqliteJobStore, SqliteNntpStore,
    secrets::{SecretCipher, XChaChaSecretCipher},
};
use reelarr_media::db::{DbMediaLookup, SqliteLibraryRootStore};
use reelarr_media::health::HealthChecker;
use reelarr_nzb::nntp::RootCertStore;
use reelarr_stream::MediaLookup;
use std::sync::Arc;

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
    /// Trust anchors for NNTP TLS (test-connection, and the worker pool later).
    pub nntp_trust: NntpTrust,
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
        Self {
            jobs: Arc::new(SqliteJobStore::new(&db)),
            auth: Arc::new(InMemoryAuthService::default()),
            media: Arc::new(DbMediaLookup::new(&db)),
            library_roots: SqliteLibraryRootStore::new(&db),
            root_health: HealthChecker::default(),
            nntp: SqliteNntpStore::new(&db, cipher),
            nntp_trust: NntpTrust::Native,
            db,
        }
    }
}
