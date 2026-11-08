//! Settings page (placeholder until its Phase 1 screen lands).

use leptos::prelude::*;

use super::placeholder::Placeholder;

#[component]
pub fn SettingsPage() -> impl IntoView {
    view! { <Placeholder title="Settings" description="Indexers, Usenet servers, folders and metadata settings will live here." /> }
}
