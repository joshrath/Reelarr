//! Media file storage abstraction. The SQLite implementation lives in
//! `reelarr-db`; this crate only has an in-memory one for tests.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Mutex;

use reelarr_types::{LibraryRootId, MediaFile, MediaFileId, MediaFileStatus, MediaProbe};

use crate::store::StoreError;

/// A file to add to the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMediaFile {
    pub root_id: LibraryRootId,
    pub path: String,
    pub size_bytes: u64,
    pub modified_at: i64,
    pub probe: Option<MediaProbe>,
}

/// Persistence for scanned media files.
pub trait MediaFileStore: Send + Sync {
    /// Insert a file. Paths are unique; inserting a known path fails with
    /// [`StoreError::Backend`].
    fn insert(
        &self,
        file: NewMediaFile,
    ) -> impl Future<Output = Result<MediaFile, StoreError>> + Send;

    /// Replace a stored file with the same id.
    fn update(&self, file: &MediaFile) -> impl Future<Output = Result<(), StoreError>> + Send;

    fn get(
        &self,
        id: MediaFileId,
    ) -> impl Future<Output = Result<Option<MediaFile>, StoreError>> + Send;

    /// All files under a root, in any status.
    fn list_by_root(
        &self,
        root_id: LibraryRootId,
    ) -> impl Future<Output = Result<Vec<MediaFile>, StoreError>> + Send;
}

type Files = (i64, BTreeMap<MediaFileId, MediaFile>);

/// In-memory [`MediaFileStore`].
#[derive(Debug, Default)]
pub struct InMemoryMediaFileStore {
    inner: Mutex<Files>,
}

impl InMemoryMediaFileStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Files>, StoreError> {
        self.inner
            .lock()
            .map_err(|_| StoreError::Backend("in-memory store lock poisoned".into()))
    }
}

impl MediaFileStore for InMemoryMediaFileStore {
    async fn insert(&self, file: NewMediaFile) -> Result<MediaFile, StoreError> {
        let mut guard = self.lock()?;
        if guard.1.values().any(|f| f.path == file.path) {
            return Err(StoreError::Backend(format!(
                "media file already stored: {}",
                file.path
            )));
        }
        guard.0 += 1;
        let stored = MediaFile {
            id: MediaFileId(guard.0),
            movie_id: None,
            root_id: file.root_id,
            path: file.path,
            size_bytes: file.size_bytes,
            modified_at: file.modified_at,
            status: MediaFileStatus::Present,
            probe: file.probe,
        };
        guard.1.insert(stored.id, stored.clone());
        Ok(stored)
    }

    async fn update(&self, file: &MediaFile) -> Result<(), StoreError> {
        let mut guard = self.lock()?;
        match guard.1.get_mut(&file.id) {
            Some(slot) => {
                *slot = file.clone();
                Ok(())
            }
            None => Err(StoreError::Backend(format!(
                "media file {:?} not found",
                file.id
            ))),
        }
    }

    async fn get(&self, id: MediaFileId) -> Result<Option<MediaFile>, StoreError> {
        Ok(self.lock()?.1.get(&id).cloned())
    }

    async fn list_by_root(&self, root_id: LibraryRootId) -> Result<Vec<MediaFile>, StoreError> {
        Ok(self
            .lock()?
            .1
            .values()
            .filter(|f| f.root_id == root_id)
            .cloned()
            .collect())
    }
}
