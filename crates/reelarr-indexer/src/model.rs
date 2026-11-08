//! Public protocol types. Credentials deliberately have no serialization implementation.
use reqwest::Url;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    time::Duration,
};

/// A credential or capability URL. Debug and Display never reveal its contents.
/// Deliberately not serializable: persistence must explicitly integrate secret storage.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);
impl SecretString {
    /// Wrap a secret without copying it into diagnostics.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    /// Explicit exposure for request construction / secret-store integration only.
    pub fn expose(&self) -> &str {
        &self.0
    }
}
impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}
impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

/// Administrator-managed indexer settings. No database/state-file policy is embedded.
#[derive(Clone)]
pub struct IndexerConfig {
    pub name: String,
    pub base_url: Url,
    /// Origin-relative API path, e.g. `/api` or `/newznab/api`.
    pub api_path: String,
    pub api_key: SecretString,
    pub categories: Vec<u32>,
    pub enabled: bool,
    /// Explicit trust override for local/private HTTP services (not automatic).
    pub allow_http: bool,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub minimum_interval: Duration,
    pub caps_ttl: Duration,
    pub max_response_bytes: usize,
    pub max_pages: usize,
    pub max_results: usize,
}
impl fmt::Debug for IndexerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Even invalid configs can contain secrets in names, userinfo, paths or queries.
        f.debug_struct("IndexerConfig")
            .field("endpoint", &"[REDACTED]")
            .field("api_key", &self.api_key)
            .field("enabled", &self.enabled)
            .finish_non_exhaustive()
    }
}
impl IndexerConfig {
    /// Safe defaults: HTTPS, 4 MiB responses, 20 pages / 1000 unique results.
    pub fn new(name: impl Into<String>, base_url: Url, api_key: SecretString) -> Self {
        Self {
            name: name.into(),
            base_url,
            api_path: "/api".into(),
            api_key,
            categories: vec![2000],
            enabled: true,
            allow_http: false,
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(30),
            minimum_interval: Duration::from_secs(1),
            caps_ttl: Duration::from_secs(3600),
            max_response_bytes: 4 * 1024 * 1024,
            max_pages: 20,
            max_results: 1000,
        }
    }
    /// Validate without putting rejected values into an error message.
    pub fn validate(&self) -> Result<(), IndexerError> {
        let url = &self.base_url;
        if self.name.trim().is_empty()
            || self.name.len() > 256
            || self.name.chars().any(char::is_control)
            || self.api_key.expose().trim().is_empty()
            || self.api_key.expose().len() > 4096
            || self.api_key.expose().chars().any(char::is_control)
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !(url.scheme() == "https" || (self.allow_http && url.scheme() == "http"))
            || !self.api_path.starts_with('/')
            || self.api_path.starts_with("//")
            || self.api_path.contains(['?', '#', '\\'])
            || self.api_path.chars().any(char::is_control)
            || self.categories.contains(&0)
            || self.categories.len() > 100
            || self.connect_timeout.is_zero()
            || self.request_timeout.is_zero()
            || self.connect_timeout > self.request_timeout
            || self.request_timeout > Duration::from_secs(300)
            || self.minimum_interval > Duration::from_secs(3600)
            || self.caps_ttl > Duration::from_secs(86400)
            || !(1..=16 * 1024 * 1024).contains(&self.max_response_bytes)
            || !(1..=100).contains(&self.max_pages)
            || !(1..=10000).contains(&self.max_results)
        {
            return Err(IndexerError::InvalidConfig);
        }
        let endpoint = url
            .join(&self.api_path)
            .map_err(|_| IndexerError::InvalidConfig)?;
        if endpoint.origin() != url.origin() {
            return Err(IndexerError::InvalidConfig);
        }
        Ok(())
    }
}

/// Advertised search mode and parameter names (not guessed extensions).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchMode {
    pub available: bool,
    pub supported_params: BTreeSet<String>,
}
/// A flattened category; parent IDs retain the hierarchy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Category {
    pub id: u32,
    pub name: String,
    pub parent_id: Option<u32>,
}
/// Cached `t=caps` document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    pub search: SearchMode,
    pub movie: SearchMode,
    pub categories: Vec<Category>,
    pub default_limit: u32,
    pub max_limit: u32,
}
/// Metadata-to-indexer boundary: IDs are not tied to the metadata provider's DTOs.
#[derive(Clone, Default)]
pub struct MovieQuery {
    pub title: String,
    pub year: Option<u16>,
    /// With or without `tt`; leading zeroes are preserved.
    pub imdb_id: Option<String>,
    pub tmdb_id: Option<u64>,
    pub limit: Option<u32>,
    pub offset: u64,
}
/// Protocol candidates, not quality decisions. Provider strings are untrusted.
#[derive(Clone)]
pub struct Release {
    pub title: String,
    pub guid: String,
    /// Same-origin HTTP(S) enclosure. Treat it as a credential, not a loggable URL.
    pub download_url: SecretString,
    pub size: Option<u64>,
    /// Original RFC 822/2822 RSS timestamp, with its supplied zone retained.
    pub pub_date: Option<String>,
    pub categories: Vec<u32>,
    pub imdb_id: Option<String>,
    pub tmdb_id: Option<u64>,
    pub grabs: Option<u64>,
    pub files: Option<u64>,
    pub password: Option<u8>,
    pub year: Option<u16>,
    /// Unknown attributes retained for future selection; repeated values preserved.
    pub attributes: BTreeMap<String, Vec<String>>,
}
impl fmt::Debug for Release {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Release")
            .field("provider_fields", &"[REDACTED]")
            .field("size", &self.size)
            .field("categories", &self.categories)
            .finish_non_exhaustive()
    }
}
/// A bounded search; `truncated` distinguishes limits/broken pagination from exhaustion.
#[derive(Debug, Clone)]
pub struct SearchResults {
    pub releases: Vec<Release>,
    pub truncated: bool,
}

/// Typed errors contain no URLs, raw XML, credentials or provider descriptions.
/// Newznab descriptions are parsed but deliberately not exposed: servers can echo keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexerError {
    InvalidConfig,
    InvalidQuery,
    Disabled,
    BadApiKey,
    AccountSuspended,
    Unauthorized,
    InvalidParameters,
    Unsupported,
    NotFound,
    ApiDisabled,
    /// 429 HTTP and common provider quota extensions (e.g. 500) use this variant.
    RateLimited {
        retry_after: Duration,
    },
    Api {
        code: u32,
    },
    Http {
        status: u16,
    },
    Timeout,
    Transport,
    MalformedXml,
    ResponseTooLarge,
    UnsafeDownloadUrl,
    Store,
}
impl fmt::Display for IndexerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "indexer error: {self:?}")
    }
}
impl std::error::Error for IndexerError {}
impl IndexerError {
    pub(crate) fn api(code: u32) -> Self {
        match code {
            100 => Self::BadApiKey,
            101 => Self::AccountSuspended,
            102 => Self::Unauthorized,
            200 | 201 => Self::InvalidParameters,
            202 | 203 => Self::Unsupported,
            300 => Self::NotFound,
            910 => Self::ApiDisabled,
            500 => Self::RateLimited {
                retry_after: Duration::from_secs(60),
            },
            _ => Self::Api { code },
        }
    }
    pub(crate) fn transport(error: reqwest::Error) -> Self {
        // Never retain/display reqwest::Error: it can include an apikey-bearing URL.
        if error.is_timeout() {
            Self::Timeout
        } else {
            Self::Transport
        }
    }
}
