//! Storage boundary for shared-database implementations and restart checkpoints.
use crate::{JobId, NzbDocument, ParseError, validate};
use std::{
    collections::{BTreeSet, HashMap},
    future::Future,
    sync::{Arc, RwLock},
};
use thiserror::Error;

/// Stable segment address within an immutable stored NZB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SegmentKey {
    /// Zero-based file index in document order.
    pub file_index: usize,
    /// One-based segment number.
    pub number: u32,
}

/// Parsed document plus durable completion checkpoints.
#[derive(Clone, PartialEq, Eq)]
pub struct StoredNzb {
    /// Immutable original NZB data.
    pub document: NzbDocument,
    /// Segments whose verified bytes have been durably written before checkpoint.
    pub completed: BTreeSet<SegmentKey>,
}
impl StoredNzb {
    /// Calculate pending work and byte counters, validating stored data first.
    /// Declared NZB bytes are encoded article bytes, not decoded file sizes.
    /// A real restart must reconcile/checksum disk artifacts before trusting the
    /// checkpoints; this helper cannot establish filesystem durability.
    pub fn resume(&self) -> Result<ResumePlan, StoreError> {
        let bytes_total = validate(&self.document)?;
        for key in &self.completed {
            self.check_key(*key)?;
        }
        let mut plan = ResumePlan {
            remaining: Vec::new(),
            bytes_done: 0,
            bytes_total,
        };
        for (file_index, file) in self.document.files.iter().enumerate() {
            for segment in &file.segments {
                let key = SegmentKey {
                    file_index,
                    number: segment.number,
                };
                if self.completed.contains(&key) {
                    plan.bytes_done += segment.bytes;
                } else {
                    plan.remaining.push(key);
                }
            }
        }
        Ok(plan)
    }
    fn check_key(&self, key: SegmentKey) -> Result<(), StoreError> {
        let index = key
            .number
            .checked_sub(1)
            .and_then(|n| usize::try_from(n).ok());
        if self
            .document
            .files
            .get(key.file_index)
            .and_then(|f| index.and_then(|i| f.segments.get(i)))
            .is_some_and(|s| s.number == key.number)
        {
            Ok(())
        } else {
            Err(StoreError::UnknownSegment(key))
        }
    }
}

/// Remaining segment addresses in file/number order and declared byte progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumePlan {
    /// Segments not checkpointed as complete.
    pub remaining: Vec<SegmentKey>,
    /// Sum of declared bytes for completed segments.
    pub bytes_done: u64,
    /// Sum of declared bytes for all segments.
    pub bytes_total: u64,
}

/// Typed storage failures without NZB content in error messages.
#[derive(Debug, Error)]
pub enum StoreError {
    /// Job has no stored NZB.
    #[error("NZB not found for job")]
    NotFound,
    /// Insertion never overwrites a document/checkpoints.
    #[error("job already has an NZB")]
    AlreadyExists,
    /// Checkpoint addresses must exist in the stored immutable document.
    #[error("unknown segment: {0:?}")]
    UnknownSegment(SegmentKey),
    /// Supplied domain data is invalid.
    #[error(transparent)]
    InvalidDocument(#[from] ParseError),
    /// Database or in-memory lock failure; avoid leaking raw SQL or secrets.
    #[error("NZB storage backend failed")]
    Backend,
}

/// Asynchronous storage contract keyed by core's job identifier.
///
/// `reelarr-db` supplies the SQLite implementation and migrations; no lifecycle
/// states or queue ordering belong here. Documents are immutable after insert.
/// Batches must validate all addresses before committing atomically; repeated
/// updates are idempotent. Only checkpoint **after** verified segment files are
/// durably written. `false` clears a checkpoint after failed disk reconciliation.
/// Futures are Send so workers may use Tokio's multithreaded runtime. This trait
/// uses static dispatch and deliberately introduces no database/runtime dependency.
pub trait NzbStore: Send + Sync {
    /// Insert a new document with no completed segments, rejecting existing IDs.
    fn insert(
        &self,
        job_id: JobId,
        document: NzbDocument,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;
    /// Load a consistent snapshot; return None if absent.
    fn load(
        &self,
        job_id: JobId,
    ) -> impl Future<Output = Result<Option<StoredNzb>, StoreError>> + Send;
    /// Atomically apply the whole batch in order; last update wins for duplicates.
    fn checkpoint(
        &self,
        job_id: JobId,
        updates: &[(SegmentKey, bool)],
    ) -> impl Future<Output = Result<(), StoreError>> + Send;
}

/// Cloneable in-memory store for tests; clones share state across worker restarts.
/// This is not process-persistent and must not be used as production durability.
#[derive(Clone, Default)]
pub struct InMemoryNzbStore {
    entries: Arc<RwLock<HashMap<JobId, StoredNzb>>>,
}
impl NzbStore for InMemoryNzbStore {
    async fn insert(&self, job_id: JobId, document: NzbDocument) -> Result<(), StoreError> {
        validate(&document)?;
        let mut entries = self.entries.write().map_err(|_| StoreError::Backend)?;
        if entries.contains_key(&job_id) {
            return Err(StoreError::AlreadyExists);
        }
        entries.insert(
            job_id,
            StoredNzb {
                document,
                completed: BTreeSet::new(),
            },
        );
        Ok(())
    }
    async fn load(&self, job_id: JobId) -> Result<Option<StoredNzb>, StoreError> {
        Ok(self
            .entries
            .read()
            .map_err(|_| StoreError::Backend)?
            .get(&job_id)
            .cloned())
    }
    async fn checkpoint(
        &self,
        job_id: JobId,
        updates: &[(SegmentKey, bool)],
    ) -> Result<(), StoreError> {
        let mut entries = self.entries.write().map_err(|_| StoreError::Backend)?;
        let stored = entries.get_mut(&job_id).ok_or(StoreError::NotFound)?;
        for (key, _) in updates {
            stored.check_key(*key)?;
        }
        for (key, complete) in updates {
            if *complete {
                stored.completed.insert(*key);
            } else {
                stored.completed.remove(key);
            }
        }
        Ok(())
    }
}
