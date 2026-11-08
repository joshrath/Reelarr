//! Metadata sources for movies.
//!
//! [`MetadataProvider`] is the abstraction the rest of Reelarr uses, so the
//! source can be swapped or mocked. [`tmdb::TmdbProvider`] is the default
//! implementation, backed by The Movie Database (TMDB) API.

pub mod tmdb;

use std::future::Future;

use reelarr_types::{MovieMetadata, MovieSearchResult};

/// A movie search request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchQuery {
    pub title: String,
    pub year: Option<u16>,
}

impl SearchQuery {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            year: None,
        }
    }

    pub fn with_year(mut self, year: u16) -> Self {
        self.year = Some(year);
        self
    }
}

/// Errors returned by a metadata provider.
#[derive(Debug, thiserror::Error)]
pub enum MetadataError {
    /// The API key or token was rejected.
    #[error("metadata source rejected the API credentials")]
    Unauthorized,
    /// The requested movie does not exist in the source.
    #[error("movie {0} was not found in the metadata source")]
    NotFound(u64),
    /// The source asked us to slow down.
    #[error("metadata source rate limit hit (retry after {retry_after_secs:?}s)")]
    RateLimited { retry_after_secs: Option<u64> },
    /// The source answered with an unexpected HTTP status.
    #[error("metadata source returned HTTP {0}")]
    Http(u16),
    /// The request could not be sent or the connection failed.
    #[error("could not reach metadata source: {0}")]
    Transport(String),
    /// The response body was not in the documented shape.
    #[error("could not decode metadata response: {0}")]
    Decode(String),
    /// The response lacked a field Reelarr requires, such as a title.
    #[error("metadata response is missing required field `{0}`")]
    MissingField(&'static str),
    /// The query was empty or otherwise invalid.
    #[error("invalid metadata query: {0}")]
    InvalidQuery(&'static str),
}

/// A source of movie metadata.
pub trait MetadataProvider: Send + Sync {
    /// Search movies by title and optional year.
    fn search_movies(
        &self,
        query: &SearchQuery,
    ) -> impl Future<Output = Result<Vec<MovieSearchResult>, MetadataError>> + Send;

    /// Fetch full metadata for one movie by its TMDB id.
    fn movie_details(
        &self,
        tmdb_id: u64,
    ) -> impl Future<Output = Result<MovieMetadata, MetadataError>> + Send;
}

/// Extract the year from a `YYYY-MM-DD` date. Empty or malformed dates give `None`.
pub(crate) fn year_from_date(date: Option<&str>) -> Option<u16> {
    let date = date?.trim();
    let year = date.get(0..4)?;
    if date.len() > 4 && date.as_bytes()[4] != b'-' {
        return None;
    }
    year.parse().ok().filter(|y| *y > 0)
}

/// Turn empty strings into `None`.
pub(crate) fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn year_parsing() {
        assert_eq!(year_from_date(Some("1999-03-31")), Some(1999));
        assert_eq!(year_from_date(Some("2024")), Some(2024));
        assert_eq!(year_from_date(Some("")), None);
        assert_eq!(year_from_date(Some("abcd-01-01")), None);
        assert_eq!(year_from_date(Some("20240101")), None);
        assert_eq!(year_from_date(None), None);
    }
}
