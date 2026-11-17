//! Employees Only: the staff console for the settings Reelarr has.
//!
//! - Library folders: `GET`/`POST /api/v1/library/roots`,
//!   `POST /api/v1/library/roots/{id}/check` and
//!   `DELETE /api/v1/library/roots/{id}` (confirmed first).
//! - Usenet server: `GET`/`PUT`/`DELETE /api/v1/settings/nntp` and
//!   `POST /api/v1/settings/nntp/test`. The password is write-only and is
//!   never put back into the page.
//!
//! Parts of the design with no API yet (disk usage, metadata key,
//! indexers, staff accounts, quality) are listed as not connected yet,
//! with no sample values. See [`crate::settings`].

use leptos::prelude::*;
use leptos_meta::Title;
use reelarr_types::{
    LibraryRootInfo, LibraryRootKind, SessionInfo,
    nntp::{NntpServerSettings, NntpTestResult},
};

use crate::api::{ApiFailure, Method, get_json, send_empty, send_json};
use crate::components::confirm::Confirm;
use crate::components::icon::{Icon, IconName};
use crate::library::FolderHealth;
use crate::playback::SIGN_IN_PATH;
use crate::settings::{
    NNTP_PATH, NNTP_SHOWN_CODES, NNTP_TEST_PATH, NntpDraft, NntpField, RESCAN_CHOICES,
    ROOT_SHOWN_CODES, ROOTS_PATH, RootRow, kind_value, new_root, nntp_summary, password_status,
    root_check_path, root_remove_path, test_result,
};
use crate::staff::{Gate, LoadProblem, SESSION_PATH, action_error, gate};

#[component]
pub fn EmployeesOnlyPage() -> AnyView {
    // Checked in the browser, with the session cookie.
    let session = LocalResource::new(|| get_json::<SessionInfo>(SESSION_PATH));
    let retry = Callback::new(move |()| session.refetch());
    view! {
        <Title text="Employees Only" />
        <section class="page employees" aria-labelledby="page-title">
            <header class="staff-head">
                <span class="staff-head-icon" aria-hidden="true">
                    <Icon name=IconName::AdminPanelSettings />
                </span>
                <div class="staff-head-title">
                    <p class="staff-overline label">
                        "System configuration"
                        <span class="staff-clearance">"Staff only"</span>
                    </p>
                    <h1 id="page-title">"Employees Only"</h1>
                    <p class="staff-lede">
                        "Where the store gets set up: the folders Reelarr stocks its shelves from, and the Usenet server it downloads through."
                    </p>
                </div>
            </header>
            <Suspense fallback=|| view! { <Console gate=Gate::Checking /> }>
                {move || Suspend::new(async move {
                    let gate = gate(session.await);
                    view! { <Console gate on_retry=retry /> }
                })}
            </Suspense>
            <NotConnected />
        </section>
    }
    .into_any()
}

/// The managed settings for one sign-in state.
#[component]
fn Console(gate: Gate, #[prop(optional, into)] on_retry: Option<Callback<()>>) -> AnyView {
    match gate {
        Gate::Checking => view! {
            <RootsFrame>
                <p class="staff-status" role="status">"Checking your staff badge…"</p>
            </RootsFrame>
            <NntpFrame>
                <p class="staff-status">"Checking your staff badge…"</p>
            </NntpFrame>
        }
        .into_any(),
        Gate::SignIn => view! {
            <div class="staff-card staff-card-alert">
                <p class="staff-card-title">
                    <Icon name=IconName::Info />
                    "Staff only"
                </p>
                <p>"Sign in to see and change Reelarr's library folders and Usenet server."</p>
                <p class="staff-actions">
                    <a class="button primary" href=SIGN_IN_PATH>"Sign in"</a>
                </p>
            </div>
        }
        .into_any(),
        Gate::TryAgain => view! {
            <div class="staff-card staff-card-alert">
                <p class="staff-card-title">
                    <Icon name=IconName::Info />
                    "Couldn't check your badge"
                </p>
                <p>"Something went wrong checking your session. Try again in a moment."</p>
                {on_retry.map(|retry| view! {
                    <p class="staff-actions">
                        <button type="button" class="button primary" on:click=move |_| retry.run(())>
                            "Try again"
                        </button>
                    </p>
                })}
            </div>
        }
        .into_any(),
        Gate::Open(username) => view! {
            <p class="staff-signed-in">
                <Icon name=IconName::Person />
                "Signed in as "
                <strong>{username}</strong>
            </p>
            <RootsSection />
            <NntpSection />
        }
        .into_any(),
    }
}

