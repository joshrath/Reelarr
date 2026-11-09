//! How the playback endpoints find a media file.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Mutex;

use reelarr_types::{MediaFileId, MediaProbe};

/// A file that can be streamed.
#[derive(Debug, Clone, PartialEq)]
pub struct PlayableFile {
    pub id: MediaFileId,
    pub path: PathBuf,
    /// The library root the file belongs to; the file must resolve inside it.
    pub root: PathBuf,
    pub probe: Option<MediaProbe>,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct LookupError(pub String);

/// Result of looking up a media file.
#[derive(Debug, Clone, PartialEq)]
pub enum Lookup {
    Found(PlayableFile),
    /// Unknown id, or a file a scan found missing.
    NotFound,
    /// Known file whose library root is offline (e.g. an unmounted share).
    Unavailable {
        reason: String,
    },
}

pub type LookupFuture<'a> = Pin<Box<dyn Future<Output = Result<Lookup, LookupError>> + Send + 'a>>;

/// Resolves a media file id to something playable.
pub trait MediaLookup: Send + Sync {
    fn lookup(&self, id: MediaFileId) -> LookupFuture<'_>;
}

/// In-memory lookup for tests and an empty default server.
#[derive(Debug, Default)]
pub struct InMemoryMediaLookup {
    files: Mutex<HashMap<MediaFileId, PlayableFile>>,
}

impl InMemoryMediaLookup {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, file: PlayableFile) {
        self.files
            .lock()
            .expect("lookup lock poisoned")
            .insert(file.id, file);
    }
}

impl MediaLookup for InMemoryMediaLookup {
    fn lookup(&self, id: MediaFileId) -> LookupFuture<'_> {
        let file = self
            .files
            .lock()
            .expect("lookup lock poisoned")
            .get(&id)
            .cloned();
        Box::pin(async move { Ok(file.map_or(Lookup::NotFound, Lookup::Found)) })
    }
}
