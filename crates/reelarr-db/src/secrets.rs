//! Swappable secret-at-rest policy. Initialization performs blocking filesystem I/O;
//! call it during startup, not for each request. No errors retain external values.
use std::{fmt, path::Path};

use base64::{Engine, engine::general_purpose::STANDARD};
use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305, XNonce,
    aead::{Aead, OsRng, Payload, rand_core::RngCore},
};
use reelarr_indexer::SecretString;
use zeroize::Zeroizing;

/// Errors deliberately contain no underlying I/O, environment or crypto values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SecretError {
    #[error("REELARR_SECRET_KEY must be standard base64 encoding exactly 32 bytes")]
    InvalidKey,
    #[error("cannot read or atomically create secret.key")]
    KeyFile,
    #[error("secret.key must be a regular file owned by the current user with mode 0600")]
    KeyFilePermissions,
    #[error("secret.key must contain exactly 32 raw bytes")]
    KeyFileLength,
    #[error("secure key files are unsupported on this platform; use REELARR_SECRET_KEY")]
    UnsupportedPlatform,
    #[error("operating system randomness is unavailable")]
    Randomness,
    #[error("secret encryption failed")]
    Encrypt,
    #[error("secret decryption failed (wrong key, context, or damaged ciphertext)")]
    Decrypt,
    #[error("unsupported or malformed secret ciphertext")]
    InvalidCiphertext,
}

/// Internal storage integration only. `context` binds a value to its record/field.
/// Implementations must not log/retain plaintext in diagnostics. The default can be
/// replaced without changing IndexerStore or its callers (policy pending approval).
pub trait SecretCipher: Send + Sync {
    fn encrypt(&self, context: &[u8], secret: &str) -> Result<Vec<u8>, SecretError>;
    fn decrypt(&self, context: &[u8], ciphertext: &[u8]) -> Result<SecretString, SecretError>;
}

/// RustCrypto XChaCha20-Poly1305, random 24-byte nonce for every encryption.
/// Format v1: ASCII `RSC`, version byte 1, nonce (24 bytes), ciphertext + tag.
/// The header and caller context are authenticated as associated data.
pub struct XChaChaSecretCipher(XChaCha20Poly1305);
const HEADER: &[u8; 4] = b"RSC\x01";
const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;

impl fmt::Debug for XChaChaSecretCipher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("XChaChaSecretCipher([REDACTED])")
    }
}

impl XChaChaSecretCipher {
    /// Consumes a key; temporary key material is zeroed after cipher initialization.
    pub fn new(key: [u8; 32]) -> Self {
        let key = Zeroizing::new(key);
        Self(XChaCha20Poly1305::new((&*key).into()))
    }

    pub fn from_base64_key(encoded: &str) -> Result<Self, SecretError> {
        let decoded = Zeroizing::new(
            STANDARD
                .decode(encoded)
                .map_err(|_| SecretError::InvalidKey)?,
        );
        let key: [u8; 32] = decoded
            .as_slice()
            .try_into()
            .map_err(|_| SecretError::InvalidKey)?;
        Ok(Self::new(key))
    }

    /// An explicitly set environment key always wins; invalid values never silently
    /// fall back to a file. The directory must already exist and be private/writable.
    pub fn from_env_or_data_dir(data_dir: &Path) -> Result<Self, SecretError> {
        match std::env::var("REELARR_SECRET_KEY") {
            Ok(value) => Self::from_base64_key(&Zeroizing::new(value)),
            Err(std::env::VarError::NotPresent) => Self::from_key_file(data_dir),
            Err(std::env::VarError::NotUnicode(_)) => Err(SecretError::InvalidKey),
        }
    }

    /// Uses REELARR_DATA_DIR, default /config. Does not log either environment value.
    pub fn from_env() -> Result<Self, SecretError> {
        let data_dir = std::env::var_os("REELARR_DATA_DIR").unwrap_or_else(|| "/config".into());
        Self::from_env_or_data_dir(Path::new(&data_dir))
    }

