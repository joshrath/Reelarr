//! NNTP (Usenet) provider settings DTOs (#76 PR 3).
//!
//! Plain serde data only, so the UI can share these types from WebAssembly.
//! Credentials are **write-only**: [`NntpServerSettings`] never carries the
//! password, only `has_password`. Debug output redacts the username and any
//! password, so request/response values are safe to log by accident.
//! Reelarr talks to exactly one enabled provider; connections always use
//! implicit TLS with certificate and hostname verification.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Implicit-TLS NNTP port (RFC 8143); the default for new settings.
pub const NNTP_DEFAULT_PORT: u16 = 563;
/// Plaintext NNTP port. Rejected: Reelarr only speaks implicit TLS.
pub const NNTP_PLAINTEXT_PORT: u16 = 119;
/// Default simultaneous connections to the provider.
pub const NNTP_DEFAULT_CONNECTIONS: u16 = 8;
/// Upper bound on simultaneous connections accepted from settings.
pub const NNTP_MAX_CONNECTIONS: u16 = 100;
/// Default TCP+TLS connect timeout, and the bound on a whole test-connection.
pub const NNTP_DEFAULT_CONNECT_TIMEOUT_SECS: u32 = 15;
/// Default timeout for each response line or command write.
pub const NNTP_DEFAULT_IO_TIMEOUT_SECS: u32 = 15;
/// Upper bound for either timeout, in seconds.
pub const NNTP_MAX_TIMEOUT_SECS: u32 = 300;
/// Default reconnect attempts after a transient failure.
pub const NNTP_DEFAULT_RETRIES: u8 = 2;
/// Upper bound on reconnect attempts.
pub const NNTP_MAX_RETRIES: u8 = 10;

/// Stored provider settings as returned by the API. Never contains a password.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NntpServerSettings {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    /// AUTHINFO username, if the provider requires login.
    pub username: Option<String>,
    /// Whether an (encrypted) password is stored. The value itself is never returned.
    pub has_password: bool,
    pub max_connections: u16,
    pub connect_timeout_secs: u32,
    pub io_timeout_secs: u32,
    pub max_retries: u8,
}

impl fmt::Debug for NntpServerSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NntpServerSettings")
            .field("enabled", &self.enabled)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username.as_ref().map(|_| Redacted))
            .field("has_password", &self.has_password)
            .field("max_connections", &self.max_connections)
            .field("connect_timeout_secs", &self.connect_timeout_secs)
            .field("io_timeout_secs", &self.io_timeout_secs)
            .field("max_retries", &self.max_retries)
            .finish()
    }
}

/// What an update does with the stored password.
///
/// JSON: `{"action":"keep"}`, `{"action":"set","value":"..."}` or
/// `{"action":"clear"}`. Omitting the field means `keep`.
#[derive(Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "action", content = "value", rename_all = "snake_case")]
pub enum PasswordUpdate {
    /// Leave the stored password as it is. Refused if the host changes, so a
    /// saved password is never sent to a different server without re-entry.
    #[default]
    Keep,
    /// Replace the stored password.
    Set(String),
    /// Remove the stored password.
    Clear,
}

impl fmt::Debug for PasswordUpdate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Keep => f.write_str("Keep"),
            Self::Set(_) => f.write_str("Set([REDACTED])"),
            Self::Clear => f.write_str("Clear"),
        }
    }
}

/// Body of `PUT /api/v1/settings/nntp` and `POST /api/v1/settings/nntp/test`.
/// Omitted optional fields take the documented defaults.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NntpServerUpdate {
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: PasswordUpdate,
    #[serde(default = "default_connections")]
    pub max_connections: u16,
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_secs: u32,
    #[serde(default = "default_io_timeout")]
    pub io_timeout_secs: u32,
    #[serde(default = "default_retries")]
    pub max_retries: u8,
}

impl NntpServerUpdate {
    /// Defaults for a provider host, with no credentials.
    pub fn new(host: impl Into<String>) -> Self {
        Self {
            enabled: true,
            host: host.into(),
            port: NNTP_DEFAULT_PORT,
            username: None,
            password: PasswordUpdate::Keep,
            max_connections: NNTP_DEFAULT_CONNECTIONS,
            connect_timeout_secs: NNTP_DEFAULT_CONNECT_TIMEOUT_SECS,
            io_timeout_secs: NNTP_DEFAULT_IO_TIMEOUT_SECS,
            max_retries: NNTP_DEFAULT_RETRIES,
        }
    }
}

