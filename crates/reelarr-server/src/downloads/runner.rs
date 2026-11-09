//! How the supervisor runs one job: a [`JobRunner`] per NNTP settings
//! generation, built by a [`RunnerFactory`]. Production uses
//! [`NzbRunnerFactory`] over [`PoolConnector`]; tests substitute either half.

use std::{future::Future, sync::Arc};

use async_trait::async_trait;
use reelarr_core::JobStore;
use reelarr_db::{NntpServer, SqliteJobStore, SqliteNzbStore, SqliteRecoveryPaths};
use reelarr_nzb::{
    JobId,
    nntp::{self, Pool, ServerConfig},
    worker::{
        ArticleSource, CancelToken, NzbWorker, Outcome, Par2Verify, WorkerConfig, WorkerError,
    },
};

use crate::state::NntpTrust;

/// Runs jobs for one NNTP settings generation. One runner serves many jobs
/// at once; the supervisor never runs the same job twice concurrently.
#[async_trait]
pub trait JobRunner: Send + Sync + 'static {
    /// Drive one job as far as `ReadyForImport` (see [`NzbWorker::run`]).
    async fn run(&self, id: JobId, cancel: &CancelToken) -> Result<Outcome, WorkerError>;
    /// Called once, after this generation was replaced and its last running
    /// job finished (or at shutdown). Closes the generation's connections.
    async fn retire(&self) {}
}

/// Why a runner could not be built from saved settings.
#[derive(Debug, thiserror::Error)]
#[error("could not prepare the NNTP connection pool: {0}")]
pub struct BuildError(#[from] pub nntp::Error);

/// Builds the runner for a newly loaded (enabled) NNTP provider.
#[async_trait]
pub trait RunnerFactory: Send + Sync + 'static {
    async fn build(&self, server: NntpServer) -> Result<Arc<dyn JobRunner>, BuildError>;
}

/// Opens the article source for one settings generation.
pub trait ArticleConnector: Send + Sync + 'static {
    type Source: ArticleSource + Clone;
    /// Build the source. Must not block the async runtime.
    fn connect(
        &self,
        config: ServerConfig,
    ) -> impl Future<Output = Result<Self::Source, nntp::Error>> + Send;
    /// Close a retired source once no job uses it.
    fn retire(source: &Self::Source) -> impl Future<Output = ()> + Send;
}

/// The production connector: one lazy TLS [`Pool`] per generation.
#[derive(Clone)]
pub struct PoolConnector {
    pub trust: NntpTrust,
}

impl ArticleConnector for PoolConnector {
    type Source = Pool;
    async fn connect(&self, config: ServerConfig) -> Result<Pool, nntp::Error> {
        match &self.trust {
            NntpTrust::Roots(roots) => Pool::with_root_certificates(config, (**roots).clone()),
            // Loading native roots is blocking file I/O.
            NntpTrust::Native => tokio::task::spawn_blocking(move || Pool::new(config))
                .await
                .unwrap_or(Err(nntp::Error::TrustRoots)),
        }
    }
    async fn retire(pool: &Pool) {
        pool.shutdown().await;
    }
}

/// The worker type the server runs: SQLite NZB store, PAR2 verification and
/// recovery paths recorded on the job row. The parity type is part of the
/// signature so the default `NoParity` cannot slip back in.
pub type ServerWorker<A> = NzbWorker<SqliteNzbStore, A, Par2Verify, SqliteRecoveryPaths>;

/// The one place the server constructs an [`NzbWorker`]: always
/// `with_parity(Par2Verify)` and the [`SqliteRecoveryPaths`] adapter.
pub fn server_worker<A: ArticleSource>(
    jobs: Arc<SqliteJobStore>,
    nzbs: SqliteNzbStore,
    articles: A,
    config: WorkerConfig,
) -> ServerWorker<A> {
    let recovery = SqliteRecoveryPaths::new((*jobs).clone());
    let jobs: Arc<dyn JobStore> = jobs;
    NzbWorker::new(jobs, nzbs, articles, config)
        .with_parity(Par2Verify::default())
        .with_recovery_paths(recovery)
}

/// Builds a [`ServerWorker`] per settings generation.
pub struct NzbRunnerFactory<C> {
    jobs: Arc<SqliteJobStore>,
    nzbs: SqliteNzbStore,
    connector: C,
    config: WorkerConfig,
    max_concurrent_jobs: usize,
}

impl<C: ArticleConnector> NzbRunnerFactory<C> {
    /// `max_concurrent_jobs` must match the supervisor's; it splits the
    /// provider's connection budget between jobs.
    pub fn new(
        jobs: Arc<SqliteJobStore>,
        nzbs: SqliteNzbStore,
        connector: C,
        config: WorkerConfig,
        max_concurrent_jobs: usize,
    ) -> Self {
        Self {
            jobs,
            nzbs,
            connector,
            config,
            max_concurrent_jobs: max_concurrent_jobs.max(1),
        }
    }
}

/// Article fetches per job: the provider's connections split evenly between
/// concurrent jobs (at least one), never above the configured ceiling, so all
/// running jobs together stay within `max_connections` and do not time out
/// waiting for a pool slot.
pub fn articles_per_job(max_connections: usize, max_jobs: usize, ceiling: usize) -> usize {
    (max_connections / max_jobs.max(1)).clamp(1, ceiling.max(1))
}

#[async_trait]
impl<C: ArticleConnector> RunnerFactory for NzbRunnerFactory<C> {
    async fn build(&self, server: NntpServer) -> Result<Arc<dyn JobRunner>, BuildError> {
        let mut config = self.config.clone();
        config.max_concurrent_articles = articles_per_job(
            server.config.max_connections,
            self.max_concurrent_jobs,
            config.max_concurrent_articles,
        );
        let source = self.connector.connect(server.config).await?;
        let worker = server_worker(self.jobs.clone(), self.nzbs.clone(), source.clone(), config);
        Ok(Arc::new(NzbRunner::<C> { worker, source }))
    }
}

struct NzbRunner<C: ArticleConnector> {
    worker: ServerWorker<C::Source>,
    source: C::Source,
}

#[async_trait]
impl<C: ArticleConnector> JobRunner for NzbRunner<C> {
    async fn run(&self, id: JobId, cancel: &CancelToken) -> Result<Outcome, WorkerError> {
        self.worker.run(id, cancel).await
    }
    async fn retire(&self) {
        C::retire(&self.source).await;
    }
}
