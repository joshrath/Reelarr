//! The Back Room: the dubbing lab where downloads are worked on.
//!
//! Reads the job list from the job API and keeps it current with the live
//! feed (`GET /api/v1/jobs/events`, Server-Sent Events through the
//! browser's `EventSource`). Pause, resume, cancel and priority use the
//! job endpoints; cancelling asks first. Nothing here is made up: no
//! sample jobs, engines or speeds, only what the server reports. See
//! [`crate::jobs`].

use leptos::prelude::*;
use leptos_meta::Title;
use reelarr_types::{JobList, JobSnapshot, Priority, SessionInfo, SetPriorityRequest};

use crate::api::{Method, get_json, send_json};
use crate::components::confirm::Confirm;
use crate::components::icon::{Icon, IconName};
use crate::jobs::{
    Applied, Feed, Frame, JOBS_PATH, JobAction, JobCard, PRIORITIES, Telemetry, apply_event,
    cancel_question, is_finished, parse_frame, priority_from_value, priority_label, priority_path,
    priority_value, telemetry, waiting_for_import,
};
use crate::library::SETTINGS_PATH;
use crate::playback::SIGN_IN_PATH;
use crate::settings::{MainFolder, import_wait};
use crate::staff::{Gate, LoadProblem, SESSION_PATH, action_error, gate};

/// Job error codes whose static server message a card may repeat.
const JOB_SHOWN_CODES: &[&str] = &["invalid_transition", "job_finished", "job_not_found"];

/// Segments on a deck's level meter.
const METER_SEGMENTS: u8 = 10;

#[component]
pub fn BackRoomPage() -> AnyView {
    // Checked in the browser, with the session cookie.
    let session = LocalResource::new(|| get_json::<SessionInfo>(SESSION_PATH));
    let retry = Callback::new(move |()| session.refetch());
    view! {
        <Title text="The Back Room" />
        <section class="page back-room" aria-labelledby="page-title">
            <header class="lab-head">
                <p class="lab-overline label">
                    <span class="lab-led" aria-hidden="true"></span>
                    "Dubbing lab // Reelarr"
                </p>
                <h1 id="page-title">"The Back Room"</h1>
                <p class="lab-lede">
                    "Cassette dubbing & requisition lab: the downloads Reelarr is working on, and the ones that came back."
                </p>
            </header>
            <Suspense fallback=|| view! { <Lab gate=Gate::Checking /> }>
                {move || Suspend::new(async move {
                    let gate = gate(session.await);
                    view! { <Lab gate on_retry=retry /> }
                })}
            </Suspense>
        </section>
    }
    .into_any()
}

/// The lab for one sign-in state.
#[component]
fn Lab(gate: Gate, #[prop(optional, into)] on_retry: Option<Callback<()>>) -> AnyView {
    match gate {
        Gate::Checking => view! {
            <TelemetryStrip feed=Signal::stored(None) telemetry=Signal::stored(None) />
            <DeckFrame>
                <p class="lab-status" role="status">"Checking the back room…"</p>
            </DeckFrame>
        }
        .into_any(),
        Gate::SignIn => view! {
            <DeckFrame>
                <LabCard alert=true title="Staff only">
                    <p>"Sign in to see what's in the back room."</p>
                    <p class="lab-actions">
                        <a class="button primary" href=SIGN_IN_PATH>"Sign in"</a>
                    </p>
                </LabCard>
            </DeckFrame>
        }
        .into_any(),
        Gate::TryAgain => view! {
            <DeckFrame>
                <LabCard alert=true title="Couldn't open the back room">
                    <p>"Something went wrong checking your session. Try again in a moment."</p>
                    {on_retry.map(|retry| view! {
                        <p class="lab-actions">
                            <button type="button" class="button primary" on:click=move |_| retry.run(())>
                                "Try again"
                            </button>
                        </p>
                    })}
                </LabCard>
            </DeckFrame>
        }
        .into_any(),
        Gate::Open(_) => view! { <JobBoard /> }.into_any(),
    }
}

