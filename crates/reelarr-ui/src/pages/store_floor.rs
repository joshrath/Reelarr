//! The Store Floor: the library home. Shelves land with the library API.

use leptos::prelude::*;

use super::placeholder::Placeholder;

#[component]
pub fn StoreFloorPage() -> impl IntoView {
    view! {
        <Placeholder
            title="The Store Floor"
            description="Your movies and shows will be on the shelves here once the library is set up."
        />
    }
}