/// A sign-in or offline message for one section.
#[component]
fn SectionProblem(problem: LoadProblem, what: &'static str) -> impl IntoView {
    view! {
        <div class="staff-card staff-card-alert" role="alert">
            <p>{problem.message(what)}</p>
            {(problem == LoadProblem::SignIn).then(|| view! {
                <p class="staff-actions">
                    <a class="button primary" href=SIGN_IN_PATH>"Sign in"</a>
                </p>
            })}
        </div>
    }
}

#[component]
fn RootsFrame(children: Children) -> impl IntoView {
    view! {
        <section class="staff-panel" aria-labelledby="roots-title">
            <h2 id="roots-title" class="staff-panel-title">
                <Icon name=IconName::HardDrive />
                "Media storage vaults // Library folders"
            </h2>
            {children()}
        </section>
    }
}

#[component]
fn NntpFrame(children: Children) -> impl IntoView {
    view! {
        <section class="staff-panel" aria-labelledby="nntp-title">
            <h2 id="nntp-title" class="staff-panel-title">
                <Icon name=IconName::Hub />
                "Wire room // Usenet server"
            </h2>
            {children()}
        </section>
    }
}

/// Library folders: the list with health, and the add form.
#[component]
fn RootsSection() -> AnyView {
    let roots = RwSignal::new(None::<Result<Vec<LibraryRootInfo>, LoadProblem>>);
    let busy = RwSignal::new(None::<i64>);
    let row_error = RwSignal::new(None::<(i64, String)>);
    let announce = RwSignal::new(String::new());

    let load = move || {
        leptos::task::spawn_local(async move {
            let result = get_json::<Vec<LibraryRootInfo>>(ROOTS_PATH)
                .await
                .map_err(|f| LoadProblem::from_failure(&f));
            roots.set(Some(result));
        });
    };
    // Browser only.
    #[cfg(not(feature = "ssr"))]
    Effect::new(move |_| load());

    let replace = move |info: LibraryRootInfo| {
        roots.update(|r| {
            if let Some(Ok(list)) = r {
                match list.iter_mut().find(|i| i.root.id == info.root.id) {
                    Some(slot) => *slot = info,
                    None => list.push(info),
                }
            }
        });
    };

    let check = Callback::new(move |(id, path): (i64, String)| {
        busy.set(Some(id));
        row_error.set(None);
        leptos::task::spawn_local(async move {
            match send_json::<(), LibraryRootInfo>(Method::Post, &root_check_path(id), None).await {
                Ok(info) => {
                    let health = FolderHealth::from_root(&info.health);
                    announce.set(format!("Checked {path}: {}.", health.label()));
                    replace(info);
                }
                Err(failure) => row_error.set(Some((id, action_error(&failure, ROOT_SHOWN_CODES)))),
            }
            busy.set(None);
        });
    });
    let remove = Callback::new(move |(id, path, forget): (i64, String, bool)| {
        busy.set(Some(id));
        row_error.set(None);
        leptos::task::spawn_local(async move {
            match send_empty::<()>(Method::Delete, &root_remove_path(id, forget), None).await {
                Ok(()) => {
                    roots.update(|r| {
                        if let Some(Ok(list)) = r {
                            list.retain(|i| i.root.id.0 != id);
                        }
                    });
                    announce.set(format!("Removed {path}."));
                }
                Err(failure) => {
                    if failure.code() == Some("root_has_files") {
                        // Files were found since the list was read: show the
                        // new count so the next confirm says it.
                        load();
                    }
                    row_error.set(Some((id, action_error(&failure, ROOT_SHOWN_CODES))));
                }
            }
            busy.set(None);
        });
    });

    let ids = Memo::new(move |_| {
        roots.with(|r| match r {
            Some(Ok(list)) => list.iter().map(|i| i.root.id.0).collect::<Vec<_>>(),
            _ => Vec::new(),
        })
    });

    view! {
        <RootsFrame>
            <p class="visually-hidden" role="status" aria-live="polite">{move || announce.get()}</p>
            {move || match roots.with(|r| r.as_ref().map(|r| r.as_ref().map(Vec::is_empty).map_err(|p| *p))) {
                None => view! {
                    <p class="staff-status" role="status">"Reading the library folders…"</p>
                }
                .into_any(),
                Some(Err(problem)) => view! {
                    <SectionProblem problem what="the library folders" />
                }
                .into_any(),
                Some(Ok(true)) => view! {
                    <div class="staff-card">
                        <p class="staff-card-title">"No library folders yet"</p>
                        <p>
                            "Add the folder where your movies live, as Reelarr sees it inside its container. The Store Floor stocks its shelves from these folders."
                        </p>
                    </div>
                }
                .into_any(),
                Some(Ok(false)) => ().into_any(),
            }}
            <ul class="vault-list" class:vault-list-empty=move || ids.with(Vec::is_empty)>
                <For each=move || ids.get() key=|id| *id let:id>
                    <RootItem id roots busy row_error check remove />
                </For>
            </ul>
            <p class="staff-note">
                <Icon name=IconName::Info />
                "Disk usage for these folders isn't reported yet."
            </p>
            {move || roots.with(|r| matches!(r, Some(Ok(_)))).then(|| view! {
                <AddRoot on_added=Callback::new(move |info: LibraryRootInfo| {
                    let row = RootRow::of(&info);
                    announce.set(format!("Added {}: {}.", row.path, row.health.label()));
                    replace(info);
                }) />
            })}
        </RootsFrame>
    }
    .into_any()
}