/// The lab's readouts. Empty dashes until the jobs are known.
#[component]
fn TelemetryStrip(
    feed: Signal<Option<Feed>>,
    telemetry: Signal<Option<Telemetry>>,
) -> impl IntoView {
    let dash = || "—".to_string();
    view! {
        <ul class="lab-telemetry" aria-label="Lab status">
            <li class="lab-readout">
                <span class="lab-readout-label">"Feed"</span>
                <span class="lab-readout-value" class:lab-readout-warn=move || {
                    matches!(feed.get(), Some(Feed::Stopped | Feed::Reconnecting))
                }>
                    {move || feed.get().map_or_else(dash, |f| f.label().to_string())}
                </span>
            </li>
            <li class="lab-readout">
                <span class="lab-readout-label">"On the decks"</span>
                <span class="lab-readout-value">
                    {move || telemetry.get().map_or_else(dash, |t| t.on_decks.to_string())}
                </span>
            </li>
            <li class="lab-readout">
                <span class="lab-readout-label">"Downloading"</span>
                <span class="lab-readout-value">
                    {move || telemetry.get().map_or_else(dash, |t| t.downloading.to_string())}
                </span>
            </li>
            <li class="lab-readout">
                <span class="lab-readout-label">"Throughput"</span>
                <span class="lab-readout-value">
                    {move || telemetry.get().and_then(|t| t.throughput).unwrap_or_else(dash)}
                </span>
            </li>
        </ul>
    }
}

/// The dubbing decks section around its content.
#[component]
fn DeckFrame(children: Children) -> impl IntoView {
    view! {
        <section class="lab-decks" aria-labelledby="decks-title">
            <h2 id="decks-title" class="lab-section-title" tabindex="-1">
                <Icon name=IconName::Downloading />
                "Dubbing decks"
            </h2>
            {children()}
        </section>
    }
}

/// A message card in the lab.
#[component]
fn LabCard(alert: bool, title: &'static str, children: Children) -> impl IntoView {
    view! {
        <div class="lab-card" class:lab-card-alert=alert>
            <p class="lab-card-title">
                <Icon name=if alert { IconName::Info } else { IconName::Videocam } />
                {title}
            </p>
            {children()}
        </div>
    }
}

