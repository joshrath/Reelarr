//! Playback decisions shared by the server and the player UI.

use serde::{Deserialize, Serialize};

use crate::MediaFileId;

/// Answer to `GET /api/v1/media/{id}/playback`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybackInfo {
    pub media_file_id: MediaFileId,
    /// How the client should play the file.
    pub method: PlaybackMethod,
    /// Relative URL of the byte-range stream, set for direct play.
    pub stream_url: Option<String>,
    /// MIME type to put on the `<video>` source, e.g. `video/mp4`.
    pub mime_type: Option<String>,
    /// Why direct play is not possible; empty for direct play.
    pub unsupported: Vec<UnsupportedReason>,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackMethod {
    /// The browser can play the original file as-is.
    DirectPlay,
    /// The file needs remuxing or transcoding, which is not available yet.
    Unsupported,
}

/// One reason a file cannot be played directly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnsupportedReason {
    pub kind: UnsupportedKind,
    /// The container or codec name as reported by ffprobe, when known.
    pub name: Option<String>,
    /// Human-readable explanation for the player UI.
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnsupportedKind {
    Container,
    VideoCodec,
    AudioCodec,
    /// The file has not been probed yet, so support is unknown.
    NotProbed,
    /// A disc-image file (ISO), not playable yet.
    DiscImage,
    /// A DVD/Blu-ray folder rip, not playable yet.
    DiscFolder,
}
