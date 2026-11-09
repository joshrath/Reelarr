//! The fixed top bar from the design: wordmark, sections, search shortcut,
//! CRT and theme switches, and the account menu.

use leptos::{ev, prelude::*};
use leptos_router::hooks::use_navigate;

use super::icon::{Icon, IconName};
use super::nav::MainNav;
use crate::theme::{PrefsSignal, Theme};

/// Where the search field and the Ctrl/⌘+K shortcut lead.
pub const SEARCH_PATH: &str = "/counter";

#[component]
pub fn Header() -> AnyView {
    let prefs = PrefsSignal::use_context();
    let drawer_open = RwSignal::new(false);
    let menu_open = RwSignal::new(false);

    // Ctrl/⌘+K opens search; Escape closes the menu and the drawer.
    let navigate = use_navigate();
    let handle = window_event_listener(ev::keydown, move |e| {
        if (e.ctrl_key() || e.meta_key()) && e.key().eq_ignore_ascii_case("k") {
            e.prevent_default();
            navigate(SEARCH_PATH, Default::default());
        } else if e.key() == "Escape" {
            menu_open.set(false);
            drawer_open.set(false);
        }
    });
    on_cleanup(move || handle.remove());

    view! {
        <header class="app-header">
            <div class="app-header-row">
                <button
                    type="button"
                    class="header-icon-button drawer-toggle"
                    aria-expanded=move || drawer_open.get().to_string()
                    aria-controls="nav-drawer"
                    on:click=move |_| drawer_open.update(|o| *o = !*o)
                >
                    <Icon name=IconName::Menu label="Menu" />
                </button>
                <a class="brand" href="/">"Reelarr"</a>
                <MainNav class="main-nav-bar" />
                <a class="header-search" href=SEARCH_PATH>
                    <Icon name=IconName::Search />
                    <span class="header-search-text">"_Search archive…"</span>
                    <kbd aria-label="Shortcut: Control or Command K">"⌘K"</kbd>
                </a>
                <div class="header-tools">
                    <button
                        type="button"
                        class="chip-button crt-toggle"
                        aria-pressed=move || prefs.0.get().crt.to_string()
                        on:click=move |_| prefs.toggle_crt()
                    >
                        <Icon name=IconName::Tv />
                        <span>"CRT: " {move || if prefs.0.get().crt { "On" } else { "Off" }}</span>
                    </button>
                    <ThemeSwitch />
                    <AccountMenu open=menu_open />
                </div>
            </div>
            <div id="nav-drawer" class="nav-drawer" hidden=move || !drawer_open.get()>
                <MainNav class="main-nav-drawer" on_navigate=move |()| drawer_open.set(false) />
            </div>
        </header>
    }
    .into_any()
}

/// Two-way '80s/'70s switch shown in the bar on wider screens.
#[component]
fn ThemeSwitch() -> AnyView {
    let prefs = PrefsSignal::use_context();
    let button = move |theme: Theme, icon: IconName, label: &'static str| {
        view! {
            <button
                type="button"
                aria-pressed=move || (prefs.0.get().theme == theme).to_string()
                on:click=move |_| prefs.set_theme(theme)
            >
                <Icon name=icon />
                <span>{label}</span>
            </button>
        }
    };
    view! {
        <div class="theme-switch" role="group" aria-label="Theme">
            {button(Theme::Vhs80s, IconName::Bolt, "'80s VHS")}
            {button(Theme::Wood70s, IconName::Nature, "'70s Wood")}
        </div>
    }
    .into_any()
}

/// Account button and menu. Shows the signed-in user once auth is wired up.
#[component]
fn AccountMenu(open: RwSignal<bool>) -> AnyView {
    let prefs = PrefsSignal::use_context();
    let option = move |theme: Theme, icon: IconName, label: &'static str| {
        view! {
            <button
                type="button"
                class="menu-item"
                aria-pressed=move || (prefs.0.get().theme == theme).to_string()
                on:click=move |_| {
                    prefs.set_theme(theme);
                    open.set(false);
                }
            >
                <span class="menu-item-label">
                    <Icon name=icon />
                    {label}
                </span>
                <Show when=move || prefs.0.get().theme == theme>
                    <Icon name=IconName::Check />
                </Show>
            </button>
        }
    };
    view! {
        <div class="account">
            <button
                type="button"
                class="account-button"
                aria-expanded=move || open.get().to_string()
                aria-controls="account-menu"
                on:click=move |_| open.update(|o| *o = !*o)
            >
                <span class="avatar">
                    <Icon name=IconName::Person />
                </span>
                <span class="visually-hidden">"Account and display settings"</span>
                <Icon name=IconName::ExpandMore />
            </button>
            <div id="account-menu" class="account-menu" hidden=move || !open.get()>
                <p class="menu-heading">"Theme preferences"</p>
                {option(Theme::Vhs80s, IconName::Fluorescent, "'80s VHS Neon")}
                {option(Theme::Wood70s, IconName::Radio, "'70s Wood Paneling")}
                <button
                    type="button"
                    class="menu-item"
                    aria-pressed=move || prefs.0.get().crt.to_string()
                    on:click=move |_| prefs.toggle_crt()
                >
                    <span class="menu-item-label">
                        <Icon name=IconName::Tv />
                        "CRT scanlines"
                    </span>
                    <Show when=move || prefs.0.get().crt>
                        <Icon name=IconName::Check />
                    </Show>
                </button>
                <hr />
                <a class="menu-item" href="/employees-only" on:click=move |_| open.set(false)>
                    <span class="menu-item-label">
                        <Icon name=IconName::AdminPanelSettings />
                        "System console"
                    </span>
                </a>
            </div>
        </div>
    }
    .into_any()
}