/// The signed-in lab: telemetry, the decks and the returns shelf.
#[component]
fn JobBoard() -> AnyView {
    // `None` until the first list arrives.
    let jobs = RwSignal::new(None::<Vec<JobSnapshot>>);
    let problem = RwSignal::new(None::<LoadProblem>);
    let feed = RwSignal::new(Feed::Connecting);
    // The job with an action in flight, and the last action error.
    let busy = RwSignal::new(None::<String>);
    let failed = RwSignal::new(None::<(String, String)>);
    let announce = RwSignal::new(String::new());
    // Bumped to reopen the live feed.
    let reconnect = RwSignal::new(0_u32);

    // Re-read the whole list. Calls that land while one is in flight are
    // folded into one more read afterwards.
    let in_flight = StoredValue::new(false);
    let again = StoredValue::new(false);
    let refresh = move || {
        if in_flight.get_value() {
            again.set_value(true);
            return;
        }
        in_flight.set_value(true);
        leptos::task::spawn_local(async move {
            loop {
                again.set_value(false);
                match get_json::<JobList>(JOBS_PATH).await {
                    Ok(list) => {
                        jobs.set(Some(list.jobs));
                        problem.set(None);
                    }
                    Err(failure) => problem.set(Some(LoadProblem::from_failure(&failure))),
                }
                if !again.get_value() {
                    break;
                }
            }
            in_flight.set_value(false);
        });
    };

    let on_frame = move |name: &str, data: &str| match parse_frame(name, data) {
        Some(Frame::Snapshot(list)) => {
            jobs.set(Some(list.jobs));
            problem.set(None);
            feed.set(Feed::Live);
        }
        Some(Frame::Job(event)) => {
            let mut stale = true;
            jobs.update(|list| {
                if let Some(list) = list {
                    stale = apply_event(list, &event) == Applied::Refresh;
                }
            });
            if stale {
                refresh();
            }
        }
        None => {}
    };
    let on_open = move || feed.set(Feed::Live);
    let on_error = move |closed: bool| {
        if closed {
            // The browser gave up (e.g. 401 or 503): read the list once to
            // find out why, and offer a manual reconnect.
            feed.set(Feed::Stopped);
            refresh();
        } else {
            feed.set(Feed::Reconnecting);
        }
    };

    // Browser only: open the feed, reopen it when asked, close it when the
    // page goes away.
    #[cfg(not(feature = "ssr"))]
    {
        use send_wrapper::SendWrapper;
        use std::{cell::RefCell, rc::Rc};
        let stream = Rc::new(RefCell::new(None::<live::Stream>));
        let current = stream.clone();
        Effect::new(move |_| {
            reconnect.track();
            // Dropping the old stream closes it.
            current.borrow_mut().take();
            feed.set(Feed::Connecting);
            let opened = live::Stream::open(
                crate::jobs::JOB_EVENTS_PATH,
                Rc::new(on_frame),
                Rc::new(on_open),
                Rc::new(on_error),
            );
            if opened.is_none() {
                on_error(true);
            }
            *current.borrow_mut() = opened;
        });
        let stream = SendWrapper::new(stream);
        on_cleanup(move || {
            stream.borrow_mut().take();
        });
    }
    #[cfg(feature = "ssr")]
    let _ = (on_frame, on_open, on_error);

    let act = Callback::new(move |(id, title, action): (String, String, JobAction)| {
        let Some(path) = action.path(&id) else {
            return;
        };
        busy.set(Some(id.clone()));
        failed.set(None);
        leptos::task::spawn_local(async move {
            match send_json::<(), JobSnapshot>(Method::Post, &path, None).await {
                Ok(_) => {
                    announce.set(action.done(&title));
                    refresh();
                }
                Err(failure) => failed.set(Some((id, action_error(&failure, JOB_SHOWN_CODES)))),
            }
            busy.set(None);
        });
    });
    let reprioritize = Callback::new(move |(id, title, priority): (String, String, Priority)| {
        let Some(path) = priority_path(&id) else {
            return;
        };
        busy.set(Some(id.clone()));
        failed.set(None);
        leptos::task::spawn_local(async move {
            let body = SetPriorityRequest { priority };
            match send_json::<_, JobSnapshot>(Method::Put, &path, Some(&body)).await {
                Ok(_) => {
                    announce.set(format!(
                        "{title} is now {} priority.",
                        priority_label(priority).to_lowercase()
                    ));
                    refresh();
                }
                Err(failure) => failed.set(Some((id, action_error(&failure, JOB_SHOWN_CODES)))),
            }
            busy.set(None);
        });
    });

    let active_ids = Memo::new(move |_| {
        jobs.with(|list| {
            list.iter()
                .flatten()
                .filter(|j| !is_finished(j.state))
                .map(|j| j.id.clone())
                .collect::<Vec<_>>()
        })
    });
    let finished = Memo::new(move |_| {
        jobs.with(|list| {
            list.iter()
                .flatten()
                .filter(|j| is_finished(j.state))
                .map(JobCard::of)
                .collect::<Vec<_>>()
        })
    });
    let stats = Signal::derive(move || jobs.with(|list| list.as_deref().map(telemetry)));

    // When downloads are waiting to be imported, check whether they have a
    // library folder to go to.
    let waiting =
        Memo::new(move |_| jobs.with(|list| list.as_deref().is_some_and(waiting_for_import)));
    let main = RwSignal::new(None::<MainFolder>);
    #[cfg(not(feature = "ssr"))]
    Effect::new(move |_| {
        if waiting.get() {
            leptos::task::spawn_local(async move {
                if let Ok(list) =
                    get_json::<Vec<reelarr_types::LibraryRootInfo>>(crate::library::ROOTS_PATH)
                        .await
                {
                    main.set(Some(MainFolder::of(&list)));
                }
            });
        } else {
            main.set(None);
        }
    });
    let feed_state = Signal::derive(move || Some(feed.get()));

    view! {
        <TelemetryStrip feed=feed_state telemetry=stats />
        <p class="visually-hidden" role="status" aria-live="polite">{move || announce.get()}</p>
        <div class="lab-toolbar">
            <button type="button" class="button" on:click=move |_| refresh()>
                <Icon name=IconName::Sync />
                "Refresh"
            </button>
            {move || (feed.get() == Feed::Stopped).then(|| view! {
                <button type="button" class="button" on:click=move |_| reconnect.update(|n| *n += 1)>
                    <Icon name=IconName::Bolt />
                    "Reconnect live feed"
                </button>
                <span class="lab-toolbar-note">
                    "Live updates stopped. The list changes when you refresh."
                </span>
            })}
        </div>
        {move || problem.get().map(|problem| view! { <Problem problem has_jobs=jobs.with(Option::is_some) /> })}
        {move || {
            let (title, message) = main.get().filter(|_| waiting.get()).and_then(import_wait)?;
            Some(view! {
                <div class="lab-card lab-card-alert" role="status">
                    <p class="lab-card-title">
                        <Icon name=IconName::Info />
                        {title}
                    </p>
                    <p>{message}</p>
                    <p class="lab-actions">
                        <a class="button primary" href=SETTINGS_PATH>"Go to Employees Only"</a>
                    </p>
                </div>
            })
        }}
        <DeckFrame>
            {move || match jobs.with(|list| list.as_ref().map(Vec::is_empty)) {
                None if problem.get().is_none() => view! {
                    <p class="lab-status" role="status">"Loading the decks…"</p>
                }
                .into_any(),
                None => ().into_any(),
                Some(true) => view! {
                    <LabCard alert=false title="All decks idle">
                        <p>
                            "Nothing is downloading. Downloads you start will be dubbed here, with live progress."
                        </p>
                    </LabCard>
                }
                .into_any(),
                Some(false) => (move || {
                    active_ids.with(Vec::is_empty).then(|| view! {
                        <LabCard alert=false title="All decks idle">
                            <p>"Nothing is downloading right now. Finished jobs are on the returns shelf below."</p>
                        </LabCard>
                    })
                })
                .into_any(),
            }}
            <div class="lab-deck-grid">
                <For
                    each=move || active_ids.get()
                    key=|id| id.clone()
                    let:id
                >
                    <Deck id jobs active_ids busy failed act reprioritize />
                </For>
            </div>
        </DeckFrame>
        <section class="lab-returns" aria-labelledby="returns-title">
            <h2 id="returns-title" class="lab-section-title">
                <Icon name=IconName::Shelves />
                "Returns shelf // Finished jobs"
            </h2>
            {move || {
                let cards = finished.get();
                if cards.is_empty() {
                    view! { <p class="lab-empty">"No finished jobs yet."</p> }.into_any()
                } else {
                    view! {
                        <ul class="lab-returns-list">
                            {cards.into_iter().map(|card| view! { <Returned card /> }).collect_view()}
                        </ul>
                    }
                    .into_any()
                }
            }}
        </section>
    }
    .into_any()
}

