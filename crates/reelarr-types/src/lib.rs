//! Types shared between the Reelarr server and UI.

use serde::{Deserialize, Serialize};

pub mod movie;

pub use movie::{
    Artwork, DEFAULT_RESCAN_INTERVAL_SECS, ExternalIds, LibraryRoot, LibraryRootId,
    LibraryRootKind, MediaFile, MediaFileId, MediaFileStatus, MediaProbe, MediaStream, Movie,
    MovieId, MovieMetadata, MovieSearchResult, StreamKind,
};

/// Response body for the `/health` endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    pub status: String,
    pub version: String,
}

impl Health {
    pub fn ok() -> Self {
        Self {
            status: "ok".into(),
            version: env!("CARGO_PKG_VERSION").into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_ok() {
        assert_eq!(Health::ok().status, "ok");
    }
}
