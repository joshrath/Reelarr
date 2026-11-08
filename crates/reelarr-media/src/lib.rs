//! Library scanning, import, rename and metadata.
//!
//! This crate currently provides movie metadata lookup ([`metadata`]), a
//! storage abstraction for movies ([`store`]) and the service that ties them
//! together ([`service`]). Scanning and import arrive in later issues.

pub mod metadata;
pub mod service;
pub mod store;

pub use metadata::{MetadataError, MetadataProvider, SearchQuery};
pub use service::{AddMovieError, MovieService};
pub use store::{InMemoryMovieStore, MovieStore, StoreError};
