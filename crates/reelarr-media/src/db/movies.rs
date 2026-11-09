use reelarr_db::Database;
use reelarr_types::{Artwork, ExternalIds, Movie, MovieId, MovieMetadata};
// `COLUMNS` is a compile-time constant, so these `format!`ed queries carry no
// caller-supplied SQL; values are always bound with `?`.
use sqlx::sqlite::SqliteRow;
use sqlx::{AssertSqlSafe, Row};

use super::{backend, to_i64, to_u64};
use crate::store::{MovieStore, StoreError};

const COLUMNS: &str = "id, tmdb_id, imdb_id, title, original_title, year, overview, \
     runtime_minutes, poster_path, backdrop_path, metadata_fetched_at";

/// [`MovieStore`] backed by the `movies` table.
#[derive(Debug, Clone)]
pub struct SqliteMovieStore {
    db: Database,
}

impl SqliteMovieStore {
    pub fn new(db: &Database) -> Self {
        Self { db: db.clone() }
    }
}

fn row_to_movie(row: &SqliteRow) -> Result<Movie, StoreError> {
    let year: Option<i64> = row.try_get("year").map_err(backend)?;
    let runtime: Option<i64> = row.try_get("runtime_minutes").map_err(backend)?;
    Ok(Movie {
        id: MovieId(row.try_get("id").map_err(backend)?),
        metadata: MovieMetadata {
            external_ids: ExternalIds {
                tmdb_id: to_u64(row.try_get("tmdb_id").map_err(backend)?, "tmdb_id")?,
                imdb_id: row.try_get("imdb_id").map_err(backend)?,
            },
            title: row.try_get("title").map_err(backend)?,
            original_title: row.try_get("original_title").map_err(backend)?,
            year: year.map(u16::try_from).transpose().map_err(backend)?,
            overview: row.try_get("overview").map_err(backend)?,
            runtime_minutes: runtime.map(u32::try_from).transpose().map_err(backend)?,
            artwork: Artwork {
                poster_path: row.try_get("poster_path").map_err(backend)?,
                backdrop_path: row.try_get("backdrop_path").map_err(backend)?,
            },
        },
        metadata_fetched_at: row.try_get("metadata_fetched_at").map_err(backend)?,
    })
}

impl MovieStore for SqliteMovieStore {
    async fn insert(&self, metadata: MovieMetadata, fetched_at: i64) -> Result<Movie, StoreError> {
        let tmdb_id = metadata.external_ids.tmdb_id;
        let result = sqlx::query(
            "INSERT INTO movies (tmdb_id, imdb_id, title, original_title, year, overview, \
             runtime_minutes, poster_path, backdrop_path, metadata_fetched_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(to_i64(tmdb_id, "tmdb_id")?)
        .bind(&metadata.external_ids.imdb_id)
        .bind(&metadata.title)
        .bind(&metadata.original_title)
        .bind(metadata.year.map(i64::from))
        .bind(&metadata.overview)
        .bind(metadata.runtime_minutes.map(i64::from))
        .bind(&metadata.artwork.poster_path)
        .bind(&metadata.artwork.backdrop_path)
        .bind(fetched_at)
        .execute(self.db.pool())
        .await;
        match result {
            Ok(done) => Ok(Movie {
                id: MovieId(done.last_insert_rowid()),
                metadata,
                metadata_fetched_at: fetched_at,
            }),
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
                let existing = self
                    .find_by_tmdb_id(tmdb_id)
                    .await?
                    .ok_or_else(|| backend(e))?;
                Err(StoreError::Duplicate {
                    tmdb_id,
                    existing: existing.id,
                })
            }
            Err(e) => Err(backend(e)),
        }
    }

    async fn get(&self, id: MovieId) -> Result<Option<Movie>, StoreError> {
        sqlx::query(AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM movies WHERE id = ?"
        )))
        .bind(id.0)
        .fetch_optional(self.db.pool())
        .await
        .map_err(backend)?
        .as_ref()
        .map(row_to_movie)
        .transpose()
    }

    async fn find_by_tmdb_id(&self, tmdb_id: u64) -> Result<Option<Movie>, StoreError> {
        sqlx::query(AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM movies WHERE tmdb_id = ?"
        )))
        .bind(to_i64(tmdb_id, "tmdb_id")?)
        .fetch_optional(self.db.pool())
        .await
        .map_err(backend)?
        .as_ref()
        .map(row_to_movie)
        .transpose()
    }

    async fn list(&self) -> Result<Vec<Movie>, StoreError> {
        sqlx::query(AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM movies ORDER BY id"
        )))
        .fetch_all(self.db.pool())
        .await
        .map_err(backend)?
        .iter()
        .map(row_to_movie)
        .collect()
    }
}