/// Why the list couldn't be read.
#[component]
fn Problem(problem: LoadProblem, has_jobs: bool) -> impl IntoView {
    let mut message = problem.message("the job list");
    if has_jobs {
        message.push_str(" The decks below may be out of date.");
    }
    view! {
        <div class="lab-card lab-card-alert" role="alert">
            <p class="lab-card-title">
                <Icon name=IconName::Error />
                {match problem {
                    LoadProblem::SignIn => "Signed out",
                    LoadProblem::Offline => "Back room offline",
                    LoadProblem::TryAgain => "Couldn't read the job list",
                }}
            </p>
            <p>{message}</p>
            {(problem == LoadProblem::SignIn).then(|| view! {
                <p class="lab-actions">
                    <a class="button primary" href=SIGN_IN_PATH>"Sign in"</a>
                </p>
            })}
        </div>
    }
}

/// One dubbing deck: a job that isn't finished.
#[component]
fn Deck(
    id: String,
    jobs: RwSignal<Option<Vec<JobSnapshot>>>,
    active_ids: Memo<Vec<String>>,
    busy: RwSignal<Option<String>>,
    failed: RwSignal<Option<(String, String)>>,
    act: Callback<(String, String, JobAction)>,
    reprioritize: Callback<(String, String, Priority)>,
) -> impl IntoView {
    let key = id.clone();
    let card = Memo::new(move |_| {
        jobs.with(|list| list.iter().flatten().find(|j| j.id == key).map(JobCard::of))
    });
    let Some(initial) = card.get_untracked() else {
        return ().into_any();
    };
    let title = initial.title.clone();
    let title_id = format!("deck-{id}-title");
    let labelled_by = title_id.clone();
    let key = id.clone();
    let number = Memo::new(move |_| {
        active_ids.with(|ids| ids.iter().position(|i| *i == key).map_or(0, |n| n + 1))
    });
    let key = id.clone();
    let is_busy = Signal::derive(move || busy.with(|b| b.as_deref() == Some(key.as_str())));
    let key = id.clone();
    let error = move || {
        failed.with(|f| {
            f.as_ref()
                .filter(|(job, _)| *job == key)
                .map(|(_, message)| message.clone())
        })
    };
    let get =
        move |f: fn(&JobCard) -> Option<String>| move || card.with(|c| c.as_ref().and_then(f));
    let tone = move || card.with(|c| c.as_ref().map_or("waiting", |c| c.tone));
    let percent = move || card.with(|c| c.as_ref().and_then(|c| c.percent));
    let state = move || card.with(|c| c.as_ref().map(|c| c.state));

    // Pause and resume share one key, so focus stays put when it flips.
    let toggle_action = move || match state() {
        Some(s) if JobAction::Resume.allowed(s) => Some(JobAction::Resume),
        Some(s) if JobAction::Pause.allowed(s) => Some(JobAction::Pause),
        _ => None,
    };
    let toggle_id = id.clone();
    let toggle_title = title.clone();
    let on_toggle = move |_| {
        if let Some(action) = toggle_action() {
            act.run((toggle_id.clone(), toggle_title.clone(), action));
        }
    };

    let chosen = RwSignal::new(priority_value(initial.priority).to_string());
    let current_priority = move || card.with(|c| c.as_ref().map(|c| c.priority));
    let select_id = format!("deck-{id}-priority");
    let priority_id = id.clone();
    let priority_title = title.clone();
    let on_priority = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        if let Some(priority) = priority_from_value(&chosen.get_untracked())
            && Some(priority) != current_priority()
        {
            reprioritize.run((priority_id.clone(), priority_title.clone(), priority));
        }
    };

    let cancel_id = id.clone();
    let cancel_title = title.clone();
    let cancel_allowed = move || state().is_some_and(|s| JobAction::Cancel.allowed(s));

    view! {
        <article class=move || format!("lab-deck lab-tone-{}", tone()) aria-labelledby=labelled_by>
            <p class="lab-deck-tag">
                <span class="lab-deck-number">
                    {move || format!("Deck {:02}", number.get())}
                    " // "
                    {initial.kind}
                </span>
                <span class="lab-state">
                    {move || card.with(|c| c.as_ref().map(|c| c.state_label))}
                </span>
            </p>
            <h3 id=title_id class="lab-deck-title">{title.clone()}</h3>
            <p class="lab-deck-meta">
                {move || {
                    card.with(|c| {
                        c.as_ref()
                            .map(|c| {
                                let mut parts = vec![format!("Priority: {}", priority_label(c.priority))];
                                if let Some(category) = &c.category {
                                    parts.push(format!("Category: {category}"));
                                }
                                if let Some(attempts) = &c.attempts {
                                    parts.push(attempts.clone());
                                }
                                parts.join(" • ")
                            })
                            .unwrap_or_default()
                    })
                }}
            </p>
            <div class="lab-meter">
                <p class="lab-meter-readout">
                    <span class="lab-meter-percent">
                        {move || percent().map_or_else(|| "—".to_string(), |p| format!("{p}%"))}
                    </span>
                    <span class="lab-meter-rate">{get(|c| c.rate.clone())}</span>
                </p>
                <div
                    class="lab-meter-bar"
                    role="progressbar"
                    aria-label=format!("Progress of {title}")
                    aria-valuemin="0"
                    aria-valuemax="100"
                    aria-valuenow=move || percent().map(|p| p.to_string())
                    aria-valuetext=move || percent().map_or_else(|| "Not measured yet".to_string(), |p| format!("{p}%"))
                >
                    {(0..METER_SEGMENTS)
                        .map(|i| {
                            view! {
                                <span
                                    class="lab-meter-segment"
                                    class:lab-meter-on=move || {
                                        percent().is_some_and(|p| p >= (i + 1) * (100 / METER_SEGMENTS))
                                    }
                                ></span>
                            }
                        })
                        .collect_view()}
                </div>
                <p class="lab-meter-detail">
                    {move || {
                        card.with(|c| {
                            c.as_ref()
                                .map(|c| {
                                    [c.size.clone(), c.eta.clone(), c.missing.clone()]
                                        .into_iter()
                                        .flatten()
                                        .collect::<Vec<_>>()
                                        .join(" • ")
                                })
                                .unwrap_or_default()
                        })
                    }}
                </p>
            </div>
            <p class="lab-deck-note">{move || card.with(|c| c.as_ref().map(|c| c.note))}</p>
            <div class="lab-deck-controls">
                <button
                    type="button"
                    class="button"
                    disabled=move || is_busy.get() || toggle_action().is_none()
                    aria-label=move || {
                        let verb = if toggle_action() == Some(JobAction::Resume) { "Resume" } else { "Pause" };
                        format!("{verb} {}", title.clone())
                    }
                    on:click=on_toggle
                >
                    {move || {
                        if toggle_action() == Some(JobAction::Resume) {
                            view! { <Icon name=IconName::PlayArrow /> "Resume" }.into_any()
                        } else {
                            view! { <Icon name=IconName::Pause /> "Pause" }.into_any()
                        }
                    }}
                </button>
                <form class="lab-priority" on:submit=on_priority>
                    <label for=select_id.clone()>"Priority"</label>
                    <select
                        id=select_id
                        prop:value=move || chosen.get()
                        on:change=move |ev| chosen.set(event_target_value(&ev))
                        disabled=move || is_busy.get()
                    >
                        {PRIORITIES
                            .into_iter()
                            .map(|p| view! {
                                <option value=priority_value(p) selected=p == initial.priority>
                                    {priority_label(p)}
                                </option>
                            })
                            .collect_view()}
                    </select>
                    <button
                        type="submit"
                        class="button"
                        disabled=move || {
                            is_busy.get()
                                || priority_from_value(&chosen.get()) == current_priority()
                        }
                    >
                        "Set"
                    </button>
                </form>
                <Show when=cancel_allowed>
                    <Confirm
                        id=format!("cancel-{cancel_id}")
                        trigger="Cancel…"
                        trigger_label=format!("Cancel {cancel_title}")
                        question={
                            let title = cancel_title.clone();
                            Signal::derive(move || {
                                state().map(|s| cancel_question(&title, s)).unwrap_or_default()
                            })
                        }
                        confirm="Yes, cancel job"
                        keep="Keep it"
                        on_confirm={
                            let id = cancel_id.clone();
                            let title = cancel_title.clone();
                            move |()| act.run((id.clone(), title.clone(), JobAction::Cancel))
                        }
                        busy=is_busy
                        focus_after="decks-title"
                    />
                </Show>
            </div>
            {move || error().map(|message| view! { <p class="form-error" role="alert">{message}</p> })}
        </article>
    }
    .into_any()
}

