//! TMDB (The Movie Database) metadata provider.
//!
//! Uses the documented v3 endpoints `GET /3/search/movie` and
//! `GET /3/movie/{movie_id}` (<https://developer.themoviedb.org/reference>).
//!
//! Each Reelarr install supplies its own TMDB credential through configuration
//! or the `REELARR_TMDB_API_KEY` environment variable. Reelarr never ships a
//! key. See `crates/reelarr-media/README.md` for TMDB's terms and attribution
//! requirements.

use std::time::Duration;

use reelarr_types::{Artwork, ExternalIds, MovieMetadata, MovieSearchResult};
use reqwest::{StatusCode, header};
use serde::Deserialize;

use super::{MetadataError, MetadataProvider, SearchQuery, non_empty, year_from_date};

/// Default base URL of the TMDB API.
pub const DEFAULT_API_BASE: &str = "https://api.themoviedb.org";
/// Default base URL for TMDB images. TMDB's `/3/configuration` endpoint
/// reports the current value; this is the documented default.
pub const DEFAULT_IMAGE_BASE: &str = "https://image.tmdb.org/t/p/";
/// Environment variable holding the TMDB credential.
pub const API_KEY_ENV: &str = "REELARR_TMDB_API_KEY";

/// How Reelarr authenticates to TMDB.
#[derive(Clone, PartialEq, Eq)]
pub enum TmdbCredential {
    /// A v4 "API Read Access Token", sent as a bearer token.
    ReadAccessToken(String),
    /// A v3 API key, sent as the `api_key` query parameter.
    ApiKey(String),
}

impl TmdbCredential {
    /// Pick the credential kind from the value. Read access tokens are JWTs,
    /// which start with `eyJ`; anything else is treated as a v3 API key.
    pub fn detect(value: impl Into<String>) -> Self {
        let value = value.into().trim().to_owned();
        if value.starts_with("eyJ") {
            Self::ReadAccessToken(value)
        } else {
            Self::ApiKey(value)
        }
    }
}

impl std::fmt::Debug for TmdbCredential {
    // Never print the secret.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReadAccessToken(_) => f.write_str("ReadAccessToken(<redacted>)"),
            Self::ApiKey(_) => f.write_str("ApiKey(<redacted>)"),
        }
    }
}

/// Settings for [`TmdbProvider`].
#[derive(Debug, Clone)]
pub struct TmdbConfig {
    pub credential: TmdbCredential,
    pub api_base: String,
    pub image_base: String,
    /// ISO 639-1 language with optional region, e.g. `en-US`.
    pub language: String,
    pub timeout: Duration,
}

impl TmdbConfig {
    pub fn new(credential: TmdbCredential) -> Self {
        Self {
            credential,
            api_base: DEFAULT_API_BASE.into(),
            image_base: DEFAULT_IMAGE_BASE.into(),
            language: "en-US".into(),
            timeout: Duration::from_secs(15),
        }
    }

    /// Build a config from `REELARR_TMDB_API_KEY`. Returns `None` when it is
    /// unset or empty.
    pub fn from_env() -> Option<Self> {
        let value = std::env::var(API_KEY_ENV).ok()?;
        if value.trim().is_empty() {
            return None;
        }
        Some(Self::new(TmdbCredential::detect(value)))
    }
}

/// Metadata provider backed by the TMDB API.
#[derive(Debug, Clone)]
pub struct TmdbProvider {
    config: TmdbConfig,
    http: reqwest::Client,
}

