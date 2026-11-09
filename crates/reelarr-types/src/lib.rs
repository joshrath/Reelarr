//! Types shared between the Reelarr server and UI.

use serde::{Deserialize, Serialize};

pub mod job;
pub mod movie;
pub mod playback;
pub use job::{JobEvent, JobEventKind, JobState, Progress};

pub use movie::{
    Artwork, DEFAULT_RESCAN_INTERVAL_SECS, ExternalIds, LibraryRoot, LibraryRootId,
    LibraryRootInfo, LibraryRootKind, MediaFile, MediaFileId, MediaFileStatus, MediaKind,
    MediaProbe, MediaStream, Movie, MovieId, MovieMetadata, MovieSearchResult, NewLibraryRoot,
    RootHealth, StreamKind,
};
pub use playback::{PlaybackInfo, PlaybackMethod, UnsupportedKind, UnsupportedReason};

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

/// First-run status; setup is allowed exactly once per store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupStatus {
    pub needs_setup: bool,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupRequest {
    pub username: String,
    pub password: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub user: Option<UserInfo>,
    pub csrf_token: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserInfo {
    pub id: String,
    pub username: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    pub code: String,
    pub message: String,
}

#[cfg(test)]
mod auth_tests {
    use super::*;
    fn round_trip<T: Serialize + serde::de::DeserializeOwned + PartialEq>(value: T) {
        let json = serde_json::to_string(&value).unwrap();
        assert!(value == serde_json::from_str::<T>(&json).unwrap());
    }
    #[test]
    fn auth_dtos_round_trip() {
        round_trip(SetupStatus { needs_setup: true });
        round_trip(SetupRequest {
            username: "admin".into(),
            password: "secret".into(),
        });
        round_trip(LoginRequest {
            username: "admin".into(),
            password: "secret".into(),
        });
        let user = UserInfo {
            id: "1".into(),
            username: "admin".into(),
        };
        round_trip(user.clone());
        round_trip(SessionInfo {
            user: Some(user),
            csrf_token: "token".into(),
        });
        round_trip(SessionInfo {
            user: None,
            csrf_token: "token".into(),
        });
        round_trip(ApiError {
            code: "unauthorized".into(),
            message: "Sign in".into(),
        });
    }
}