/// A finished job on the returns shelf.
#[component]
fn Returned(card: JobCard) -> impl IntoView {
    let failure = card.failure.clone();
    view! {
        <li class=format!("lab-return lab-tone-{}", card.tone)>
            <span class="lab-return-main">
                <span class="lab-return-title">{card.title.clone()}</span>
                <span class="lab-return-meta">
                    {card.kind} " • " {card.updated.clone()}
                    {card.size.clone().map(|s| format!(" • {s}"))}
                </span>
                {failure.map(|f| view! {
                    <span class="lab-return-problem">
                        <strong>{f.label}</strong>
                        ": "
                        {crate::staff::sentence(&f.message)}
                        {f.retryable.then_some(" It may work if tried again.")}
                    </span>
                })}
            </span>
            <span class="lab-state">{card.state_label}</span>
        </li>
    }
}

/// The browser's `EventSource`, with its listeners kept alive.
#[cfg(not(feature = "ssr"))]
mod live {
    use std::rc::Rc;
    use web_sys::wasm_bindgen::{JsCast, closure::Closure};

    type Listener = Closure<dyn FnMut(web_sys::Event)>;
    /// Gets a frame's SSE event name and its data.
    pub type OnFrame = Rc<dyn Fn(&str, &str)>;

    pub struct Stream {
        source: web_sys::EventSource,
        _listeners: Vec<Listener>,
    }

