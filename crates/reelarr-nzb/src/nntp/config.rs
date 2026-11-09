use super::Error;
use rustls::pki_types::ServerName;
use std::{fmt, time::Duration};

/// Credential value redacted in both Debug and Display. Not serializable.
/// This protects diagnostics, not memory dumps or encrypted-at-rest storage.
#[derive(Clone)]
pub struct Secret(pub(super) String);
impl Secret {
    /// Wrap a credential without exposing it through formatting.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}
impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}
impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

/// Optional AUTHINFO USER/PASS credentials, sent only after verified TLS.
#[derive(Clone, Debug)]
pub struct Credentials {
    /// Username, also treated as secret.
    pub username: Secret,
    /// Password, never included in errors, traces or response text.
    pub password: Secret,
}

/// Time budgets; all must be nonzero and at most one day.
#[derive(Clone, Copy, Debug)]
pub struct Timeouts {
    /// Waiting for a pool slot (default 10 seconds, not retried).
    pub acquire: Duration,
    /// DNS lookup, TCP connect and TLS handshake together (default 15 seconds).
    pub connect: Duration,
    /// Each complete physical response line or command write (default 15 seconds).
    pub io: Duration,
    /// Entire public BODY/STAT operation, including acquire/retries (default 90s).
    pub operation: Duration,
}
impl Default for Timeouts {
    fn default() -> Self {
        Self {
            acquire: Duration::from_secs(10),
            connect: Duration::from_secs(15),
            io: Duration::from_secs(15),
            operation: Duration::from_secs(90),
        }
    }
}

/// Hard wire-response ceilings, not inferred from untrusted NZB declarations.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Physical BODY line bytes including CRLF (default 8192). Status lines
    /// additionally have the RFC's 512-byte limit. Minimum 512, maximum 1 MiB.
    pub max_line_bytes: usize,
    /// Entire dot-stuffed BODY including CRLFs but not terminator (default 8 MiB).
    /// Must be 1..=128 MiB. Bodies are buffered; budget memory per connection.
    pub max_article_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_line_bytes: 8192,
            max_article_bytes: 8 * 1024 * 1024,
        }
    }
}

/// Per-operation reconnect budget; 430, auth, TLS and protocol errors never retry.
#[derive(Clone, Copy, Debug)]
pub struct RetryPolicy {
    /// Additional attempts after the first (default 2, maximum 10).
    pub max_retries: u8,
    /// Exponential backoff floor (default 100ms), with bounded positive jitter.
    pub initial_backoff: Duration,
    /// Backoff including jitter never exceeds this (default 2s, maximum 30s).
    pub max_backoff: Duration,
}
impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(2),
        }
    }
}

/// A single implicit-TLS provider. No plaintext or insecure verification option.
/// Construct with [`ServerConfig::new`], then customize budgets before pooling.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// Hostname/IP used for both DNS/TCP and certificate identity verification.
    pub host: String,
    /// Implicit-TLS port (default 563).
    pub port: u16,
    /// No AUTHINFO is sent if absent.
    pub credentials: Option<Credentials>,
    /// Maximum simultaneously established/in-use connections (default 8, 1..=1024).
    pub max_connections: usize,
    /// Timeout budgets.
    pub timeouts: Timeouts,
    /// Wire-response limits.
    pub limits: Limits,
    /// Bounded reconnect/backoff policy.
    pub retry: RetryPolicy,
}
impl ServerConfig {
    /// Secure defaults for a provider hostname; validation occurs at pool creation.
    pub fn new(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port: 563,
            credentials: None,
            max_connections: 8,
            timeouts: Timeouts::default(),
            limits: Limits::default(),
            retry: RetryPolicy::default(),
        }
    }
    pub(super) fn validate(&self) -> Result<(), Error> {
        if self.host.is_empty()
            || self
                .host
                .chars()
                .any(|c| c.is_whitespace() || c.is_control())
            || ServerName::try_from(self.host.clone()).is_err()
            || self.port == 0
        {
            return Err(Error::Config("host or port"));
        }
        if !(1..=1024).contains(&self.max_connections) {
            return Err(Error::Config("connection count"));
        }
        if !(512..=1024 * 1024).contains(&self.limits.max_line_bytes)
            || !(1..=128 * 1024 * 1024).contains(&self.limits.max_article_bytes)
        {
            return Err(Error::Config("response limits"));
        }
        for d in [
            self.timeouts.acquire,
            self.timeouts.connect,
            self.timeouts.io,
            self.timeouts.operation,
        ] {
            if d.is_zero() || d > Duration::from_secs(86400) {
                return Err(Error::Config("timeouts"));
            }
        }
        if self.retry.max_retries > 10
            || self.retry.initial_backoff.is_zero()
            || self.retry.initial_backoff > self.retry.max_backoff
            || self.retry.max_backoff > Duration::from_secs(30)
        {
            return Err(Error::Config("retry policy"));
        }
        if let Some(c) = &self.credentials {
            // Conservative AUTHINFO token subset: whitespace is not portable
            // between servers (RFC 4643 §2.3.2). No ambiguous normalization.
            for value in [&c.username.0, &c.password.0] {
                if value.is_empty()
                    || value.len() + 16 > 512
                    || !value.bytes().all(|b| b.is_ascii_graphic())
                {
                    return Err(Error::Config(
                        "AUTHINFO requires printable non-whitespace credentials within 512 bytes",
                    ));
                }
            }
        }
        Ok(())
    }
}