impl TmdbProvider {
    pub fn new(config: TmdbConfig) -> Result<Self, MetadataError> {
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            .user_agent(concat!("Reelarr/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| MetadataError::Transport(e.to_string()))?;
        Ok(Self { config, http })
    }

    /// Full URL for an artwork path such as `/abc.jpg` at a TMDB size such as
    /// `w500` or `original`.
    pub fn image_url(&self, size: &str, path: &str) -> String {
        image_url(&self.config.image_base, size, path)
    }

    async fn get<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        params: &[(&str, String)],
        tmdb_id: Option<u64>,
    ) -> Result<T, MetadataError> {
        let url = format!("{}{}", self.config.api_base.trim_end_matches('/'), path);
        let mut request = self
            .http
            .get(url)
            .header(header::ACCEPT, "application/json")
            .query(&[("language", self.config.language.as_str())])
            .query(params);
        request = match &self.config.credential {
            TmdbCredential::ReadAccessToken(token) => request.bearer_auth(token),
            TmdbCredential::ApiKey(key) => request.query(&[("api_key", key.as_str())]),
        };

        let response = request
            .send()
            .await
            .map_err(|e| MetadataError::Transport(e.without_url().to_string()))?;

        let status = response.status();
        match status {
            s if s.is_success() => {}
            StatusCode::UNAUTHORIZED => return Err(MetadataError::Unauthorized),
            StatusCode::NOT_FOUND => {
                return Err(match tmdb_id {
                    Some(id) => MetadataError::NotFound(id),
                    None => MetadataError::Http(404),
                });
            }
            StatusCode::TOO_MANY_REQUESTS => {
                let retry_after_secs = response
                    .headers()
                    .get(header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse().ok());
                return Err(MetadataError::RateLimited { retry_after_secs });
            }
            other => return Err(MetadataError::Http(other.as_u16())),
        }

        let bytes = response
            .bytes()
            .await
            .map_err(|e| MetadataError::Transport(e.without_url().to_string()))?;
        serde_json::from_slice(&bytes).map_err(|e| MetadataError::Decode(e.to_string()))
    }
}

/// Join a TMDB image base, size and path.
pub fn image_url(image_base: &str, size: &str, path: &str) -> String {
    format!(
        "{}/{}/{}",
        image_base.trim_end_matches('/'),
        size.trim_matches('/'),
        path.trim_start_matches('/')
    )
}

impl MetadataProvider for TmdbProvider {
    async fn search_movies(
        &self,
        query: &SearchQuery,
    ) -> Result<Vec<MovieSearchResult>, MetadataError> {
        let title = query.title.trim();
        if title.is_empty() {
            return Err(MetadataError::InvalidQuery("title must not be empty"));
        }
        let mut params = vec![
            ("query", title.to_owned()),
            ("include_adult", "false".to_owned()),
        ];
        if let Some(year) = query.year {
            params.push(("year", year.to_string()));
        }
        let page: SearchPage = self.get("/3/search/movie", &params, None).await?;
        Ok(page.results.into_iter().filter_map(map_search).collect())
    }

    async fn movie_details(&self, tmdb_id: u64) -> Result<MovieMetadata, MetadataError> {
        let details: MovieDetails = self
            .get(&format!("/3/movie/{tmdb_id}"), &[], Some(tmdb_id))
            .await?;
        map_details(details)
    }
}

// Response shapes, limited to the fields Reelarr uses. Every field is
// optional so that missing data is handled explicitly in the mappers.

#[derive(Debug, Deserialize)]
struct SearchPage {
    #[serde(default)]
    results: Vec<SearchItem>,
}

#[derive(Debug, Deserialize)]
struct SearchItem {
    id: Option<u64>,
    title: Option<String>,
    original_title: Option<String>,
    release_date: Option<String>,
    overview: Option<String>,
    poster_path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MovieDetails {
    id: Option<u64>,
    imdb_id: Option<String>,
    title: Option<String>,
    original_title: Option<String>,
    release_date: Option<String>,
    overview: Option<String>,
    runtime: Option<u32>,
    poster_path: Option<String>,
    backdrop_path: Option<String>,
}

/// Results without an id or title cannot be added, so they are skipped.
fn map_search(item: SearchItem) -> Option<MovieSearchResult> {
    let title = non_empty(item.title)?;
    Some(MovieSearchResult {
        tmdb_id: item.id?,
        title,
        original_title: non_empty(item.original_title),
        year: year_from_date(item.release_date.as_deref()),
        overview: non_empty(item.overview),
        poster_path: non_empty(item.poster_path),
    })
}

fn map_details(details: MovieDetails) -> Result<MovieMetadata, MetadataError> {
    let tmdb_id = details.id.ok_or(MetadataError::MissingField("id"))?;
    let title = non_empty(details.title).ok_or(MetadataError::MissingField("title"))?;
    Ok(MovieMetadata {
        external_ids: ExternalIds {
            tmdb_id,
            imdb_id: non_empty(details.imdb_id),
        },
        title,
        original_title: non_empty(details.original_title),
        year: year_from_date(details.release_date.as_deref()),
        overview: non_empty(details.overview),
        runtime_minutes: details.runtime.filter(|r| *r > 0),
        artwork: Artwork {
            poster_path: non_empty(details.poster_path),
            backdrop_path: non_empty(details.backdrop_path),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_detection_and_redaction() {
        let token = TmdbCredential::detect("eyJhbGciOiJIUzI1NiJ9.x.y");
        assert!(matches!(token, TmdbCredential::ReadAccessToken(_)));
        let key = TmdbCredential::detect(" 0123abcd ");
        assert_eq!(key, TmdbCredential::ApiKey("0123abcd".into()));
        assert!(!format!("{key:?}").contains("0123abcd"));
    }

    #[test]
    fn builds_image_urls() {
        assert_eq!(
            image_url(DEFAULT_IMAGE_BASE, "w500", "/poster.jpg"),
            "https://image.tmdb.org/t/p/w500/poster.jpg"
        );
    }

    #[test]
    fn details_without_title_is_an_error() {
        let details = MovieDetails {
            id: Some(1),
            imdb_id: None,
            title: Some("  ".into()),
            original_title: None,
            release_date: None,
            overview: None,
            runtime: None,
            poster_path: None,
            backdrop_path: None,
        };
        assert!(matches!(
            map_details(details),
            Err(MetadataError::MissingField("title"))
        ));
    }
}
