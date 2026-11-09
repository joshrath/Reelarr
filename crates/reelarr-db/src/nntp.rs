//! NNTP provider settings (#76 PR 3) in the shared SQLite database.
//!
//! One provider row (migration 0009). Non-secret settings are versioned JSON;
//! the AUTHINFO password is encrypted with [`SecretCipher`] (the policy decided
//! in #78: XChaCha20-Poly1305 keyed by `REELARR_SECRET_KEY` or the 0600
//! `secret.key` in the data directory), bound to this record/field as
//! associated data. Errors carry only static text: no SQL, bind values,
//! host-supplied text or credentials. SQL statements are string literals.
//!
//! API-facing reads ([`SqliteNntpStore::get`]) never select or decrypt the
//! password and report only `has_password`. [`SqliteNntpStore::load`] and
//! [`SqliteNntpStore::resolve`] decrypt for trusted internal pool construction.
//!
//! Password updates are keep / replace / clear. `Keep` is refused when the host
//! changes, so a stored password is never sent to a different server unless the
//! administrator re-enters it.
use std::{fmt, sync::Arc, time::Duration};

use reelarr_nzb::nntp::{Credentials, Secret, ServerConfig, validate_credential};
use reelarr_types::nntp::{
    NNTP_MAX_CONNECTIONS, NNTP_MAX_RETRIES, NNTP_MAX_TIMEOUT_SECS, NNTP_PLAINTEXT_PORT,
    NntpServerSettings, NntpServerUpdate, PasswordUpdate,
};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool, sqlite::SqliteRow};
use zeroize::Zeroizing;

use crate::{Database, secrets::SecretCipher};

const PASSWORD_CONTEXT: &[u8] = b"reelarr:nntp_server:password:1";
const MAX_HOST_BYTES: usize = 253;

/// Secret-free failure classification for settings writes and reads.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NntpSettingsError {
    /// A field is out of range or malformed. The text never contains input values.
    #[error("invalid NNTP settings: {0}")]
    Invalid(&'static str),
    /// `Keep` was requested but no stored password may be reused (none stored,
    /// or the host changed). The administrator must enter the password.
    #[error("enter the password again: {0}")]
    PasswordRequired(&'static str),
    /// No provider is configured.
    #[error("no NNTP server is configured")]
    NotConfigured,
    /// Database, encryption or decryption failure (details deliberately dropped).
    #[error("NNTP settings storage failed")]
    Store,
}

/// The one configured provider, ready for `reelarr_nzb::nntp::Pool`.
/// Debug is safe: credentials inside [`ServerConfig`] are redacted.
#[derive(Clone, Debug)]
pub struct NntpServer {
    pub enabled: bool,
    pub config: ServerConfig,
}

/// SQLite adapter sharing the application's pool.
#[derive(Clone)]
pub struct SqliteNntpStore {
    pool: SqlitePool,
    cipher: Arc<dyn SecretCipher>,
}
impl fmt::Debug for SqliteNntpStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SqliteNntpStore { cipher: [REDACTED], .. }")
    }
}

// Versioned non-secret payload. Never contains the password.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    version: u8,
    enabled: bool,
    host: String,
    port: u16,
    username: Option<String>,
    max_connections: u16,
    connect_timeout_secs: u32,
    io_timeout_secs: u32,
    max_retries: u8,
}

enum Password {
    None,
    Stored(Vec<u8>),
    New(Zeroizing<String>),
}

fn backend<E>(_: E) -> NntpSettingsError {
    NntpSettingsError::Store
}

impl Settings {
    /// Field validation shared by save and test-connection. Mirrors the NNTP
    /// crate's own rules (via `ServerConfig::validate`) plus product bounds.
    fn from_update(update: &NntpServerUpdate) -> Result<Self, NntpSettingsError> {
        let invalid = NntpSettingsError::Invalid;
        let host = update.host.trim();
        if host.is_empty() || host.len() > MAX_HOST_BYTES {
            return Err(invalid("host must be 1–253 characters"));
        }
        if update.port == 0 {
            return Err(invalid("port must be 1–65535"));
        }
        if update.port == NNTP_PLAINTEXT_PORT {
            return Err(invalid(
                "port 119 is plaintext NNTP; Reelarr requires implicit TLS (usually port 563)",
            ));
        }
        if !(1..=NNTP_MAX_CONNECTIONS).contains(&update.max_connections) {
            return Err(invalid("connections must be 1–100"));
        }
        for secs in [update.connect_timeout_secs, update.io_timeout_secs] {
            if !(1..=NNTP_MAX_TIMEOUT_SECS).contains(&secs) {
                return Err(invalid("timeouts must be 1–300 seconds"));
            }
        }
        if update.max_retries > NNTP_MAX_RETRIES {
            return Err(invalid("retries must be 0–10"));
        }
        if let Some(username) = &update.username {
            validate_credential(username).map_err(|_| {
                invalid("username must be 1–496 printable ASCII characters without spaces")
            })?;
        }
        let settings = Self {
            version: 1,
            enabled: update.enabled,
            host: host.to_ascii_lowercase(),
            port: update.port,
            username: update.username.clone(),
            max_connections: update.max_connections,
            connect_timeout_secs: update.connect_timeout_secs,
            io_timeout_secs: update.io_timeout_secs,
            max_retries: update.max_retries,
        };
        settings
            .server_config(None)
            .validate()
            .map_err(|_| invalid("host must be a valid DNS name or IP address"))?;
        Ok(settings)
    }

