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

/// A media file found in a library root. Probe data comes from the library
/// scanner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaFile {
    pub id: MediaFileId,
    pub movie_id: Option<MovieId>,
    /// The library root this file was found under.
    pub root_id: LibraryRootId,
    /// Absolute path inside the Reelarr container.
    pub path: String,
    pub size_bytes: u64,
    /// Modification time as Unix seconds, used for change detection.
    pub modified_at: i64,
    pub status: MediaFileStatus,
    /// `None` until the file has been probed successfully.
    pub probe: Option<MediaProbe>,
}

/// Whether a known file is currently on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MediaFileStatus {
    Present,
    /// The file was not found by a scan of a reachable root. It is kept, not
    /// deleted, so an unmounted share does not wipe the library.
    Missing {
        since: i64,
    },
}

/// Reelarr's identifier for a configured library root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LibraryRootId(pub i64);

/// What a library root is used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibraryRootKind {
    /// A folder of organized movies.
    Library,
    /// A folder of files the user wants to import by hand.
    ManualImport,
}

/// Default rescan interval for a library root: six hours.
pub const DEFAULT_RESCAN_INTERVAL_SECS: u64 = 6 * 60 * 60;

/// A folder Reelarr scans for media, local or on a network share.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryRoot {
    pub id: LibraryRootId,
    /// Absolute path inside the Reelarr container.
    pub path: String,
    pub kind: LibraryRootKind,
    /// How often the root is rescanned. `None` disables periodic rescans.
    pub rescan_interval_secs: Option<u64>,
}

/// Technical details of a media file, read with ffprobe.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MediaProbe {
    /// Container names as reported by ffprobe, e.g. `["mov", "mp4"]`.
    pub container: Vec<String>,
    pub duration_ms: Option<u64>,
    pub bit_rate: Option<u64>,
    pub streams: Vec<MediaStream>,
}

/// One stream inside a media file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaStream {
    pub index: u32,
    pub kind: StreamKind,
    /// Codec name as reported by ffprobe, e.g. `h264`, `aac`, `subrip`.
    pub codec: Option<String>,
    pub profile: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub channels: Option<u32>,
    /// ISO 639-2 language tag, when present and not `und`.
    pub language: Option<String>,
    pub default: bool,
    pub forced: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamKind {
    Video,
    Audio,
    Subtitle,
    Attachment,
    Data,
    Other,
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

/// Whether a library root can be reached. An offline root's files are kept
/// and shown as unavailable; they are never treated as deleted.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RootHealth {
    /// Not checked yet.
    #[default]
    Unknown,
    Online,
    /// Unreachable: missing, unmounted, a stale NFS handle, a timeout, etc.
    Offline {
        since: i64,
        reason: String,
    },
    /// Reachable but Reelarr may not read it.
    PermissionDenied {
        reason: String,
    },
}

impl RootHealth {
    /// Whether files under the root can be served and scanned.
    pub fn is_available(&self) -> bool {
        matches!(self, RootHealth::Unknown | RootHealth::Online)
    }
}

/// A library root with its latest health check, as shown in settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryRootInfo {
    #[serde(flatten)]
    pub root: LibraryRoot,
    pub health: RootHealth,
    /// Unix seconds of the last health check.
    pub health_checked_at: Option<i64>,
    /// Files recorded under this root, in any status.
    pub file_count: u64,
}

/// Body of `POST /api/v1/library/roots`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewLibraryRoot {
    pub path: String,
    pub kind: LibraryRootKind,
    /// Defaults to [`DEFAULT_RESCAN_INTERVAL_SECS`] when omitted.
    #[serde(default)]
    pub rescan_interval_secs: Option<u64>,
}
