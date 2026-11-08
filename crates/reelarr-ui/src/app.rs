//! Root component, HTML shell and routes.

use leptos::prelude::*;
use leptos_meta::{MetaTags, Stylesheet, Title, provide_meta_context};
use leptos_router::{
    StaticSegment,
    components::{Route, Router, Routes},
};

use crate::components::layout::Layout;
use crate::pages::{
    library::LibraryPage, login::LoginPage, not_found::NotFoundPage, queue::QueuePage,
    search::SearchPage, settings::SettingsPage,
};

/// The full HTML document rendered on the server around [`App`].
pub fn shell(options: LeptosOptions) -> impl IntoView {
    view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8" />
                <meta name="viewport" content="width=device-width, initial-scale=1" />
                <meta name="color-scheme" content="dark light" />
                <AutoReload options=options.clone() />
                <HydrationScripts options />
                <MetaTags />
            </head>
            <body>
                <App />
            </body>
        </html>
    }
}

/// Root application component.
#[component]
pub fn App() -> impl IntoView {
    provide_meta_context();

    view! {
        <Stylesheet id="reelarr" href="/pkg/reelarr.css" />
        <Title formatter=|page: String| {
            if page.is_empty() { "Reelarr".to_string() } else { format!("{page} · Reelarr") }
        } />
        <Router>
            <Routes fallback=|| view! { <Layout><NotFoundPage /></Layout> }>
                <Route path=StaticSegment("") view=|| view! { <Layout><LibraryPage /></Layout> } />
                <Route path=StaticSegment("search") view=|| view! { <Layout><SearchPage /></Layout> } />
                <Route path=StaticSegment("queue") view=|| view! { <Layout><QueuePage /></Layout> } />
                <Route path=StaticSegment("settings") view=|| view! { <Layout><SettingsPage /></Layout> } />
                <Route path=StaticSegment("login") view=LoginPage />
            </Routes>
        </Router>
    }
}
