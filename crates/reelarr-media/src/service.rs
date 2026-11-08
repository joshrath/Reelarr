//! Movie lookup and add flow.

use std::time::{SystemTime, UNIX_EPOCH};

use reelarr_types::{Movie, MovieId, MovieSearchResult};

use crate::metadata::{MetadataError, MetadataProvider, SearchQuery};
use crate::store::{MovieStore, StoreError};

/// Errors from [`MovieService::add_movie`].
#[derive(Debug, thiserror::Error)]
pub enum AddMovieError {
    #[error("movie is already in the library as {0:?}")]
    AlreadyExists(MovieId),
    #[error(transparent)]
    Metadata(#[from] MetadataError),
    #[error(transparent)]
    Store(StoreError),
}

impl From<StoreError> for AddMovieError {
    fn from(err: StoreError) -> Self {
        match err {
            StoreError::Duplicate { existing, .. } => Self::AlreadyExists(existing),
            other => Self::Store(other),
        }
    }
}

/// Searches a metadata source and adds movies to the library.
#[derive(Debug)]
pub struct MovieService<P, S> {
    provider: P,
    store: S,
}

impl<P: MetadataProvider, S: MovieStore> MovieService<P, S> {
    pub fn new(provider: P, store: S) -> Self {
        Self { provider, store }
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    /// Search the metadata source.
    pub async fn search(
        &self,
        query: &SearchQuery,
    ) -> Result<Vec<MovieSearchResult>, MetadataError> {
        self.provider.search_movies(query).await
    }

    /// Fetch full metadata for `tmdb_id` and add it to the library.
    ///
    /// Already-added movies are detected before calling the metadata source,
    /// so re-adding does not spend an API request.
    pub async fn add_movie(&self, tmdb_id: u64) -> Result<Movie, AddMovieError> {
        if let Some(existing) = self.store.find_by_tmdb_id(tmdb_id).await? {
            return Err(AddMovieError::AlreadyExists(existing.id));
        }
        let metadata = self.provider.movie_details(tmdb_id).await?;
        tracing::info!(tmdb_id, title = %metadata.title, "adding movie to library");
        Ok(self.store.insert(metadata, unix_now()).await?)
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
