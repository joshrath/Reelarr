use super::{
    ArticleBody, Error, ServerConfig,
    connection::{Connection, deadline, validate_id},
};
use crate::Segment;
use rustls::{ClientConfig, RootCertStore};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Cloneable, lazy pool for one provider. No connection is opened at construction.
/// At most `max_connections` TLS sockets exist at once. Idle sockets are reused
/// until the peer closes them or an operation fails. Drop closes sockets without
/// waiting; [`Pool::shutdown`] optionally sends QUIT to idle connections.
#[derive(Clone)]
pub struct Pool {
    inner: Arc<Inner>,
}
struct Inner {
    config: Arc<ServerConfig>,
    tls: Arc<ClientConfig>,
    slots: Arc<Semaphore>,
    idle: Mutex<Vec<Connection>>,
    closed: AtomicBool,
    jitter: AtomicU64,
}
impl Pool {
    /// Use platform-native trust roots (loaded synchronously once). Call during
    /// setup or on a blocking worker, not on a latency-sensitive async path.
    /// Environment overrides supported by rustls-native-certs remain effective.
    pub fn new(config: ServerConfig) -> Result<Self, Error> {
        config.validate()?;
        let loaded = rustls_native_certs::load_native_certs();
        let mut roots = RootCertStore::empty();
        roots.add_parsable_certificates(loaded.certs);
        if roots.is_empty() {
            return Err(Error::TrustRoots);
        }
        Self::with_root_certificates(config, roots)
    }
    /// Use an explicit trust store (e.g. a private provider CA or test CA).
    /// This only changes trust anchors: hostname and chain verification remain
    /// mandatory. Does not accept a custom verifier, plaintext, or TLS config.
    pub fn with_root_certificates(
        config: ServerConfig,
        roots: RootCertStore,
    ) -> Result<Self, Error> {
        config.validate()?;
        if roots.is_empty() {
            return Err(Error::TrustRoots);
        }
        let tls =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
                .map_err(|_| Error::Tls)?
                .with_root_certificates(roots)
                .with_no_client_auth();
        let max = config.max_connections;
        Ok(Self {
            inner: Arc::new(Inner {
                config: Arc::new(config),
                tls: Arc::new(tls),
                slots: Arc::new(Semaphore::new(max)),
                idle: Mutex::new(Vec::new()),
                closed: AtomicBool::new(false),
                jitter: AtomicU64::new(1),
            }),
        })
    }
    /// Retrieve a complete raw dot-stuffed BODY by the parser's segment locator.
    /// Ignore declared size as an allocation hint; configured ceilings apply.
    /// Partial responses are never returned. Cancelling the future discards the
    /// socket and releases the pool slot. Transient failures retry with backoff.
    pub async fn body(&self, segment: &Segment) -> Result<ArticleBody, Error> {
        match self.perform(&segment.message_id, Operation::Body).await? {
            Reply::Body(body) => Ok(body),
            Reply::Stat => Err(Error::Protocol("internal operation mismatch")),
        }
    }
    /// Check article existence by Message-ID; absence returns [`Error::NotFound`].
    /// A successful STAT does not guarantee a later BODY will still be available.
    pub async fn stat(&self, segment: &Segment) -> Result<(), Error> {
        self.perform(&segment.message_id, Operation::Stat)
            .await
            .map(|_| ())
    }
    async fn acquire(&self) -> Result<Lease, Error> {
        let permit = deadline(self.inner.config.timeouts.acquire, "acquire", async {
            self.inner
                .slots
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| Error::Closed)
        })
        .await?;
        let connection = {
            let mut idle = self.inner.idle.lock().map_err(|_| Error::Closed)?;
            if self.inner.closed.load(Ordering::Acquire) {
                return Err(Error::Closed);
            }
            idle.pop()
        };
        let connection = match connection {
            Some(connection) => connection,
            None => Connection::connect(self.inner.config.clone(), self.inner.tls.clone()).await?,
        };
        Ok(Lease {
            inner: self.inner.clone(),
            connection: Some(connection),
            _permit: permit,
        })
    }
    async fn perform(&self, id: &str, operation: Operation) -> Result<Reply, Error> {
        validate_id(id)?;
        deadline(self.inner.config.timeouts.operation, "operation", async {
            let retry = self.inner.config.retry;
            for attempt in 0..=retry.max_retries {
                let result = self.attempt(id, operation).await;
                match result {
                    Err(ref error) if error.is_retryable() && attempt < retry.max_retries => {
                        // Only static local data and attempt count: no provider text,
                        // identifiers, credential fields, commands or config Debug.
                        tracing::debug!(target: "reelarr_nzb::nntp", attempt, "retrying transient NNTP failure");
                        let base = (retry.initial_backoff * (1_u32 << attempt)).min(retry.max_backoff);
                        let salt = self.inner.jitter.fetch_add(1, Ordering::Relaxed)
                            ^ SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().subsec_nanos() as u64;
                        let jitter = base.mul_f64((salt % 251) as f64 / 1000.0);
                        tokio::time::sleep((base + jitter).min(retry.max_backoff)).await;
                    }
                    result => return result,
                }
            }
            Err(Error::Protocol("retry budget"))
        }).await
    }
    async fn attempt(&self, id: &str, operation: Operation) -> Result<Reply, Error> {
        let mut lease = self.acquire().await?;
        // Take before any I/O. Cancellation/panic/unwind drops this socket, not
        // a healthy-looking connection in the idle pool. Only restore after a
        // fully consumed success or 430. No async work in Drop.
        let mut connection = lease.connection.take().ok_or(Error::Closed)?;
        let result = match operation {
            Operation::Body => connection.body(id).await.map(Reply::Body),
            Operation::Stat => connection.stat(id).await.map(|()| Reply::Stat),
        };
        if result.is_ok() || matches!(result, Err(Error::NotFound)) {
            lease.connection = Some(connection);
        }
        result
    }
    /// Reject new work and wake waiting acquirers. Send QUIT to currently idle
    /// sockets, bounded by one operation deadline **in total**. Busy sockets close
    /// when their operations finish/cancel; shutdown does not cancel caller tasks.
    /// Failed/timed-out QUIT is best effort: sockets are always dropped.
    pub async fn shutdown(&self) {
        self.inner.closed.store(true, Ordering::Release);
        self.inner.slots.close();
        let idle = match self.inner.idle.lock() {
            Ok(mut idle) => std::mem::take(&mut *idle),
            Err(_) => return,
        };
        let _ = tokio::time::timeout(self.inner.config.timeouts.operation, async {
            for connection in idle {
                let _ = connection.quit().await;
            }
        })
        .await;
    }
}
#[derive(Clone, Copy)]
enum Operation {
    Body,
    Stat,
}
enum Reply {
    Body(ArticleBody),
    Stat,
}
struct Lease {
    inner: Arc<Inner>,
    connection: Option<Connection>,
    _permit: OwnedSemaphorePermit,
}
impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take()
            && let Ok(mut idle) = self.inner.idle.lock()
            && !self.inner.closed.load(Ordering::Acquire)
        {
            idle.push(connection);
        }
    }
}
