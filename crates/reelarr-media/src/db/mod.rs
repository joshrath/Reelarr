//! SQLite-backed stores for the movie library, built on `reelarr-db`.
//!
//! The schema lives in `reelarr-db`'s migrations (`0003_library.sql`); this
//! module only reads and writes it, so `reelarr-db` never depends on this
//! crate.

mod files;
mod ledger;
mod lookup;
mod movies;
mod roots;

pub use files::SqliteMediaFileStore;
pub use ledger::{LedgerEntry, LedgerState, SqliteImportLedger};
pub use lookup::DbMediaLookup;
pub use movies::SqliteMovieStore;
pub use roots::{RootError, SqliteLibraryRootStore};

use crate::store::StoreError;

pub(crate) fn backend(e: impl std::fmt::Display) -> StoreError {
    StoreError::Backend(e.to_string())
}

pub(crate) fn to_i64(value: u64, what: &str) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::Backend(format!("{what} {value} is too large")))
}

pub(crate) fn to_u64(value: i64, what: &str) -> Result<u64, StoreError> {
    u64::try_from(value)
        .map_err(|_| StoreError::Backend(format!("stored {what} {value} is negative")))
}

pub(crate) fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
