//! The Special Order Counter: the clerk's desk where you ask for a title.
//!
//! There's no title search in the API yet, so the desk is honest about it:
//! the search field works and keeps its query in `?q=`, but nothing is
//! looked up and no results are made up. See [`crate::counter`].

use leptos::prelude::*;
use leptos_meta::Title;
use leptos_router::hooks::{use_navigate, use_query_map};
use reelarr_types::SessionInfo;

use crate::api::get_json;
use crate::components::icon::{Icon, IconName};
use crate::counter::{
    BACK_ROOM_PATH, DeskState, Format, Results, SESSION_PATH, counter_href, desk_state,
    normalize_query,
};
use crate::playback::SIGN_IN_PATH;

/// Id of the search field, for its label and for focusing it.
const SEARCH_INPUT_ID: &str = "counter-q";

#[component]
pub fn CounterPage() -> AnyView {
    let query_map = use_query_map();
    // The query that was asked for, from the address bar.
    let asked = Memo::new(move |_| normalize_query(&query_map.read().get("q").unwrap_or_default()));
    // What's in the field right now.
    let draft = RwSignal::new(asked.get_untracked());
    // Keep the field in step when the address changes (back/forward, or the
    // header's search link). Browser only, like every effect here: skipped on
    // the server even when a build turns on both `ssr` and `hydrate`.
    #[cfg(not(feature = "ssr"))]
    Effect::new(move |_| draft.set(asked.get()));
    let format = RwSignal::new(Format::default());

    let input = NodeRef::<leptos::html::Input>::new();
    // Landing on the counter (from the header search or Ctrl/⌘+K) puts the
    // cursor in the field.
    #[cfg(not(feature = "ssr"))]
    Effect::new(move |_| {
        if let Some(el) = input.get() {
            let _ = el.focus();
        }
    });

    let navigate = use_navigate();
    let go = Callback::new(move |query: String| {
        navigate(&counter_href(&query), Default::default());
    });
    let on_submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        go.run(draft.get_untracked());
    };
    let on_clear = move |_| {
        draft.set(String::new());
        if !asked.get_untracked().is_empty() {
            go.run(String::new());
        }
        if let Some(el) = input.get_untracked() {
            let _ = el.focus();
        }
    };
    let clear_query = Callback::new(move |()| on_clear(()));

    // Fetched in the browser only, with the session cookie.
    let session = LocalResource::new(|| get_json::<SessionInfo>(SESSION_PATH));
    let retry = Callback::new(move |()| session.refetch());

    view! {
        <Title text="The Special Order Counter" />
        <section class="page counter" aria-labelledby="page-title">
            <div class="counter-strip">
                <p class="counter-strip-label label">"Counter terminal // Reelarr"</p>
                <ul class="counter-strip-chips" aria-label="Desk status">
                    <li class="counter-chip">
                        <Icon name=IconName::Hub />
                        "Catalog search: not connected yet"
                    </li>
                    <li class="counter-chip counter-chip-warm">
                        <Icon name=IconName::PostAdd />
                        "Special orders: not open yet"
                    </li>
                </ul>
            </div>

            <div class="counter-desk">
                <div class="counter-desk-head">
                    <div class="counter-desk-title">
                        <p class="counter-overline label">
                            <Icon name=IconName::ConfirmationNumber />
                            "Customer & clerk desk"
                        </p>
                        <h1 id="page-title">"The Special Order Counter"</h1>
                        <p class="counter-lede">
                            "Ask for a title to find it on your shelves, or put in a special order for one you don't have yet."
                        </p>
                    </div>
                    <div class="counter-relay">
                        <div>
                            <p class="counter-relay-label label">"Catalog lookup"</p>
                            <p class="counter-relay-value">"Coming soon"</p>
                        </div>
                        <Icon name=IconName::Hub />
                    </div>
                </div>

                <div class="counter-terminal">
                    <form
                        class="counter-search"
                        role="search"
                        action="/counter"
                        method="get"
                        on:submit=on_submit
                    >
                        <div class="counter-field">
                            <Icon name=IconName::Search />
                            <label class="visually-hidden" for=SEARCH_INPUT_ID>
                                "Search the archive"
                            </label>
                            <input
                                id=SEARCH_INPUT_ID
                                node_ref=input
                                name="q"
                                type="search"
                                autocomplete="off"
                                spellcheck="false"
                                maxlength="200"
                                placeholder="_Search archive…"
                                value=draft.get_untracked()
                                bind:value=draft
                            />
                            <button
                                type="button"
                                class="counter-clear"
                                aria-label="Clear search"
                                on:click=move |_| on_clear(())
                            >
                                "Clr"
                            </button>
                        </div>
                        <button type="submit" class="button primary counter-submit">
                            "Look up"
                        </button>
                    </form>
                    <div class="counter-formats" role="group" aria-label="Formats">
                        {Format::ALL
                            .into_iter()
                            .map(|f| {
                                view! {
                                    <button
                                        type="button"
                                        class="counter-format"
                                        aria-pressed=move || (format.get() == f).to_string()
                                        on:click=move |_| format.set(f)
                                    >
                                        {f.label()}
                                    </button>
                                }
                            })
                            .collect_view()}
                    </div>
                </div>
            </div>

            <div class="counter-workspace">
                <section class="counter-results" aria-labelledby="results-title">
                    <h2 id="results-title" class="counter-bar">
                        <Icon name=IconName::Shelves />
                        "Search results & shelf inventory"
                    </h2>
                    <div class="counter-results-body" role="status" aria-live="polite">
                        <Suspense fallback=move || {
                            view! { <Desk state=DeskState::Checking asked /> }
                        }>
                            {move || Suspend::new(async move {
                                let state = desk_state(session.await);
                                view! { <Desk state asked on_retry=retry on_clear=clear_query /> }
                            })}
                        </Suspense>
                    </div>
                </section>
                <Clipboard />
            </div>
        </section>
    }
    .into_any()
}

