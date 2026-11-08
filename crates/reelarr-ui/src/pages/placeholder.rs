//! Shared layout for pages that are not built yet.

use leptos::prelude::*;
use leptos_meta::Title;

/// A titled page with a short "coming soon" description.
#[component]
pub fn Placeholder(title: &'static str, description: &'static str) -> impl IntoView {
    view! {
        <Title text=title />
        <section class="page" aria-labelledby="page-title">
            <h1 id="page-title">{title}</h1>
            <p class="muted">{description}</p>
        </section>
    }
}
