//! Clean-room Newznab configuration, capabilities discovery and bounded movie search.
//!
//! Protocol source: <https://newznab.readthedocs.io/en/latest/misc/api.html>.
//! This crate returns candidates; metadata ownership (#8) and release selection (#10)
//! belong elsewhere. It does not download NZBs, poll RSS, or persist credentials.
//! Configure one shared [`NewznabClient`] per indexer and clone it to share its
//! capability cache and rate limiter. Configuration is trusted administrator input.
//! HTTP is rejected unless explicitly opted into; redirects and cross-origin
//! download URLs are rejected. Never log exposed secrets or raw provider fields.

mod client;
mod model;
mod store;
mod xml;

pub use client::NewznabClient;
pub use model::*;
pub use reqwest::Url;
pub use store::{InMemoryIndexerStore, IndexerStore, StoreFuture};

/// Usage, integration boundaries, limits and security policy.
#[doc = include_str!("../README.md")]
pub mod guide {}
