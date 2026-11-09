//! Root component, HTML shell and routes.

use leptos::prelude::*;
use leptos_meta::{MetaTags, Stylesheet, Title, provide_meta_context};
use leptos_router::{
    ParamSegment, StaticSegment,
    components::{Route, Router, Routes},
};

use crate::components::layout::Layout;
use crate::pages::{
    back_room::BackRoomPage, counter::CounterPage, employees_only::EmployeesOnlyPage,
    login::LoginPage, not_found::NotFoundPage, record_corner::RecordCornerPage,
    store_floor::StoreFloorPage, watch::WatchPage,
};
use crate::theme::{Prefs, PrefsSignal};

/// Old paths that now permanently redirect to their renamed sections, as
/// `(from, to)`. The server answers these before the UI renders.
pub const REDIRECTS: &[(&str, &str)] = &[
    ("/search", "/counter"),
    ("/queue", "/back-room"),
    ("/settings", "/employees-only"),
];

/// The full HTML document rendered on the server around [`App`].
pub fn shell(options: LeptosOptions) -> impl IntoView {
    // Saved display preferences, applied before the first paint.
    let prefs = Prefs::current();
    view! {
        <!DOCTYPE html>
        <html lang="en" data-theme=prefs.theme.as_str() class=prefs.html_class()>
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
    PrefsSignal::provide();

    view! {
        <Stylesheet id="reelarr" href="/pkg/reelarr.css" />
        <Title formatter=|page: String| {
            if page.is_empty() { "Reelarr".to_string() } else { format!("{page} · Reelarr") }
        } />
        <Router>
            <Routes fallback=|| view! { <Layout><NotFoundPage /></Layout> }>
                <Route path=StaticSegment("") view=|| view! { <Layout><StoreFloorPage /></Layout> } />
                <Route path=StaticSegment("counter") view=|| view! { <Layout><CounterPage /></Layout> } />
                <Route path=StaticSegment("records") view=|| view! { <Layout><RecordCornerPage /></Layout> } />
                <Route path=StaticSegment("back-room") view=|| view! { <Layout><BackRoomPage /></Layout> } />
                <Route
                    path=StaticSegment("employees-only")
                    view=|| view! { <Layout><EmployeesOnlyPage /></Layout> }
                />
                <Route
                    path=(StaticSegment("watch"), ParamSegment("id"))
                    view=|| view! { <Layout><WatchPage /></Layout> }
                />
                <Route path=StaticSegment("login") view=LoginPage />
            </Routes>
        </Router>
    }
}