/// One library folder with its health and actions.
#[component]
fn RootItem(
    id: i64,
    roots: RwSignal<Option<Result<Vec<LibraryRootInfo>, LoadProblem>>>,
    busy: RwSignal<Option<i64>>,
    row_error: RwSignal<Option<(i64, String)>>,
    check: Callback<(i64, String)>,
    remove: Callback<(i64, String, bool)>,
) -> AnyView {
    let row = Memo::new(move |_| {
        roots.with(|r| match r {
            Some(Ok(list)) => list.iter().find(|i| i.root.id.0 == id).map(RootRow::of),
            _ => None,
        })
    });
    let Some(initial) = row.get_untracked() else {
        return ().into_any();
    };
    let path = initial.path.clone();
    let is_busy = Signal::derive(move || busy.get() == Some(id));
    let health = move || row.with(|r| r.as_ref().map(|r| r.health.clone()));
    let tone = move || match health() {
        Some(FolderHealth::Online) => "ok",
        Some(FolderHealth::Offline { .. } | FolderHealth::PermissionDenied { .. }) => "bad",
        _ => "unknown",
    };
    let problem = move || row.with(|r| r.as_ref().and_then(|r| r.health.problem()));
    let error = move || {
        row_error.with(|e| {
            e.as_ref()
                .filter(|(row, _)| *row == id)
                .map(|(_, m)| m.clone())
        })
    };
    let check_path = path.clone();
    let remove_path = path.clone();

    view! {
        <li class="vault" class:vault-problem=move || problem().is_some()>
            <span class="vault-icon" aria-hidden="true">
                <Icon name=IconName::HardDrive />
            </span>
            <div class="vault-main">
                <p class="vault-path">{path.clone()}</p>
                <p class="vault-facts">
                    <span class="vault-kind">{initial.kind}</span>
                    <span>{move || row.with(|r| r.as_ref().map(RootRow::files_label))}</span>
                    <span>{initial.rescan.clone()}</span>
                    <span>{move || row.with(|r| r.as_ref().map(|r| r.checked.clone()))}</span>
                </p>
                {move || problem().map(|p| view! { <p class="vault-note">{p}</p> })}
                {move || error().map(|m| view! { <p class="form-error" role="alert">{m}</p> })}
            </div>
            <div class="vault-side">
                <span class=move || format!("health-chip health-{}", tone())>
                    {move || match tone() {
                        "ok" => view! { <Icon name=IconName::CheckCircle /> }.into_any(),
                        "bad" => view! { <Icon name=IconName::Error /> }.into_any(),
                        _ => view! { <Icon name=IconName::Info /> }.into_any(),
                    }}
                    {move || health().map(|h| h.label())}
                </span>
                <div class="vault-actions">
                    <button
                        type="button"
                        class="button"
                        aria-label=format!("Check {path} now")
                        disabled=move || is_busy.get()
                        on:click=move |_| check.run((id, check_path.clone()))
                    >
                        <Icon name=IconName::Sync />
                        {move || if is_busy.get() { "Working…" } else { "Check now" }}
                    </button>
                    {move || {
                        let current = row.get()?;
                        let forget = current.forget_files();
                        let path = remove_path.clone();
                        Some(view! {
                            <Confirm
                                id=format!("remove-root-{id}")
                                trigger="Remove…"
                                trigger_label=format!("Remove {}", current.path)
                                question=current.remove_question()
                                confirm="Yes, remove folder"
                                keep="Keep it"
                                on_confirm=move |()| remove.run((id, path.clone(), forget))
                                busy=is_busy
                            />
                        })
                    }}
                </div>
            </div>
        </li>
    }
    .into_any()
}

