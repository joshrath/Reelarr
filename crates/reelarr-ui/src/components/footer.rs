//! The fixed bottom deck: a mini-player for whatever is playing.
//!
//! Nothing feeds it yet, so it shows an empty deck with its controls
//! disabled. It will pick up the now-playing item when music and video
//! playback report one.

use leptos::prelude::*;

use super::icon::{Icon, IconName};

#[component]
pub fn Footer() -> AnyView {
    view! {
        <footer class="app-footer">
            <section class="deck" aria-label="Now playing">
                <div class="deck-now">
                    <span class="deck-cassette" aria-hidden="true">
                        <Icon name=IconName::MobileShareStack />
                    </span>
                    <div class="deck-title">
                        <span class="deck-track">"No tape in the deck"</span>
                        <span class="deck-artist">"Pick something from the shelves to play it here."</span>
                    </div>
                </div>
                <div class="deck-transport">
                    <div class="deck-buttons">
                        <button type="button" class="deck-button" disabled=true>
                            <Icon name=IconName::FastRewind label="Rewind" />
                        </button>
                        <button type="button" class="deck-button deck-play" disabled=true>
                            <Icon name=IconName::PlayArrow label="Play" />
                        </button>
                        <button type="button" class="deck-button" disabled=true>
                            <Icon name=IconName::Stop label="Stop" />
                        </button>
                        <button type="button" class="deck-button" disabled=true>
                            <Icon name=IconName::FastForward label="Fast forward" />
                        </button>
                    </div>
                    <p class="deck-counter">
                        <span class="deck-counter-label">"Tape:"</span>
                        " --:-- "
                        <span class="deck-counter-sep">"/"</span>
                        " --:--"
                    </p>
                </div>
                <div class="deck-levels" aria-hidden="true">
                    <div class="vu">
                        <span class="vu-row"><span class="vu-label">"L"</span>{vu_bars()}</span>
                        <span class="vu-row"><span class="vu-label">"R"</span>{vu_bars()}</span>
                    </div>
                    <span class="deck-volume">
                        <Icon name=IconName::VolumeUp />
                        <span class="deck-volume-track"><span class="deck-volume-fill"></span></span>
                    </span>
                </div>
            </section>
        </footer>
    }
    .into_any()
}

fn vu_bars() -> impl IntoView {
    view! { <span class="vu-bars">{(0..6).map(|_| view! { <span class="vu-bar"></span> }).collect_view()}</span> }
}
