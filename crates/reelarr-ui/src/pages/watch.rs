//! `/watch/:id`: the VCR player for one media file.

use leptos::prelude::*;
use leptos_meta::Title;
use leptos_router::hooks::use_params_map;
use reelarr_types::PlaybackInfo;

use crate::api::{ApiFailure, get_json};
use crate::components::vcr::VcrPlayer;
use crate::playback::{DeckState, deck_state, parse_media_id, playback_path};

#[component]
pub fn WatchPage() -> impl IntoView {
    let params = use_params_map();
    let id = move || params.with(|p| p.get("id").as_deref().and_then(parse_media_id));

    // Fetched in the browser only, with the session cookie; the server
    // renders the deck in its loading state.
    let info = LocalResource::new(move || {
        let id = id();
        async move {
            match id {
                Some(id) => get_json::<PlaybackInfo>(&playback_path(id)).await,
                None => Err(ApiFailure::Status {
                    status: 404,
                    code: Some("not_found".into()),
                }),
            }
        }
    });
    let retry = Callback::new(move |()| info.refetch());

    view! {
        <Title text="Watch" />
        <section class="page watch" aria-labelledby="page-title">
            <h1 id="page-title" class="visually-hidden">"Watch"</h1>
            <Suspense fallback=|| view! { <VcrPlayer state=DeckState::Loading /> }>
                {move || Suspend::new(async move {
                    let state = deck_state(info.await);
                    view! { <VcrPlayer state on_retry=retry /> }
                })}
            </Suspense>
        </section>
    }
}
