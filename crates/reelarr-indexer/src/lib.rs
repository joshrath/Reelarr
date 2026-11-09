//! Clean-room Newznab configuration, capabilities discovery and bounded movie search.
//!
//! Protocol source: <https://newznab.readthedocs.io/en/latest/misc/api.html>.
//! This crate returns candidates; metadata ownership (#8) and release selection (#10)
//! belong elsewhere. It fetches NZB files for releases ([`NewznabClient::fetch_nzb`])
//! but does not poll RSS or persist credentials.
//! Configure one shared [`NewznabClient`] per indexer and clone it to share its
//! capability cache and rate limiter. Configuration is trusted administrator input.
//! HTTP is rejected unless explicitly opted into. API redirects are rejected; download
//! URLs must be same-origin or HTTPS to an exact per-indexer allowlisted host, on
//! every redirect hop. Never log exposed secrets or raw provider fields.

mod client;
mod model;
mod store;
mod xml;

pub use client::{MAX_DOWNLOAD_REDIRECTS, NewznabClient};
pub use model::*;
pub use reqwest::Url;
pub use store::{InMemoryIndexerStore, IndexerStore, StoreFuture};

/// Usage, integration boundaries, limits and security policy.
#[doc = include_str!("../README.md")]
pub mod guide {}
