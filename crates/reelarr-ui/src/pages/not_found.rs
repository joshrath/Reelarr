//! 404 page.

use leptos::prelude::*;
use leptos_meta::Title;

/// Shown for any path that doesn't match a route. On the server this also
/// sets the HTTP status to 404.
#[component]
pub fn NotFoundPage() -> impl IntoView {
    #[cfg(feature = "ssr")]
    {
        if let Some(response) = use_context::<leptos_axum::ResponseOptions>() {
            response.set_status(axum::http::StatusCode::NOT_FOUND);
        }
    }

    view! {
        <Title text="Page not found" />
        <section class="page" aria-labelledby="page-title">
            <h1 id="page-title">"Page not found"</h1>
            <p>"There's nothing at this address."</p>
            <p><a href="/">"Back to the library"</a></p>
        </section>
    }
}