    /// Loads/generates a raw 32-byte key file. A private, fully-written temporary file
    /// is atomically published with no-clobber semantics. Concurrent initializers
    /// use the winner's key; an existing key is never truncated, replaced or chmod'd.
    #[cfg(unix)]
    pub fn from_key_file(data_dir: &Path) -> Result<Self, SecretError> {
        use std::{fs, io::Write, os::unix::fs::PermissionsExt};
        let path = data_dir.join("secret.key");
        match read_key_file(&path) {
            Ok(key) => return Ok(Self::new(*key)),
            Err(KeyReadError::Other(error)) => return Err(error),
            Err(KeyReadError::Missing) => {}
        }
        let mut key = Zeroizing::new([0u8; 32]);
        OsRng
            .try_fill_bytes(&mut *key)
            .map_err(|_| SecretError::Randomness)?;
        let mut file =
            tempfile::NamedTempFile::new_in(data_dir).map_err(|_| SecretError::KeyFile)?;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|_| SecretError::KeyFile)?;
        file.write_all(&*key).map_err(|_| SecretError::KeyFile)?;
        file.as_file()
            .sync_all()
            .map_err(|_| SecretError::KeyFile)?;
        match file.persist_noclobber(&path) {
            Ok(_) => {
                fs::File::open(data_dir)
                    .and_then(|dir| dir.sync_all())
                    .map_err(|_| SecretError::KeyFile)?;
                Ok(Self::new(*key))
            }
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                read_key_file(&path)
                    .map(|key| Self::new(*key))
                    .map_err(|error| match error {
                        KeyReadError::Missing => SecretError::KeyFile,
                        KeyReadError::Other(error) => error,
                    })
            }
            Err(_) => Err(SecretError::KeyFile),
        }
    }

    #[cfg(not(unix))]
    pub fn from_key_file(_data_dir: &Path) -> Result<Self, SecretError> {
        Err(SecretError::UnsupportedPlatform)
    }
}

#[cfg(unix)]
enum KeyReadError {
    Missing,
    Other(SecretError),
}

#[cfg(unix)]
fn read_key_file(path: &Path) -> Result<Zeroizing<[u8; 32]>, KeyReadError> {
    use rustix::fs::{Mode, OFlags, open};
    use std::{fs::File, io::Read, os::unix::fs::MetadataExt};
    let fd = open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|error| {
        if error == rustix::io::Errno::NOENT {
            KeyReadError::Missing
        } else {
            KeyReadError::Other(SecretError::KeyFile)
        }
    })?;
    let mut file = File::from(fd);
    let metadata = file
        .metadata()
        .map_err(|_| KeyReadError::Other(SecretError::KeyFile))?;
    if !metadata.is_file()
        || metadata.mode() & 0o7777 != 0o600
        || metadata.uid() != rustix::process::geteuid().as_raw()
    {
        return Err(KeyReadError::Other(SecretError::KeyFilePermissions));
    }
    let mut key = Zeroizing::new([0u8; 32]);
    file.read_exact(&mut *key)
        .map_err(|_| KeyReadError::Other(SecretError::KeyFileLength))?;
    let mut extra = [0];
    if file
        .read(&mut extra)
        .map_err(|_| KeyReadError::Other(SecretError::KeyFile))?
        != 0
    {
        return Err(KeyReadError::Other(SecretError::KeyFileLength));
    }
    Ok(key)
}

fn associated_data(context: &[u8]) -> Vec<u8> {
    [HEADER.as_slice(), context].concat()
}

impl SecretCipher for XChaChaSecretCipher {
    fn encrypt(&self, context: &[u8], secret: &str) -> Result<Vec<u8>, SecretError> {
        let mut nonce = [0u8; NONCE_LEN];
        OsRng
            .try_fill_bytes(&mut nonce)
            .map_err(|_| SecretError::Randomness)?;
        let encrypted = self
            .0
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: secret.as_bytes(),
                    aad: &associated_data(context),
                },
            )
            .map_err(|_| SecretError::Encrypt)?;
        Ok([HEADER.as_slice(), &nonce, &encrypted].concat())
    }

    fn decrypt(&self, context: &[u8], ciphertext: &[u8]) -> Result<SecretString, SecretError> {
        if ciphertext.len() < HEADER.len() + NONCE_LEN + TAG_LEN || !ciphertext.starts_with(HEADER)
        {
            return Err(SecretError::InvalidCiphertext);
        }
        let nonce = XNonce::from_slice(&ciphertext[HEADER.len()..HEADER.len() + NONCE_LEN]);
        let plaintext = Zeroizing::new(
            self.0
                .decrypt(
                    nonce,
                    Payload {
                        msg: &ciphertext[HEADER.len() + NONCE_LEN..],
                        aad: &associated_data(context),
                    },
                )
                .map_err(|_| SecretError::Decrypt)?,
        );
        let value = std::str::from_utf8(&plaintext).map_err(|_| SecretError::Decrypt)?;
        Ok(SecretString::new(value))
    }
}
