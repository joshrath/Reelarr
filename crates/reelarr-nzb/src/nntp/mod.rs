//! Verified implicit-TLS NNTP article retrieval for a single provider.
//!
//! Independently implemented from RFC 3977 §§3, 5.1, 5.4, 6.2.3–6.2.4,
//! RFC 4643 §2.3, and RFC 4642 as updated by RFC 8143. No reference-client
//! source was used. TLS 1.2/1.3, certificate-chain and hostname verification
//! are mandatory; there is no plaintext fallback or verification bypass.
//!
//! [`Pool::body`] returns **raw dot-stuffed BODY lines**, including their CRLFs
//! but excluding the status and lone-dot terminator. Feed these bytes to #13's
//! yEnc decoder with `BodyEncoding::DotStuffed`, never `Unstuffed`. Both byte
//! and line limits apply to the wire representation. NZB declared sizes are
//! hints, not allocation sizes or trusted response limits.
//!
//! No protocol transcript, message IDs, server text or credentials are logged.
//! Errors retain only local classifications and numeric response codes. Each
//! operation owns one connection exclusively. Cancellation, timeout, incomplete
//! replies and protocol errors discard it, so a later caller cannot consume a
//! stale response. Only complete successes and 430 replies are reusable.
//!
//! This is a transport boundary, not a scheduler or persistence layer. Instantiate
//! one pool per provider; priority/fill selection can later wrap this interface.
//! There is no pipelining, STARTTLS, SASL, ARTICLE fallback or compression.
//!
//! ```no_run
//! use reelarr_nzb::{Segment, nntp::{Credentials, Error, Pool, Secret, ServerConfig}};
//!
//! async fn retrieve(segment: &Segment, username: Secret, password: Secret) -> Result<(), Error> {
//!     let mut config = ServerConfig::new("news.example.org");
//!     config.credentials = Some(Credentials { username, password });
//!     let pool = Pool::new(config)?; // Load native roots once, during setup.
//!     let body = pool.body(segment).await?;
//!     // The worker feeds body.as_bytes() to yEnc with BodyEncoding::DotStuffed.
//!     // Do not log the body, credentials, original secret strings or wire replies.
//!     let _wire_bytes = body.into_bytes();
//!     pool.shutdown().await;
//!     Ok(())
//! }
//! ```

mod config;
mod connection;
mod pool;

pub use config::{
    Credentials, Limits, RetryPolicy, Secret, ServerConfig, Timeouts, validate_credential,
};
pub use pool::Pool;
/// Trust anchors accepted by [`Pool::with_root_certificates`]; re-exported so
/// callers can hold a custom store without a direct rustls dependency.
pub use rustls::RootCertStore;
use std::io;
use thiserror::Error;

/// Secret-free failure classification. Server-supplied response text is discarded.
#[derive(Debug, Error)]
pub enum Error {
    /// Invalid local settings or an unsafe article locator.
    #[error("invalid NNTP configuration: {0}")]
    Config(&'static str),
    /// OS trust store was unavailable or contained no usable roots.
    #[error("NNTP trust roots unavailable")]
    TrustRoots,
    /// TLS setup or certificate/hostname validation failed; never retried.
    #[error("NNTP TLS validation or negotiation failed")]
    Tls,
    /// Socket failure. Only the error kind is retained, not arbitrary error text.
    #[error("NNTP transport failed: {0:?}")]
    Transport(io::ErrorKind),
    /// Server closed before a complete response arrived.
    #[error("incomplete NNTP response")]
    Incomplete,
    /// A deadline elapsed; the static label identifies its scope.
    #[error("NNTP timeout: {0}")]
    Timeout(&'static str),
    /// Article is absent on this provider (430), not a transient failure.
    #[error("NNTP article not found (430)")]
    NotFound,
    /// Authentication or authorization failed. Never retried automatically.
    #[error("NNTP authentication/authorization rejected ({code})")]
    Authentication {
        /// NNTP response code, with no response text.
        code: u16,
    },
    /// Temporary server outage/internal fault (400 or 403).
    #[error("NNTP server temporarily unavailable ({code})")]
    Unavailable {
        /// NNTP response code.
        code: u16,
    },
    /// A syntactically valid but unexpected status code.
    #[error("unexpected NNTP response ({code})")]
    Response {
        /// NNTP response code.
        code: u16,
    },
    /// Malformed framing or reply arguments; the connection is discarded.
    #[error("invalid NNTP protocol: {0}")]
    Protocol(&'static str),
    /// A bounded response exceeded a configured limit.
    #[error("NNTP limit exceeded: {0}")]
    Limit(&'static str),
    /// Shutdown has begun; new acquisitions are rejected.
    #[error("NNTP pool closed")]
    Closed,
}
impl Error {
    /// Whether the same provider may be retried within its configured budget.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Transport(_)
                | Self::Incomplete
                | Self::Unavailable { .. }
                | Self::Timeout("connect" | "read" | "write")
        )
    }
    pub(super) fn response(code: u16) -> Self {
        match code {
            430 => Self::NotFound,
            480..=483 | 502 => Self::Authentication { code },
            400 | 403 => Self::Unavailable { code },
            _ => Self::Response { code },
        }
    }
}

/// A bounded complete NNTP BODY. Debug shows only length, never article content.
/// Dot-stuffing is intentionally retained; see the module's framing contract.
pub struct ArticleBody(Vec<u8>);
impl ArticleBody {
    /// Raw dot-stuffed bytes, without the status line or terminator.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
    /// Transfer ownership of the raw dot-stuffed bytes to a decoder/worker.
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}
impl std::fmt::Debug for ArticleBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArticleBody")
            .field("bytes", &self.0.len())
            .finish()
    }
}
