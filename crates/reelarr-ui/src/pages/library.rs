//! Library page (placeholder until its Phase 1 screen lands).

use leptos::prelude::*;

use super::placeholder::Placeholder;

#[component]
pub fn LibraryPage() -> impl IntoView {
    view! { <Placeholder title="Library" description="Your movies will show up here once the library is set up." /> }
}
