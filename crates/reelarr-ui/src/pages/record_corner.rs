//! The Record Corner: music. A placeholder until the music API exists.

use leptos::prelude::*;

use super::placeholder::Placeholder;

#[component]
pub fn RecordCornerPage() -> impl IntoView {
    view! {
        <Placeholder
            title="The Record Corner"
            description="Your music will be in the bins here once music libraries are supported."
        />
    }
}
