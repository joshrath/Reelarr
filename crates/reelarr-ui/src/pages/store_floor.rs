//! The Store Floor: the library home, with the shelf and the library
//! folders behind it.
//!
//! The API can't list the library's movies yet, so the shelf says what the
//! server knows instead: which library folders exist, whether they can be
//! read and how many files are on record. It never shows sample titles.

use leptos::prelude::*;
use leptos_meta::Title;
use reelarr_types::LibraryRootInfo;

use crate::api::get_json;
use crate::components::clamshell::EmptySlot;
use crate::components::icon::{Icon, IconName};
use crate::library::{
    FloorState, Folder, FolderHealth, ROOTS_PATH, SETTINGS_PATH, ShelfSummary, floor_state,
    shelf_summary,
};
use crate::playback::{Notice, SIGN_IN_PATH};

/// Empty spots drawn on the shelf board.
const EMPTY_SLOTS: usize = 8;

#[component]
pub fn StoreFloorPage() -> impl IntoView {
    // Fetched in the browser only, with the session cookie; the server
    // renders the shelf while it's being checked.
    let roots = LocalResource::new(|| get_json::<Vec<LibraryRootInfo>>(ROOTS_PATH));
    let retry = Callback::new(move |()| roots.refetch());

    view! {
        <Title text="The Store Floor" />
        <section class="page store-floor" aria-labelledby="page-title">
            <header class="floor-marquee">
                <p class="floor-marquee-label label">
                    <Icon name=IconName::Storefront />
                    "Main floor • Eye level"
                </p>
                <h1 id="page-title">"The Store Floor"</h1>
            </header>
            <Suspense fallback=|| view! { <Floor state=FloorState::Loading /> }>
                {move || Suspend::new(async move {
                    let state = floor_state(roots.await);
                    view! { <Floor state on_retry=retry /> }
                })}
            </Suspense>
        </section>
    }
}

/// The shelf and the library folders for one state.
#[component]
fn Floor(state: FloorState, #[prop(optional, into)] on_retry: Option<Callback<()>>) -> AnyView {
    match state {
        FloorState::Loading => view! {
            <Shelf>
                <p class="shelf-status" role="status">"Checking the shelves…"</p>
            </Shelf>
        }
        .into_any(),
        FloorState::Notice(notice) => view! {
            <Shelf>
                <FloorNotice notice on_retry />
            </Shelf>
        }
        .into_any(),
        FloorState::Floor(folders) => {
            let summary = shelf_summary(&folders);
            view! {
                <Shelf>
                    <ShelfMessage summary />
                </Shelf>
                <Folders folders />
            }
            .into_any()
        }
    }
}

/// The shelf board with its empty slots, and a message over it.
#[component]
fn Shelf(children: Children) -> impl IntoView {
    view! {
        <section class="shelf" aria-labelledby="shelf-title">
            <h2 id="shelf-title" class="shelf-title">
                <span class="aisle-tag">"Aisle 1"</span>
                "New releases & the back catalog"
            </h2>
            <div class="shelf-board">
                <div class="shelf-slots">
                    {(0..EMPTY_SLOTS).map(|_| view! { <EmptySlot /> }).collect_view()}
                </div>
                <div class="shelf-message">{children()}</div>
            </div>
        </section>
    }
}

#[component]
fn ShelfMessage(summary: ShelfSummary) -> impl IntoView {
    let alert = matches!(summary, ShelfSummary::AllUnreachable { .. });
    let action = matches!(summary, ShelfSummary::NoFolders).then(|| {
        view! {
            <p class="shelf-actions">
                <a class="button primary" href=SETTINGS_PATH>
                    "Go to Employees Only"
                </a>
            </p>
        }
    });
    view! {
        <div class="shelf-card" class:shelf-card-alert=alert role=alert.then_some("alert")>
            <p class="shelf-card-title">{summary.title()}</p>
            <p>{summary.message()}</p>
            {action}
        </div>
    }
}

#[component]
fn FloorNotice(notice: Notice, on_retry: Option<Callback<()>>) -> AnyView {
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
    let message = match notice {
        Notice::SignIn => "Sign in to see what's on the shelves.".to_string(),
        Notice::TryAgain => {
            "Something went wrong checking the shelves. Try again in a moment.".to_string()
        }
        ref other => other.message(),
    };
    let title = match notice {
        Notice::SignIn => "Sign in to browse",
        Notice::TryAgain => "Couldn't check the shelves",
        ref other => other.title(),
    };
    view! {
        <div class="shelf-card shelf-card-alert" role="alert">
            <p class="shelf-card-title">{title}</p>
            <p>{message}</p>
            {action.map(|action| view! { <p class="shelf-actions">{action}</p> })}
        </div>
    }
    .into_any()
}

/// The library folders behind the shelf, with their health.
#[component]
fn Folders(folders: Vec<Folder>) -> impl IntoView {
    (!folders.is_empty()).then(|| {
        view! {
            <section class="folders" aria-labelledby="folders-title">
                <h2 id="folders-title" class="shelf-title">
                    <span class="aisle-tag aisle-tag-alt">"Stockroom"</span>
                    "Library folders"
                </h2>
                <ul class="folder-list">
                    {folders.into_iter().map(|folder| view! { <FolderRow folder /> }).collect_view()}
                </ul>
            </section>
        }
    })
}

#[component]
fn FolderRow(folder: Folder) -> impl IntoView {
    let (icon, tone) = match folder.health {
        FolderHealth::Online => (IconName::CheckCircle, "ok"),
        FolderHealth::Unchecked => (IconName::Info, "unknown"),
        FolderHealth::Offline { .. } | FolderHealth::PermissionDenied { .. } => {
            (IconName::Error, "bad")
        }
    };
    let problem = folder.health.problem();
    let has_problem = problem.is_some();
    let count = folder.count_label();
    view! {
        <li class="folder" class:folder-problem=has_problem>
            <span class="folder-icon">
                <Icon name=IconName::HardDrive />
            </span>
            <span class="folder-main">
                <span class="folder-path">{folder.path}</span>
                <span class="folder-count">{count}</span>
                {problem.map(|p| view! { <span class="folder-note">{p}</span> })}
            </span>
            <span class=format!("health-chip health-{tone}")>
                <Icon name=icon />
                {folder.health.label()}
            </span>
        </li>
    }
}
