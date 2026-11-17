//! Employees Only: settings.

use leptos::prelude::*;

use super::placeholder::Placeholder;

#[component]
pub fn EmployeesOnlyPage() -> impl IntoView {
    view! {
        <Placeholder
            title="Employees Only"
            description="Indexers, Usenet servers, library folders and metadata settings will live here."
        />
    }
}