    fn server_config(&self, credentials: Option<Credentials>) -> ServerConfig {
        let mut config = ServerConfig::new(self.host.clone());
        config.port = self.port;
        config.credentials = credentials;
        config.max_connections = usize::from(self.max_connections);
        config.timeouts.connect = Duration::from_secs(self.connect_timeout_secs.into());
        config.timeouts.io = Duration::from_secs(self.io_timeout_secs.into());
        config.retry.max_retries = self.max_retries;
        config
    }

    fn summary(&self, has_password: bool) -> NntpServerSettings {
        NntpServerSettings {
            enabled: self.enabled,
            host: self.host.clone(),
            port: self.port,
            username: self.username.clone(),
            has_password,
            max_connections: self.max_connections,
            connect_timeout_secs: self.connect_timeout_secs,
            io_timeout_secs: self.io_timeout_secs,
            max_retries: self.max_retries,
        }
    }
}

/// Apply an update to the stored row (if any): validate, then decide what
/// happens to the password. No I/O, no decryption.
fn plan(
    update: &NntpServerUpdate,
    existing: Option<(Settings, Option<Vec<u8>>)>,
) -> Result<(Settings, Password), NntpSettingsError> {
    let settings = Settings::from_update(update)?;
    let password = match &update.password {
        PasswordUpdate::Clear => Password::None,
        PasswordUpdate::Set(value) => {
            validate_credential(value).map_err(|_| {
                NntpSettingsError::Invalid(
                    "password must be 1–496 printable ASCII characters without spaces",
                )
            })?;
            Password::New(Zeroizing::new(value.clone()))
        }
        PasswordUpdate::Keep => match existing {
            Some((old, Some(ciphertext))) => {
                if old.host != settings.host {
                    return Err(NntpSettingsError::PasswordRequired(
                        "the host changed, so the saved password is not reused",
                    ));
                }
                Password::Stored(ciphertext)
            }
            _ => Password::None,
        },
    };
    match (&settings.username, &password) {
        (Some(_), Password::None) if matches!(update.password, PasswordUpdate::Keep) => Err(
            NntpSettingsError::PasswordRequired("a username needs a password"),
        ),
        (Some(_), Password::None) => Err(NntpSettingsError::Invalid(
            "a username needs a password; clear both to connect without login",
        )),
        (None, Password::Stored(_) | Password::New(_)) => {
            Err(NntpSettingsError::Invalid("a password needs a username"))
        }
        _ => Ok((settings, password)),
    }
}

fn decode(row: &SqliteRow) -> Result<(Settings, Option<Vec<u8>>), NntpSettingsError> {
    let text: String = row.try_get("settings_json").map_err(backend)?;
    let settings: Settings = serde_json::from_str(&text).map_err(backend)?;
    if settings.version != 1 {
        return Err(NntpSettingsError::Store);
    }
    let ciphertext: Option<Vec<u8>> = row.try_get("password_ciphertext").map_err(backend)?;
    Ok((settings, ciphertext))
}

impl SqliteNntpStore {
    pub fn new(db: &Database, cipher: Arc<dyn SecretCipher>) -> Self {
        Self {
            pool: db.pool().clone(),
            cipher,
        }
    }

