//! `/case/:id`: the clamshell case for one tape, front cover and back.
//!
//! The id is a media file id, the one the playback endpoint and `/watch/:id`
//! take. The API has no movie detail yet, so the back of the case shows what
//! the playback answer gives (runtime, format, whether the deck can play it)
//! and says plainly that there's no synopsis on file. It never makes up a
//! title, plot or rating.

use leptos::prelude::*;
use leptos_meta::Title;
use leptos_router::hooks::use_params_map;
use reelarr_types::PlaybackInfo;

use crate::api::{ApiFailure, get_json};
use crate::components::clamshell::CaseArt;
use crate::components::icon::{Icon, IconName};
use crate::library::{
    CaseState, CaseTape, DeckReadiness, case_notice_message, case_state, watch_path,
};
use crate::playback::{Notice, SIGN_IN_PATH, UNSUPPORTED_MESSAGE, parse_media_id, playback_path};

#[component]
pub fn CasePage() -> impl IntoView {
    let params = use_params_map();
    let id = move || params.with(|p| p.get("id").as_deref().and_then(parse_media_id));

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
        <Title text="Case" />
        <section class="page case" aria-labelledby="page-title">
            <Suspense fallback=move || view! { <Case id=id() state=CaseState::Loading /> }>
                {move || Suspend::new(async move {
                    let id = id();
                    let state = case_state(id, info.await);
                    view! { <Case id state on_retry=retry /> }
                })}
            </Suspense>
        </section>
    }
}

#[component]
fn Case(
    id: Option<i64>,
    state: CaseState,
    #[prop(optional, into)] on_retry: Option<Callback<()>>,
) -> AnyView {
    let number = id.map_or_else(|| "—".to_string(), |id| format!("#{id}"));
    let crumbs = view! {
        <nav class="case-crumbs" aria-label="Breadcrumb">
            <a href="/" class="case-back">
                <Icon name=IconName::ArrowBack />
                "Store Floor"
            </a>
            <span class="case-crumb-sep" aria-hidden="true">"/"</span>
            <span class="case-catalog" aria-current="page">
                {format!("Case {number}")}
            </span>
        </nav>
    };
    let body = match state {
        CaseState::Loading => view! {
            <div class="case-grid">
                <div class="case-front">
                    <div class="case-cover">
                        <CaseArt label=format!("Tape {number}") large=true />
                    </div>
                </div>
                <div class="case-back-cover">
                    <h1 id="page-title">{format!("Tape {number}")}</h1>
                    <p class="case-status" role="status">"Loading case…"</p>
                </div>
            </div>
        }
        .into_any(),
        CaseState::Found(tape) => view! { <Found tape /> }.into_any(),
        CaseState::Notice(notice) => view! { <CaseNotice notice on_retry /> }.into_any(),
    };
    view! {
        {crumbs}
        {body}
    }
    .into_any()
}

#[component]
fn Found(tape: CaseTape) -> impl IntoView {
    let title = format!("Tape #{}", tape.id);
    let ready = tape.deck == DeckReadiness::Ready;
    let chip = tape.deck.label();
    let runtime = tape.runtime_minutes.map_or_else(
        || "Runtime unknown".to_string(),
        |m| format!("Runtime: {m} min"),
    );
    let deck_note = match &tape.deck {
        DeckReadiness::Ready => None,
        DeckReadiness::Disc(message) => Some((message.clone(), Vec::new())),
        DeckReadiness::Unsupported(reasons) => {
            Some((UNSUPPORTED_MESSAGE.to_string(), reasons.clone()))
        }
    };
    let format = tape.format.unwrap_or_else(|| "Not reported".into());
    let play_label = if ready {
        "Play movie / insert cassette"
    } else {
        "Open in the VCR"
    };
    view! {
        <Title text=title.clone() />
        <div class="case-grid">
            <div class="case-front">
                <div class="case-cover">
                    <CaseArt label=title.clone() note=chip large=true />
                </div>
                <div class="case-play-panel">
                    <a
                        class="button case-play"
                        class:primary=ready
                        href=watch_path(tape.id)
                    >
                        <Icon name=IconName::PlayCircle />
                        {play_label}
                    </a>
                </div>
            </div>
            <div class="case-back-cover">
                <div class="case-heading">
                    <p class="case-overline label">
                        <span>{format!("Reelarr archive • Case #{}", tape.id)}</span>
                        <span class="case-runtime">{runtime}</span>
                    </p>
                    <h1 id="page-title">{title}</h1>
                    <p class="case-chips">
                        <span class="case-chip" class:case-chip-ready=ready class:case-chip-warn=!ready>
                            <Icon name=if ready { IconName::CheckCircle } else { IconName::Info } />
                            {chip}
                        </span>
                    </p>
                </div>
                {deck_note.map(|(message, reasons)| view! {
                    <div class="case-panel case-panel-warn">
                        <p class="case-panel-title label">"Deck check"</p>
                        <p>{message}</p>
                        {(!reasons.is_empty()).then(|| view! {
                            <ul class="case-reasons">
                                {reasons.into_iter().map(|r| view! { <li>{r}</li> }).collect_view()}
                            </ul>
                        })}
                    </div>
                })}
                <div class="case-panel">
                    <p class="case-panel-title label">"Synopsis // archival reel overview"</p>
                    <p class="muted">
                        "No synopsis on file. Movie details will show here once the library can list them."
                    </p>
                </div>
                <div class="case-panel">
                    <p class="case-panel-title label case-panel-title-primary">"Technical specs"</p>
                    <dl class="case-specs">
                        <div>
                            <dt>"Format"</dt>
                            <dd>{format}</dd>
                        </div>
                        <div>
                            <dt>"Playback"</dt>
                            <dd>{if ready { "Direct play" } else { "Not in this browser" }}</dd>
                        </div>
                    </dl>
                </div>
            </div>
        </div>
    }
}

#[component]
fn CaseNotice(notice: Notice, on_retry: Option<Callback<()>>) -> AnyView {
    let action = match &notice {
        Notice::SignIn => {
            Some(view! { <a class="button primary" href=SIGN_IN_PATH>"Sign in"</a> }.into_any())
        }
        Notice::TryAgain => on_retry.map(|retry| {
            view! {
                <button type="button" class="button primary" on:click=move |_| retry.run(())>
                    "Try again"
                </button>
            }
            .into_any()
        }),
        _ => None,
    };
    let title = match notice {
        Notice::SignIn => "Sign in to see this case",
        ref other => other.title(),
    };
    view! {
        <div class="shelf-card shelf-card-alert case-notice" role="alert">
            <h1 id="page-title" class="shelf-card-title">{title}</h1>
            <p>{case_notice_message(&notice)}</p>
            <p class="shelf-actions">
                {action}
                <a class="button" href="/">"Back to the Store Floor"</a>
            </p>
        </div>
    }
    .into_any()
}
