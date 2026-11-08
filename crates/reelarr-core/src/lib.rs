//! Domain services and settings.

pub mod auth;
pub mod settings;
pub use settings::{InMemorySettingsStore, Setting, SettingsError, SettingsStore};
