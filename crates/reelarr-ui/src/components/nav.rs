//! Primary navigation.

use leptos::prelude::*;
use leptos_router::hooks::use_location;

/// Primary navigation entries as `(path, label)`.
pub const NAV_ITEMS: &[(&str, &str)] = &[
    ("/", "The Store Floor"),
    ("/counter", "The Special Order Counter"),
    ("/records", "The Record Corner"),
    ("/back-room", "The Back Room"),
    ("/employees-only", "Employees Only"),
];

/// Whether the nav entry `href` is the current section for `path`. Movie
/// detail pages (`/case/...`) and the player (`/watch/...`) belong to the
/// Store Floor.
pub fn is_current(href: &str, path: &str) -> bool {
    let path = path.trim_end_matches('/');
    if href == "/" {
        return path.is_empty() || path.starts_with("/case/") || path.starts_with("/watch/");
    }
    path == href || path.starts_with(&format!("{href}/"))
}

/// Main site navigation. The current section gets `aria-current="page"`,
/// which the stylesheet also uses for styling.
#[component]
pub fn MainNav(
    /// Extra class, e.g. for the mobile drawer.
    #[prop(optional)]
    class: &'static str,
    /// Called after a link is followed, e.g. to close the drawer.
    #[prop(optional, into)]
    on_navigate: Option<Callback<()>>,
) -> AnyView {
    let location = use_location();
    view! {
        <nav class=format!("main-nav {class}") aria-label="Main">
            <ul>
                {NAV_ITEMS
                    .iter()
                    .map(|(href, label)| {
                        let current = move || {
                            is_current(href, &location.pathname.get()).then_some("page")
                        };
                        view! {
                            <li>
                                <a
                                    href=*href
                                    aria-current=current
                                    on:click=move |_| {
                                        if let Some(cb) = on_navigate {
                                            cb.run(());
                                        }
                                    }
                                >
                                    {*label}
                                </a>
                            </li>
                        }
                    })
                    .collect_view()}
            </ul>
        </nav>
    }
    .into_any()
}

#[cfg(test)]
mod tests {
    use super::is_current;

    #[test]
    fn store_floor_covers_detail_and_player() {
        assert!(is_current("/", "/"));
        assert!(is_current("/", "/case/12"));
        assert!(is_current("/", "/watch/3"));
        assert!(!is_current("/", "/counter"));
    }

    #[test]
    fn sections_match_themselves_and_children() {
        assert!(is_current("/back-room", "/back-room"));
        assert!(is_current("/back-room", "/back-room/history"));
        assert!(is_current("/counter", "/counter/"));
        assert!(!is_current("/counter", "/counterfeit"));
    }
}