    impl Stream {
        /// Opens the feed. `on_frame` gets each named frame's event name
        /// and data; `on_error` gets whether the browser gave up.
        pub fn open(
            path: &str,
            on_frame: OnFrame,
            on_open: Rc<dyn Fn()>,
            on_error: Rc<dyn Fn(bool)>,
        ) -> Option<Self> {
            // Built first so any early return below drops it, which closes
            // the source instead of leaving a half-wired feed open.
            let mut stream = Self {
                source: web_sys::EventSource::new(path).ok()?,
                _listeners: Vec::new(),
            };
            for name in ["snapshot", "job"] {
                let on_frame = on_frame.clone();
                let listener = Listener::new(move |event: web_sys::Event| {
                    if let Some(data) = event
                        .dyn_ref::<web_sys::MessageEvent>()
                        .and_then(|m| m.data().as_string())
                    {
                        on_frame(name, &data);
                    }
                });
                stream.listen(name, listener)?;
            }
            stream.listen("open", Listener::new(move |_| on_open()))?;
            let watched = stream.source.clone();
            let errored = Listener::new(move |_| {
                on_error(watched.ready_state() == web_sys::EventSource::CLOSED);
            });
            stream.listen("error", errored)?;
            Some(stream)
        }

        /// Attaches `listener` and keeps it alive with the stream.
        fn listen(&mut self, name: &str, listener: Listener) -> Option<()> {
            self.source
                .add_event_listener_with_callback(name, listener.as_ref().unchecked_ref())
                .ok()?;
            self._listeners.push(listener);
            Some(())
        }
    }

    impl Drop for Stream {
        fn drop(&mut self) {
            self.source.close();
        }
    }
}
