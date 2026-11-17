//! `reelarr_nzb::worker::RecoveryPaths` over the job row's `recovery_path` (#76).

use std::path::Path;

use reelarr_nzb::{
    JobId,
    worker::{RecoveryPathError, RecoveryPaths},
};

use crate::SqliteJobStore;

/// Records the NZB worker's artifact paths with
/// [`SqliteJobStore::set_recovery_path`], so startup recovery can tell whether a
/// processing stage (Verifying, Repairing, Extracting) can be re-entered.
///
/// Paths must be absolute and UTF-8; anything else is rejected rather than
/// stored, and an unknown job is an error. Cheap to clone: clones share the
/// job store's pool and cache.
#[derive(Clone, Debug)]
pub struct SqliteRecoveryPaths {
    jobs: SqliteJobStore,
}

impl SqliteRecoveryPaths {
    pub fn new(jobs: SqliteJobStore) -> Self {
        Self { jobs }
    }
}

impl RecoveryPaths for SqliteRecoveryPaths {
    async fn set_recovery_path(
        &self,
        job_id: JobId,
        path: Option<&Path>,
    ) -> Result<(), RecoveryPathError> {
        self.jobs
            .set_recovery_path(job_id, path)
            .await
            .map_err(|e| Box::new(e) as RecoveryPathError)
    }
}
