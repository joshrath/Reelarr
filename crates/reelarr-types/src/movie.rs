//! Movie and media-file types shared by the server and the UI.

use serde::{Deserialize, Serialize};

/// Reelarr's own identifier for a movie in the library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MovieId(pub i64);

/// Reelarr's own identifier for a media file on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MediaFileId(pub i64);

/// Identifiers for a movie in external metadata sources.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ExternalIds {
    /// The Movie Database (TMDB) id. This is the primary key used for lookups.
    pub tmdb_id: u64,
    /// IMDb id such as `tt0111161`, when the source knows it.
    pub imdb_id: Option<String>,
}

/// References to artwork hosted by the metadata source.
///
/// These are source-relative paths (for TMDB, values like `/abc123.jpg`), not
/// downloaded images. Full URLs are built by the metadata provider.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Artwork {
    pub poster_path: Option<String>,
    pub backdrop_path: Option<String>,
}

/// Metadata for a movie as returned by a metadata source, before it is added
/// to the library.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MovieMetadata {
    pub external_ids: ExternalIds,
    pub title: String,
    pub original_title: Option<String>,
    /// Release year, when the source has a release date.
    pub year: Option<u16>,
    pub overview: Option<String>,
    pub runtime_minutes: Option<u32>,
    pub artwork: Artwork,
}

/// A movie in the Reelarr library.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Movie {
    pub id: MovieId,
    #[serde(flatten)]
    pub metadata: MovieMetadata,
    /// When the metadata was last fetched, as Unix seconds. Used to refresh
    /// cached metadata before the source's caching limit expires.
    pub metadata_fetched_at: i64,
}

/// A single result from a metadata search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MovieSearchResult {
    pub tmdb_id: u64,
    pub title: String,
    pub original_title: Option<String>,
    pub year: Option<u16>,
    pub overview: Option<String>,
    pub poster_path: Option<String>,
}

/// A media file that belongs to a movie. Probe data (codecs, duration)
/// is added by the library scanner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaFile {
    pub id: MediaFileId,
    pub movie_id: Option<MovieId>,
    /// Absolute path inside the Reelarr container.
    pub path: String,
    pub size_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn movie_serializes_flat() {
        let movie = Movie {
            id: MovieId(1),
            metadata: MovieMetadata {
                external_ids: ExternalIds {
                    tmdb_id: 42,
                    imdb_id: None,
                },
                title: "Example".into(),
                original_title: None,
                year: Some(2001),
                overview: None,
                runtime_minutes: None,
                artwork: Artwork::default(),
            },
            metadata_fetched_at: 0,
        };
        let json = serde_json::to_value(&movie).unwrap();
        assert_eq!(json["id"], 1);
        assert_eq!(json["title"], "Example");
        assert_eq!(json["external_ids"]["tmdb_id"], 42);
        let back: Movie = serde_json::from_value(json).unwrap();
        assert_eq!(back, movie);
    }
}
