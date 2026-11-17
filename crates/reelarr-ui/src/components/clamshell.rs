//! A VHS clamshell case from the design: a hinged spine on the left, the
//! cover face, a "Be kind • Rewind" sticker and a shelf label.
//!
//! There's no cover art in the API yet, so the face is the empty-case
//! treatment: plain moulded plastic with a tape icon, never a stand-in
//! poster.

use leptos::prelude::*;

use super::icon::{Icon, IconName};

/// The case artwork. Decorative: pages put the real title and status in
/// text next to it.
#[component]
pub fn CaseArt(
    /// Shelf label line, e.g. "Tape #12".
    #[prop(into)]
    label: String,
    /// Second label line, e.g. "Not yet playable".
    #[prop(optional, into)]
    note: Option<String>,
    /// The large size for the case page.
    #[prop(optional)]
    large: bool,
) -> impl IntoView {
    view! {
        <div class="clamshell" class:clamshell-large=large aria-hidden="true">
            <span class="clamshell-spine"></span>
            <span class="clamshell-face">
                <span class="clamshell-blank">
                    <Icon name=IconName::Movie />
                    <span>"No cover art"</span>
                </span>
            </span>
            <span class="clamshell-sticker">"Be kind • Rewind"</span>
            <span class="clamshell-label">
                <span class="clamshell-label-main">{label}</span>
                {note.map(|note| view! { <span class="clamshell-label-note">{note}</span> })}
                <span class="clamshell-label-small">"Property of Reelarr"</span>
            </span>
        </div>
    }
}

/// An empty spot on the shelf, for a store with nothing stocked yet.
#[component]
pub fn EmptySlot() -> impl IntoView {
    view! { <span class="shelf-slot" aria-hidden="true"></span> }
}