/// The add-a-folder form.
#[component]
fn AddRoot(on_added: Callback<LibraryRootInfo>) -> impl IntoView {
    let path = RwSignal::new(String::new());
    let kind = RwSignal::new(kind_value(LibraryRootKind::Library).to_string());
    let rescan = RwSignal::new(RESCAN_CHOICES[0].0.to_string());
    let error = RwSignal::new(None::<String>);
    let path_invalid = RwSignal::new(false);
    let busy = RwSignal::new(false);

    let on_submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let new = match new_root(
            &path.get_untracked(),
            &kind.get_untracked(),
            &rescan.get_untracked(),
        ) {
            Ok(new) => new,
            Err(message) => {
                path_invalid.set(true);
                error.set(Some(message.into()));
                return;
            }
        };
        path_invalid.set(false);
        error.set(None);
        busy.set(true);
        leptos::task::spawn_local(async move {
            match send_json::<_, LibraryRootInfo>(Method::Post, ROOTS_PATH, Some(&new)).await {
                Ok(info) => {
                    path.set(String::new());
                    on_added.run(info);
                }
                Err(failure) => {
                    path_invalid.set(matches!(
                        failure.code(),
                        Some("invalid_path" | "duplicate_root" | "overlapping_root")
                    ));
                    error.set(Some(action_error(&failure, ROOT_SHOWN_CODES)));
                }
            }
            busy.set(false);
        });
    };

    let kinds = [
        (LibraryRootKind::Library, "Organized movies (library)"),
        (LibraryRootKind::ManualImport, "Files to import by hand"),
    ];

    view! {
        <form class="staff-form vault-form" on:submit=on_submit aria-labelledby="add-root-title">
            <h3 id="add-root-title" class="staff-form-title">"Add a library folder"</h3>
            <div class="staff-field staff-field-wide">
                <label for="root-path">"Folder path"</label>
                <input
                    id="root-path"
                    name="path"
                    type="text"
                    required
                    autocomplete="off"
                    spellcheck="false"
                    placeholder="/absolute/path"
                    aria-describedby="root-path-hint root-form-error"
                    aria-invalid=move || path_invalid.get().then_some("true")
                    bind:value=path
                />
                <p id="root-path-hint" class="staff-hint">
                    "The full path inside the Reelarr container, such as where a share is mounted."
                </p>
            </div>
            <fieldset class="staff-field staff-choices">
                <legend>"Holds"</legend>
                {kinds
                    .into_iter()
                    .map(|(k, label)| {
                        let value = kind_value(k);
                        let input_id = format!("root-kind-{value}");
                        let label_for = input_id.clone();
                        view! {
                            <label class="staff-choice" for=label_for>
                                <input
                                    id=input_id
                                    type="radio"
                                    name="kind"
                                    value=value
                                    prop:checked=move || kind.get() == value
                                    on:change=move |_| kind.set(value.to_string())
                                />
                                {label}
                            </label>
                        }
                    })
                    .collect_view()}
            </fieldset>
            <div class="staff-field">
                <label for="root-rescan">"Rescan"</label>
                <select id="root-rescan" name="rescan" on:change=move |ev| rescan.set(event_target_value(&ev))>
                    {RESCAN_CHOICES
                        .into_iter()
                        .map(|(secs, label)| view! {
                            <option value=secs.to_string() selected=secs == RESCAN_CHOICES[0].0>{label}</option>
                        })
                        .collect_view()}
                </select>
            </div>
            <p id="root-form-error" class="form-error" role="alert">{move || error.get()}</p>
            <div class="staff-actions">
                <button type="submit" class="button primary" disabled=move || busy.get()>
                    <Icon name=IconName::Check />
                    {move || if busy.get() { "Adding…" } else { "Add folder" }}
                </button>
            </div>
        </form>
    }
}

