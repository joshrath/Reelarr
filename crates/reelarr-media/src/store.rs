//! Movie storage abstraction.
//!
//! The real implementation (sqlx + SQLite) lives in `reelarr-db`, which owns
//! the single Reelarr database and its migrations. This crate only defines
//! the trait and an in-memory implementation used in tests and examples.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Mutex;

use reelarr_types::{Movie, MovieId, MovieMetadata};

/// Errors returned by a [`MovieStore`].
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// A movie with this TMDB id is already in the library.
    #[error("movie with TMDB id {tmdb_id} already exists as {existing:?}")]
    Duplicate { tmdb_id: u64, existing: MovieId },
    /// The backing store failed.
    #[error("movie store failure: {0}")]
    Backend(String),
}

/// Persistence for library movies.
pub trait MovieStore: Send + Sync {
    /// Insert a movie and assign it an id. Fails with
    /// [`StoreError::Duplicate`] if its TMDB id is already stored.
    fn insert(
        &self,
        metadata: MovieMetadata,
        fetched_at: i64,
    ) -> impl Future<Output = Result<Movie, StoreError>> + Send;

    fn get(&self, id: MovieId) -> impl Future<Output = Result<Option<Movie>, StoreError>> + Send;

    fn find_by_tmdb_id(
        &self,
        tmdb_id: u64,
    ) -> impl Future<Output = Result<Option<Movie>, StoreError>> + Send;

    /// All movies, ordered by id.
    fn list(&self) -> impl Future<Output = Result<Vec<Movie>, StoreError>> + Send;
}

/// A [`MovieStore`] kept in memory. Data is lost when it is dropped.
#[derive(Debug, Default)]
pub struct InMemoryMovieStore {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    next_id: i64,
    movies: BTreeMap<MovieId, Movie>,
}

impl InMemoryMovieStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>, StoreError> {
        self.inner
            .lock()
            .map_err(|_| StoreError::Backend("in-memory store lock poisoned".into()))
    }
}

impl MovieStore for InMemoryMovieStore {
    async fn insert(&self, metadata: MovieMetadata, fetched_at: i64) -> Result<Movie, StoreError> {
        let mut inner = self.lock()?;
        let tmdb_id = metadata.external_ids.tmdb_id;
        if let Some(existing) = inner
            .movies
            .values()
            .find(|m| m.metadata.external_ids.tmdb_id == tmdb_id)
        {
            return Err(StoreError::Duplicate {
                tmdb_id,
                existing: existing.id,
            });
        }
        inner.next_id += 1;
        let movie = Movie {
            id: MovieId(inner.next_id),
            metadata,
            metadata_fetched_at: fetched_at,
        };
        inner.movies.insert(movie.id, movie.clone());
        Ok(movie)
    }

    async fn get(&self, id: MovieId) -> Result<Option<Movie>, StoreError> {
        Ok(self.lock()?.movies.get(&id).cloned())
    }

    async fn find_by_tmdb_id(&self, tmdb_id: u64) -> Result<Option<Movie>, StoreError> {
        Ok(self
            .lock()?
            .movies
            .values()
            .find(|m| m.metadata.external_ids.tmdb_id == tmdb_id)
            .cloned())
    }

    async fn list(&self) -> Result<Vec<Movie>, StoreError> {
        Ok(self.lock()?.movies.values().cloned().collect())
    }
}
