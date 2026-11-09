//! Reelarr web UI, built with Leptos.
//!
//! The crate compiles two ways:
//!
//! - with the `ssr` feature (native) it renders pages on the server and
//!   exposes [`router`] for `reelarr-server` to merge into its axum router;
//! - with the `hydrate` feature (`wasm32-unknown-unknown`) it is built by
//!   `cargo leptos` into `/pkg/reelarr.wasm` and hydrates the server HTML.

pub mod api;
pub mod app;
pub mod components;
pub mod pages;
pub mod playback;
pub mod theme;

#[cfg(feature = "ssr")]
mod server;

pub use app::{App, shell};
#[cfg(feature = "ssr")]
pub use server::router;

/// WebAssembly entry point: attach to the server-rendered HTML.
#[cfg(feature = "hydrate")]
#[wasm_bindgen::prelude::wasm_bindgen]
pub fn hydrate() {
    console_error_panic_hook::set_once();
    leptos::mount::hydrate_body(App);
}
