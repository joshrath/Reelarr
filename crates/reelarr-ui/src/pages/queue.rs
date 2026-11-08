//! Queue page (placeholder until its Phase 1 screen lands).

use leptos::prelude::*;

use super::placeholder::Placeholder;

#[component]
pub fn QueuePage() -> impl IntoView {
    view! { <Placeholder title="Queue" description="Downloads in progress and recently finished will show up here." /> }
}