/// The text fields of the NNTP form.
#[derive(Clone, Copy)]
struct NntpFields {
    enabled: RwSignal<bool>,
    host: RwSignal<String>,
    port: RwSignal<String>,
    username: RwSignal<String>,
    password: RwSignal<String>,
    clear_password: RwSignal<bool>,
    max_connections: RwSignal<String>,
    connect_timeout_secs: RwSignal<String>,
    io_timeout_secs: RwSignal<String>,
    max_retries: RwSignal<String>,
}

impl NntpFields {
    fn new() -> Self {
        let d = NntpDraft::default();
        Self {
            enabled: RwSignal::new(d.enabled),
            host: RwSignal::new(d.host),
            port: RwSignal::new(d.port),
            username: RwSignal::new(d.username),
            password: RwSignal::new(d.password),
            clear_password: RwSignal::new(d.clear_password),
            max_connections: RwSignal::new(d.max_connections),
            connect_timeout_secs: RwSignal::new(d.connect_timeout_secs),
            io_timeout_secs: RwSignal::new(d.io_timeout_secs),
            max_retries: RwSignal::new(d.max_retries),
        }
    }

    fn fill(&self, d: NntpDraft) {
        self.enabled.set(d.enabled);
        self.host.set(d.host);
        self.port.set(d.port);
        self.username.set(d.username);
        self.password.set(d.password);
        self.clear_password.set(d.clear_password);
        self.max_connections.set(d.max_connections);
        self.connect_timeout_secs.set(d.connect_timeout_secs);
        self.io_timeout_secs.set(d.io_timeout_secs);
        self.max_retries.set(d.max_retries);
    }

    fn read(&self) -> NntpDraft {
        NntpDraft {
            enabled: self.enabled.get_untracked(),
            host: self.host.get_untracked(),
            port: self.port.get_untracked(),
            username: self.username.get_untracked(),
            password: self.password.get_untracked(),
            clear_password: self.clear_password.get_untracked(),
            max_connections: self.max_connections.get_untracked(),
            connect_timeout_secs: self.connect_timeout_secs.get_untracked(),
            io_timeout_secs: self.io_timeout_secs.get_untracked(),
            max_retries: self.max_retries.get_untracked(),
        }
    }
}

/// The result line under the NNTP form.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Outcome {
    ok: bool,
    title: String,
    detail: String,
}