    /// API-safe read. Never selects or decrypts the password.
    pub async fn get(&self) -> Result<Option<NntpServerSettings>, NntpSettingsError> {
        let row = sqlx::query(
            "SELECT settings_json, password_ciphertext IS NOT NULL AS has_password \
             FROM nntp_server WHERE id = 1",
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        let Some(row) = row else { return Ok(None) };
        let text: String = row.try_get("settings_json").map_err(backend)?;
        let settings: Settings = serde_json::from_str(&text).map_err(backend)?;
        if settings.version != 1 {
            return Err(NntpSettingsError::Store);
        }
        let has_password: bool = row.try_get("has_password").map_err(backend)?;
        Ok(Some(settings.summary(has_password)))
    }

    /// Internal read with the decrypted password, for building a pool.
    /// Corrupt rows, a wrong key or a moved ciphertext fail closed as `Store`.
    pub async fn load(&self) -> Result<Option<NntpServer>, NntpSettingsError> {
        let row =
            sqlx::query("SELECT settings_json, password_ciphertext FROM nntp_server WHERE id = 1")
                .fetch_optional(&self.pool)
                .await
                .map_err(backend)?;
        let Some(row) = row else { return Ok(None) };
        let (settings, ciphertext) = decode(&row)?;
        let password = match ciphertext {
            Some(ciphertext) => Password::Stored(ciphertext),
            None => Password::None,
        };
        self.server(settings, password).map(Some)
    }

    /// Candidate settings for test-connection: the update applied on top of
    /// the stored row exactly as [`save`](Self::save) would, without writing.
    pub async fn resolve(
        &self,
        update: &NntpServerUpdate,
    ) -> Result<NntpServer, NntpSettingsError> {
        let row =
            sqlx::query("SELECT settings_json, password_ciphertext FROM nntp_server WHERE id = 1")
                .fetch_optional(&self.pool)
                .await
                .map_err(backend)?;
        let existing = row.as_ref().map(decode).transpose()?;
        let (settings, password) = plan(update, existing)?;
        self.server(settings, password)
    }

    /// Validate, encrypt and upsert in one `BEGIN IMMEDIATE` transaction, so a
    /// concurrent update cannot interleave between reading the stored password
    /// and writing the row. Nothing is written if validation or encryption fails.
    pub async fn save(
        &self,
        update: &NntpServerUpdate,
    ) -> Result<NntpServerSettings, NntpSettingsError> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(backend)?;
        let row =
            sqlx::query("SELECT settings_json, password_ciphertext FROM nntp_server WHERE id = 1")
                .fetch_optional(&mut *tx)
                .await
                .map_err(backend)?;
        let existing = row.as_ref().map(decode).transpose()?;
        let (settings, password) = plan(update, existing)?;
        let ciphertext = match password {
            Password::None => None,
            Password::Stored(ciphertext) => Some(ciphertext),
            Password::New(value) => Some(
                self.cipher
                    .encrypt(PASSWORD_CONTEXT, &value)
                    .map_err(backend)?,
            ),
        };
        let json = serde_json::to_string(&settings).map_err(backend)?;
        let has_password = ciphertext.is_some();
        sqlx::query(
            "INSERT INTO nntp_server (id, settings_json, password_ciphertext) VALUES (1, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET settings_json = excluded.settings_json, \
             password_ciphertext = excluded.password_ciphertext",
        )
        .bind(json)
        .bind(ciphertext)
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
        tx.commit().await.map_err(backend)?;
        Ok(settings.summary(has_password))
    }

    /// Remove the provider and its encrypted password. Idempotent.
    pub async fn clear(&self) -> Result<bool, NntpSettingsError> {
        Ok(sqlx::query("DELETE FROM nntp_server WHERE id = 1")
            .execute(&self.pool)
            .await
            .map_err(backend)?
            .rows_affected()
            > 0)
    }

    fn server(
        &self,
        settings: Settings,
        password: Password,
    ) -> Result<NntpServer, NntpSettingsError> {
        let password = match password {
            Password::None => None,
            Password::New(value) => Some(Secret::new(value.as_str())),
            Password::Stored(ciphertext) => {
                let plain = self
                    .cipher
                    .decrypt(PASSWORD_CONTEXT, &ciphertext)
                    .map_err(backend)?;
                Some(Secret::new(plain.expose()))
            }
        };
        let credentials = match (&settings.username, password) {
            (Some(username), Some(password)) => Some(Credentials {
                username: Secret::new(username.as_str()),
                password,
            }),
            (None, None) => None,
            // A row written out of band; never connect with half a login.
            _ => return Err(NntpSettingsError::Store),
        };
        let config = settings.server_config(credentials);
        // Out-of-band or corrupt values must not bypass the NNTP crate's rules.
        config.validate().map_err(backend)?;
        Ok(NntpServer {
            enabled: settings.enabled,
            config,
        })
    }
}
