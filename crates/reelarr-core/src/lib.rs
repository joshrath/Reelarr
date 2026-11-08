//! Domain logic: movies, series, quality profiles, decisions.

pub mod settings;
pub use settings::{InMemorySettingsStore, Setting, SettingsError, SettingsStore};
