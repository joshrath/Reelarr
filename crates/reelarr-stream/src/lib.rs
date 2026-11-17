//! Playback for Reelarr.
//!
//! Direct play is implemented: files the browser can decode are served as-is
//! with HTTP byte ranges ([`serve`]), after a per-client codec check
//! ([`compat`]). Remuxing and transcoding with FFmpeg (run as a separate
//! process) come later; until then unsupported files get a clear reason.

pub mod compat;
pub mod lookup;
pub mod range;
pub mod serve;

pub use compat::{Browser, ClientProfile};
pub use lookup::{
    InMemoryMediaLookup, Lookup, LookupError, LookupFuture, MediaLookup, PlayableFile,
};

use axum::Json;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use reelarr_types::{
    ApiError, MediaFileId, MediaKind, PlaybackInfo, PlaybackMethod, UnsupportedKind,
    UnsupportedReason,
};

/// Errors from the playback endpoints, rendered as `ApiError` JSON.
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error("media file not found")]
    NotFound,
    #[error("media file unavailable: {0}")]
    Unavailable(String),
    /// A disc rip or unrecognized image, which cannot be streamed yet.
    #[error("this file is not playable yet")]
    NotPlayable,
    #[error("media lookup failed: {0}")]
    Lookup(#[from] LookupError),
}

impl IntoResponse for StreamError {
    fn into_response(self) -> Response {
        let (status, code, message) = match &self {
            StreamError::NotFound => (
                StatusCode::NOT_FOUND,
                "not_found",
                "That media file does not exist or is not available",
            ),
            StreamError::NotPlayable => (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "not_playable",
                "This file can't be played yet.",
            ),
            StreamError::Unavailable(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                "This file's library folder is offline. Check that the network share is mounted.",
            ),
            StreamError::Lookup(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "lookup_failed",
                "The library is unavailable",
            ),
        };
        match &self {
            StreamError::Lookup(e) => tracing::error!(error = %e, "media lookup failed"),
            StreamError::Unavailable(reason) => tracing::debug!(%reason, "media unavailable"),
            StreamError::NotFound | StreamError::NotPlayable => {}
        }
        (
            status,
            Json(ApiError {
                code: code.into(),
                message: message.into(),
            }),
        )
            .into_response()
    }
}

/// URL of the byte-range stream for a media file, relative to the site root.
pub fn stream_url(id: MediaFileId) -> String {
    format!("/api/v1/media/{}/stream", id.0)
}

/// Decide how `client` should play a media file.
pub async fn playback_info(
    lookup: &dyn MediaLookup,
    id: MediaFileId,
    client: ClientProfile,
) -> Result<PlaybackInfo, StreamError> {
    let file = found(lookup.lookup(id).await?)?;
    let ext = file.path.extension().and_then(|e| e.to_str());
    let duration_ms = file.probe.as_ref().and_then(|p| p.duration_ms);
    if !file.kind.is_streamable() {
        let reason = match file.kind {
            MediaKind::DiscFolder => UnsupportedReason {
                kind: UnsupportedKind::DiscFolder,
                name: None,
                message: "DVD/Blu-ray folder (not yet playable)".into(),
            },
            MediaKind::UnrecognizedImage => UnsupportedReason {
                kind: UnsupportedKind::Container,
                name: Some("iso".into()),
                message: "Not a recognized DVD or Blu-ray image, so it can't be played".into(),
            },
            _ => UnsupportedReason {
                kind: UnsupportedKind::DiscImage,
                name: None,
                message: "Disc image (not yet playable)".into(),
            },
        };
        return Ok(PlaybackInfo {
            media_file_id: id,
            method: PlaybackMethod::Unsupported,
            stream_url: None,
            mime_type: None,
            unsupported: vec![reason],
            duration_ms,
        });
    }
    Ok(match compat::check(file.probe.as_ref(), ext, client) {
        Ok(mime) => PlaybackInfo {
            media_file_id: id,
            method: PlaybackMethod::DirectPlay,
            stream_url: Some(stream_url(id)),
            mime_type: Some(mime.into()),
            unsupported: Vec::new(),
            duration_ms,
        },
        Err(reasons) => PlaybackInfo {
            media_file_id: id,
            method: PlaybackMethod::Unsupported,
            stream_url: None,
            mime_type: None,
            unsupported: reasons,
            duration_ms,
        },
    })
}

/// Serve a media file's bytes, honoring `Range`. The caller must already have
/// authenticated the request.
pub async fn stream(
    lookup: &dyn MediaLookup,
    id: MediaFileId,
    method: &Method,
    headers: &HeaderMap,
) -> Response {
    let file = match lookup
        .lookup(id)
        .await
        .map_err(StreamError::from)
        .and_then(found)
    {
        Ok(file) => file,
        Err(e) => return e.into_response(),
    };
    if !file.kind.is_streamable() {
        return StreamError::NotPlayable.into_response();
    }
    let path = match serve::resolve_within(&file.path, &file.root).await {
        Ok(path) => path,
        Err(e) => {
            if matches!(e, serve::PathError::OutsideRoot) {
                tracing::warn!(id = id.0, path = %file.path.display(), "refused file outside its library root");
            }
            return StreamError::NotFound.into_response();
        }
    };
    let mime = serve::mime_for_path(&path);
    serve::serve_file(&path, mime, method, headers).await
}

fn found(lookup: Lookup) -> Result<PlayableFile, StreamError> {
    match lookup {
        Lookup::Found(file) => Ok(file),
        Lookup::NotFound => Err(StreamError::NotFound),
        Lookup::Unavailable { reason } => Err(StreamError::Unavailable(reason)),
    }
}