impl fmt::Debug for NntpServerUpdate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NntpServerUpdate")
            .field("enabled", &self.enabled)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username.as_ref().map(|_| Redacted))
            .field("password", &self.password)
            .field("max_connections", &self.max_connections)
            .field("connect_timeout_secs", &self.connect_timeout_secs)
            .field("io_timeout_secs", &self.io_timeout_secs)
            .field("max_retries", &self.max_retries)
            .finish()
    }
}

fn default_true() -> bool {
    true
}
fn default_port() -> u16 {
    NNTP_DEFAULT_PORT
}
fn default_connections() -> u16 {
    NNTP_DEFAULT_CONNECTIONS
}
fn default_connect_timeout() -> u32 {
    NNTP_DEFAULT_CONNECT_TIMEOUT_SECS
}
fn default_io_timeout() -> u32 {
    NNTP_DEFAULT_IO_TIMEOUT_SECS
}
fn default_retries() -> u8 {
    NNTP_DEFAULT_RETRIES
}

struct Redacted;
impl fmt::Debug for Redacted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

/// How a test-connection attempt ended. Never includes server-supplied text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NntpTestOutcome {
    /// TLS verified, greeting accepted and (if configured) login succeeded.
    Connected,
    /// Certificate chain or hostname verification failed, or TLS negotiation failed.
    TlsRejected,
    /// The server refused the username/password.
    AuthRejected,
    /// The attempt did not finish within the connect timeout.
    Timeout,
    /// The host could not be reached (DNS, refused, reset).
    Unreachable,
    /// The server answered but is temporarily unavailable or refused service.
    ServerUnavailable,
    /// The server's replies were not valid NNTP.
    ProtocolError,
    /// No usable system trust roots were found.
    TrustRootsUnavailable,
}

/// Response of `POST /api/v1/settings/nntp/test`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NntpTestResult {
    pub ok: bool,
    pub outcome: NntpTestOutcome,
    /// Human-readable summary built locally; never echoes credentials or server text.
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_update_json_shapes() {
        let keep: NntpServerUpdate = serde_json::from_str(r#"{"host":"news.example"}"#).unwrap();
        assert_eq!(keep, NntpServerUpdate::new("news.example"));
        let set: NntpServerUpdate = serde_json::from_str(
            r#"{"host":"h","username":"u","password":{"action":"set","value":"p"}}"#,
        )
        .unwrap();
        assert_eq!(set.password, PasswordUpdate::Set("p".into()));
        let clear: NntpServerUpdate =
            serde_json::from_str(r#"{"host":"h","password":{"action":"clear"}}"#).unwrap();
        assert_eq!(clear.password, PasswordUpdate::Clear);
        assert!(
            serde_json::from_str::<NntpServerUpdate>(r#"{"host":"h","has_password":true}"#)
                .is_err()
        );
        let round = serde_json::to_string(&set).unwrap();
        assert_eq!(
            serde_json::from_str::<NntpServerUpdate>(&round).unwrap(),
            set
        );
    }

    #[test]
    fn debug_redacts_credentials() {
        let mut update = NntpServerUpdate::new("news.example");
        update.username = Some("user-sekrit".into());
        update.password = PasswordUpdate::Set("pass-sekrit".into());
        let text = format!("{update:?}");
        assert!(!text.contains("sekrit"));
        assert!(text.contains("REDACTED"));
        let settings = NntpServerSettings {
            enabled: true,
            host: "news.example".into(),
            port: 563,
            username: Some("user-sekrit".into()),
            has_password: true,
            max_connections: 8,
            connect_timeout_secs: 15,
            io_timeout_secs: 15,
            max_retries: 2,
        };
        assert!(!format!("{settings:?}").contains("sekrit"));
        let json = serde_json::to_string(&settings).unwrap();
        assert!(!json.contains("password\":\""));
    }
}
