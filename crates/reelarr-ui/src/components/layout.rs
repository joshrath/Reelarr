//! The application frame: skip link, header, main region and the deck.

use leptos::prelude::*;

use super::footer::Footer;
use super::header::Header;

/// Id of the `<main>` element targeted by the skip link.
pub const MAIN_ID: &str = "main-content";

/// Wraps a page in the accessible app shell.
#[component]
pub fn Layout(children: Children) -> impl IntoView {
    view! {
        <a class="skip-link" href=format!("#{MAIN_ID}")>"Skip to content"</a>
        <Header />
        <main id=MAIN_ID class="app-main" tabindex="-1">
            {children()}
        </main>
        <Footer />
    }
}