/// Usenet server settings: read, save, test and remove.
#[component]
fn NntpSection() -> AnyView {
    // `Some(Ok(None))`: nothing is set up yet.
    let saved = RwSignal::new(None::<Result<Option<NntpServerSettings>, LoadProblem>>);
    let fields = NntpFields::new();
    let invalid = RwSignal::new(None::<NntpField>);
    let outcome = RwSignal::new(None::<Outcome>);
    let busy = RwSignal::new(false);

    let show = move |settings: Option<NntpServerSettings>| {
        fields.fill(NntpDraft::from_settings(settings.as_ref()));
        saved.set(Some(Ok(settings)));
    };
    let load = move || {
        leptos::task::spawn_local(async move {
            match get_json::<NntpServerSettings>(NNTP_PATH).await {
                Ok(settings) => show(Some(settings)),
                Err(ApiFailure::Status { status: 404, .. }) => show(None),
                Err(failure) => saved.set(Some(Err(LoadProblem::from_failure(&failure)))),
            }
        });
    };
    #[cfg(not(feature = "ssr"))]
    Effect::new(move |_| load());
    #[cfg(feature = "ssr")]
    let _ = load;

    let fail = move |title: &str, detail: String| {
        outcome.set(Some(Outcome {
            ok: false,
            title: title.into(),
            detail,
        }));
    };
    // The request body, or `None` after pointing at the field to fix.
    let update = move || match fields.read().to_update() {
        Ok(update) => {
            invalid.set(None);
            Some(update)
        }
        Err(e) => {
            invalid.set(Some(e.field));
            fail("Check the form", e.message.into());
            None
        }
    };

    let on_save = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let Some(body) = update() else { return };
        busy.set(true);
        outcome.set(None);
        leptos::task::spawn_local(async move {
            match send_json::<_, NntpServerSettings>(Method::Put, NNTP_PATH, Some(&body)).await {
                Ok(settings) => {
                    show(Some(settings));
                    outcome.set(Some(Outcome {
                        ok: true,
                        title: "Saved".into(),
                        detail: "New downloads use these settings. Downloads already running finish on the old ones.".into(),
                    }));
                }
                Err(failure) => fail("Not saved", action_error(&failure, NNTP_SHOWN_CODES)),
            }
            busy.set(false);
        });
    };
    let on_test = move |_| {
        let Some(body) = update() else { return };
        busy.set(true);
        outcome.set(Some(Outcome {
            ok: true,
            title: "Testing…".into(),
            detail: "Connecting and signing in. Nothing is saved.".into(),
        }));
        leptos::task::spawn_local(async move {
            match send_json::<_, NntpTestResult>(Method::Post, NNTP_TEST_PATH, Some(&body)).await {
                Ok(result) => {
                    let (ok, title, detail) = test_result(&result);
                    outcome.set(Some(Outcome {
                        ok,
                        title: format!("Test: {title}"),
                        detail,
                    }));
                }
                Err(failure) => fail("Test didn't run", action_error(&failure, NNTP_SHOWN_CODES)),
            }
            busy.set(false);
        });
    };
    let on_remove = Callback::new(move |()| {
        busy.set(true);
        outcome.set(None);
        leptos::task::spawn_local(async move {
            match send_empty::<()>(Method::Delete, NNTP_PATH, None).await {
                Ok(()) => {
                    show(None);
                    outcome.set(Some(Outcome {
                        ok: true,
                        title: "Removed".into(),
                        detail: "The Usenet server and its saved password are gone. New downloads can't start until a server is set up.".into(),
                    }));
                }
                Err(failure) => fail("Not removed", action_error(&failure, NNTP_SHOWN_CODES)),
            }
            busy.set(false);
        });
    });

    let settings =
        move || saved.with(|s| s.as_ref().and_then(|s| s.as_ref().ok().cloned().flatten()));
    let has_password = move || settings().is_some_and(|s| s.has_password);
    let bad = move |field: NntpField| move || (invalid.get() == Some(field)).then_some("true");

    view! {
        <NntpFrame>
            {move || match saved.with(|s| s.as_ref().map(|s| s.as_ref().map(|_| ()).map_err(|p| *p))) {
                None => view! {
                    <p class="staff-status" role="status">"Reading the Usenet settings…"</p>
                }
                .into_any(),
                Some(Err(problem)) => view! {
                    <SectionProblem problem what="the Usenet settings" />
                }
                .into_any(),
                Some(Ok(())) => view! {
                    <p class="staff-summary">
                        <span
                            class="health-chip"
                            class:health-ok=move || settings().is_some_and(|s| s.enabled)
                            class:health-unknown=move || !settings().is_some_and(|s| s.enabled)
                        >
                            {move || match settings() {
                                Some(s) if s.enabled => "Set up",
                                Some(_) => "Switched off",
                                None => "Not set up",
                            }}
                        </span>
                        {move || nntp_summary(settings().as_ref())}
                    </p>
                }
                .into_any(),
            }}
            <Show when=move || saved.with(|s| matches!(s, Some(Ok(_))))>
                <form class="staff-form nntp-form" on:submit=on_save aria-labelledby="nntp-form-title">
                    <h3 id="nntp-form-title" class="visually-hidden">"Usenet server settings"</h3>
                    <label class="staff-choice staff-field-wide" for="nntp-enabled">
                        <input
                            id="nntp-enabled"
                            type="checkbox"
                            prop:checked=move || fields.enabled.get()
                            on:change=move |ev| fields.enabled.set(event_target_checked(&ev))
                        />
                        "Use this server for downloads"
                    </label>
                    <div class="staff-field staff-field-wide">
                        <label for="nntp-host">"Server host"</label>
                        <input
                            id="nntp-host"
                            type="text"
                            required
                            autocomplete="off"
                            spellcheck="false"
                            maxlength="253"
                            placeholder="news.example.com"
                            aria-describedby="nntp-outcome"
                            aria-invalid=bad(NntpField::Host)
                            bind:value=fields.host
                        />
                    </div>
                    <div class="staff-field">
                        <label for="nntp-port">"Port"</label>
                        <input
                            id="nntp-port"
                            type="number"
                            inputmode="numeric"
                            min="1"
                            max="65535"
                            aria-describedby="nntp-port-hint"
                            aria-invalid=bad(NntpField::Port)
                            bind:value=fields.port
                        />
                        <p id="nntp-port-hint" class="staff-hint">"A TLS port, usually 563."</p>
                    </div>
                    <div class="staff-field">
                        <label for="nntp-username">"Username"</label>
                        <input
                            id="nntp-username"
                            type="text"
                            autocomplete="off"
                            spellcheck="false"
                            aria-invalid=bad(NntpField::Username)
                            bind:value=fields.username
                        />
                    </div>
                    <div class="staff-field">
                        <label for="nntp-password">"Password"</label>
                        // Write-only: never filled from the server.
                        <input
                            id="nntp-password"
                            type="password"
                            autocomplete="new-password"
                            aria-describedby="nntp-password-hint"
                            aria-invalid=bad(NntpField::Password)
                            bind:value=fields.password
                        />
                        <p id="nntp-password-hint" class="staff-hint">
                            {move || password_status(settings().as_ref())}
                        </p>
                    </div>
                    <Show when=has_password>
                        <label class="staff-choice staff-field-wide" for="nntp-clear-password">
                            <input
                                id="nntp-clear-password"
                                type="checkbox"
                                prop:checked=move || fields.clear_password.get()
                                on:change=move |ev| fields.clear_password.set(event_target_checked(&ev))
                            />
                            "Forget the saved password (and connect without signing in)"
                        </label>
                    </Show>
                    <fieldset class="staff-fieldset staff-field-wide">
                        <legend>"Connection limits"</legend>
                        <div class="staff-fieldset-grid">
                        <div class="staff-field">
                            <label for="nntp-connections">"Connections"</label>
                            <input
                                id="nntp-connections"
                                type="number"
                                inputmode="numeric"
                                min="1"
                                max="100"
                                aria-invalid=bad(NntpField::Connections)
                                bind:value=fields.max_connections
                            />
                        </div>
                        <div class="staff-field">
                            <label for="nntp-connect-timeout">"Connect timeout (s)"</label>
                            <input
                                id="nntp-connect-timeout"
                                type="number"
                                inputmode="numeric"
                                min="1"
                                max="300"
                                aria-invalid=bad(NntpField::ConnectTimeout)
                                bind:value=fields.connect_timeout_secs
                            />
                        </div>
                        <div class="staff-field">
                            <label for="nntp-io-timeout">"Read timeout (s)"</label>
                            <input
                                id="nntp-io-timeout"
                                type="number"
                                inputmode="numeric"
                                min="1"
                                max="300"
                                aria-invalid=bad(NntpField::IoTimeout)
                                bind:value=fields.io_timeout_secs
                            />
                        </div>
                        <div class="staff-field">
                            <label for="nntp-retries">"Retries"</label>
                            <input
                                id="nntp-retries"
                                type="number"
                                inputmode="numeric"
                                min="0"
                                max="10"
                                aria-invalid=bad(NntpField::Retries)
                                bind:value=fields.max_retries
                            />
                        </div>
                        </div>
                    </fieldset>
                    <div id="nntp-outcome" class="staff-outcome staff-field-wide" role="status" aria-live="polite">
                        {move || outcome.get().map(|o| view! {
                            <p class="staff-outcome-line" class:staff-outcome-bad=!o.ok>
                                {if o.ok {
                                    view! { <Icon name=IconName::CheckCircle /> }.into_any()
                                } else {
                                    view! { <Icon name=IconName::Error /> }.into_any()
                                }}
                                <strong>{o.title}</strong>
                                " "
                                {o.detail}
                            </p>
                        })}
                    </div>
                    <div class="staff-actions staff-field-wide">
                        <button type="submit" class="button primary" disabled=move || busy.get()>
                            <Icon name=IconName::CloudDone />
                            "Save settings"
                        </button>
                        <button type="button" class="button" disabled=move || busy.get() on:click=on_test>
                            <Icon name=IconName::Bolt />
                            "Test connection"
                        </button>
                        <Show when=move || settings().is_some()>
                            <Confirm
                                id="remove-nntp"
                                trigger="Remove server…"
                                trigger_label="Remove the Usenet server"
                                question="Remove the Usenet server and its saved password? New downloads can't start until you set one up again."
                                confirm="Yes, remove server"
                                keep="Keep it"
                                on_confirm=on_remove
                                busy=Signal::derive(move || busy.get())
                            />
                        </Show>
                    </div>
                </form>
            </Show>
        </NntpFrame>
    }
    .into_any()
}

