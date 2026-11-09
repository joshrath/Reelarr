//! The Back Room: activity and the download queue.

use leptos::prelude::*;

use super::placeholder::Placeholder;

#[component]
pub fn BackRoomPage() -> impl IntoView {
    view! {
        <Placeholder
            title="The Back Room"
            description="Downloads in progress and recent activity will show up here."
        />
    }
}
