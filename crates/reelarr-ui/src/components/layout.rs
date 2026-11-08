//! The application frame: skip link, header with navigation, and main region.

use leptos::prelude::*;

use super::nav::MainNav;

/// Id of the `<main>` element targeted by the skip link.
pub const MAIN_ID: &str = "main-content";

/// Wraps a page in the accessible app shell.
#[component]
pub fn Layout(children: Children) -> impl IntoView {
    view! {
        <a class="skip-link" href=format!("#{MAIN_ID}")>"Skip to content"</a>
        <header class="app-header">
            <a class="brand" href="/">"Reelarr"</a>
            <MainNav />
        </header>
        <main id=MAIN_ID class="app-main" tabindex="-1">
            {children()}
        </main>
    }
}