/// Staff controls from the design that have no settings in Reelarr yet.
#[component]
fn NotConnected() -> impl IntoView {
    let parts = [
        (
            IconName::HardDrive,
            "Storage usage",
            "Reelarr doesn't report how full its folders are yet.",
        ),
        (
            IconName::Movie,
            "Movie metadata",
            "There's no setting for a metadata service key yet, so titles and artwork can't be looked up.",
        ),
        (
            IconName::Search,
            "Indexers",
            "Indexers can't be added or edited from here yet.",
        ),
        (
            IconName::Person,
            "Staff accounts",
            "Adding staff or changing passwords isn't available here yet.",
        ),
        (
            IconName::Tune,
            "Quality & formats",
            "Choosing preferred quality and formats isn't available yet.",
        ),
    ];
    view! {
        <section class="staff-later" aria-labelledby="later-title">
            <h2 id="later-title" class="staff-panel-title">
                <Icon name=IconName::Fluorescent />
                "Back office // Not connected yet"
            </h2>
            <p class="staff-lede">
                "These parts of the console don't have settings in Reelarr yet, so there's nothing to change here."
            </p>
            <ul class="staff-later-grid">
                {parts
                    .into_iter()
                    .map(|(icon, title, text)| view! {
                        <li class="staff-later-card">
                            <p class="staff-later-title">
                                <Icon name=icon />
                                {title}
                            </p>
                            <p>{text}</p>
                            <p class="staff-sticker">"Not connected yet"</p>
                        </li>
                    })
                    .collect_view()}
            </ul>
        </section>
    }
}
