//! Bounded NZB parsing, streaming yEnc decoding, disk assembly and segment storage.
//!
//! Core owns job lifecycle and scheduling; this crate stores only NZB execution
//! data. `reelarr-db` will implement [`NzbStore`] in the shared SQLite database.
//! No subject, group or message ID is interpreted as a filesystem path.

pub mod assembly;
mod parser;
mod store;
pub mod yenc;

pub use parser::{ParseError, ParseLimits, parse};
pub use store::{InMemoryNzbStore, NzbStore, ResumePlan, SegmentKey, StoreError, StoredNzb};

/// Temporary opaque job identifier, supplied by the caller (not generated here).
/// TODO: replace with `reelarr_core::JobId` once the core lifecycle PR lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JobId(pub u128);

/// Parsed NZB metadata and file list. Metadata may contain secrets (passwords);
/// do not log or expose the document to the UI indiscriminately.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct NzbDocument {
    /// Optional release title from head metadata.
    pub title: Option<String>,
    /// Optional archive password; deliberately not included in a Debug impl.
    pub password: Option<String>,
    /// Optional download category.
    pub category: Option<String>,
    /// Files in document order (stable storage indices).
    pub files: Vec<NzbFile>,
}

/// One posted file, with segments sorted by one-based segment number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NzbFile {
    /// Original subject, never a trusted filename.
    pub subject: String,
    /// Original poster.
    pub poster: String,
    /// Posting timestamp as Unix seconds.
    pub date: u64,
    /// Newsgroups where articles were posted.
    pub groups: Vec<String>,
    /// Contiguous segments numbered from one.
    pub segments: Vec<Segment>,
}

/// Article locator and declared encoded byte count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// Message ID without angle brackets; safe to embed in NNTP angle brackets.
    pub message_id: String,
    /// One-based part number within the file.
    pub number: u32,
    /// Declared article bytes (not decoded file size).
    pub bytes: u64,
}

/// Validate domain data, including data supplied directly to a store.
fn validate(document: &NzbDocument) -> Result<u64, ParseError> {
    if document.files.is_empty() {
        return Err(ParseError::Invalid("NZB has no files"));
    }
    let mut total = 0_u64;
    for file in &document.files {
        if file.groups.is_empty() || file.groups.iter().any(|g| g.trim().is_empty()) {
            return Err(ParseError::Invalid("file has no groups"));
        }
        if file.segments.is_empty() {
            return Err(ParseError::Invalid("file has no segments"));
        }
        for (index, segment) in file.segments.iter().enumerate() {
            if usize::try_from(segment.number).ok() != index.checked_add(1) {
                return Err(ParseError::Invalid("duplicate or missing segment number"));
            }
            if segment.bytes == 0 {
                return Err(ParseError::Invalid("segment bytes must be positive"));
            }
            let id = &segment.message_id;
            let valid_parts = id.split_once('@').is_some_and(|(left, right)| {
                !left.is_empty() && !right.is_empty() && !right.contains('@')
            });
            if !valid_parts
                || id
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control() || c == '<' || c == '>')
            {
                return Err(ParseError::Invalid("invalid message ID"));
            }
            total = total
                .checked_add(segment.bytes)
                .ok_or(ParseError::Invalid("byte total overflow"))?;
        }
    }
    Ok(total)
}
