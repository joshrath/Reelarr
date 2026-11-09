//! The Special Order Counter: search and requests.

use leptos::prelude::*;

use super::placeholder::Placeholder;

#[component]
pub fn CounterPage() -> impl IntoView {
    view! {
        <Placeholder
            title="The Special Order Counter"
            description="Search for movies and shows to add to your library."
        />
    }
}
