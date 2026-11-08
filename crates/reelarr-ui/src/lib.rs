//! Reelarr web UI, built with Leptos.

use leptos::prelude::*;

/// Root application component.
#[component]
pub fn App() -> impl IntoView {
    view! {
        <main>
            <h1>"Reelarr"</h1>
            <p>"Pre-alpha. Nothing to see here yet."</p>
        </main>
    }
}