/// The results area for one desk state.
#[component]
fn Desk(
    state: DeskState,
    asked: Memo<String>,
    #[prop(optional, into)] on_retry: Option<Callback<()>>,
    #[prop(optional, into)] on_clear: Option<Callback<()>>,
) -> AnyView {
    match state {
        DeskState::Checking => view! {
            <p class="counter-status">"Checking the counter…"</p>
        }
        .into_any(),
        DeskState::SignIn => view! {
            <Slip alert=true badge="Sign in" title="Sign in to use the counter".to_string()>
                <p>"Sign in to ask the clerk about titles."</p>
                <p class="counter-actions">
                    <a class="button primary" href=SIGN_IN_PATH>"Sign in"</a>
                </p>
            </Slip>
        }
        .into_any(),
        DeskState::TryAgain => view! {
            <Slip alert=true badge="Desk closed" title="Couldn't reach the counter".to_string()>
                <p>"Something went wrong opening the counter. Try again in a moment."</p>
                {on_retry.map(|retry| view! {
                    <p class="counter-actions">
                        <button type="button" class="button primary" on:click=move |_| retry.run(())>
                            "Try again"
                        </button>
                    </p>
                })}
            </Slip>
        }
        .into_any(),
        DeskState::Open => (move || {
            let results = Results::for_query(&asked.get());
            let action = match &results {
                Results::Idle => view! {
                    <a class="button" href="/">"Browse the Store Floor"</a>
                }
                .into_any(),
                Results::NotSearched(_) => match on_clear {
                    Some(clear) => view! {
                        <button type="button" class="button" on:click=move |_| clear.run(())>
                            "Clear search"
                        </button>
                    }
                    .into_any(),
                    None => ().into_any(),
                },
            };
            view! {
                <Slip alert=false badge=results.badge() title=results.title()>
                    <p>{results.message()}</p>
                    <p class="counter-actions">{action}</p>
                </Slip>
            }
        })
        .into_any(),
    }
}

/// One result row: an empty case on the left, the slip's text on the right.
#[component]
fn Slip(alert: bool, badge: &'static str, title: String, children: Children) -> impl IntoView {
    view! {
        <article class="counter-slip" class:counter-slip-alert=alert>
            <span class="counter-slip-case" aria-hidden="true">
                <Icon name=IconName::Movie />
            </span>
            <div class="counter-slip-body">
                <p class="counter-badge">
                    <Icon name=if alert { IconName::Info } else { IconName::Search } />
                    {badge}
                </p>
                <h3 class="counter-slip-title">{title}</h3>
                {children()}
            </div>
        </article>
    }
}

/// The desk clipboard where order slips will be pinned. Empty for now.
#[component]
fn Clipboard() -> impl IntoView {
    view! {
        <aside class="counter-clipboard" aria-labelledby="clipboard-title">
            <h2 id="clipboard-title" class="counter-clipboard-head">
                <span class="counter-clip" aria-hidden="true"></span>
                "Desk clipboard // Active slips"
            </h2>
            <div class="counter-paper">
                <p class="counter-paper-title">"No order slips"</p>
                <p>
                    "Special orders aren't open yet. When they are, the slips you file here will be pinned to this clipboard."
                </p>
            </div>
            <a class="counter-clipboard-link" href=BACK_ROOM_PATH>
                "View the Back Room"
                <Icon name=IconName::ArrowForward />
            </a>
        </aside>
    }
}
