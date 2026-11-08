//! Primary navigation.

use leptos::prelude::*;
use leptos_router::components::A;

/// Primary navigation entries as `(path, label)`.
pub const NAV_ITEMS: &[(&str, &str)] = &[
    ("/", "Library"),
    ("/search", "Search"),
    ("/queue", "Queue"),
    ("/settings", "Settings"),
];

/// Main site navigation. The router's `<A>` sets `aria-current="page"` on the
/// link for the current route, which the stylesheet also uses for styling.
#[component]
pub fn MainNav() -> impl IntoView {
    view! {
        <nav class="main-nav" aria-label="Main">
            <ul>
                {NAV_ITEMS
                    .iter()
                    .map(|(href, label)| {
                        view! {
                            <li>
                                <A href=*href exact=true>
                                    {*label}
                                </A>
                            </li>
                        }
                    })
                    .collect_view()}
            </ul>
        </nav>
    }
}
