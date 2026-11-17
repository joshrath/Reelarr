//! An inline confirm step for destructive actions: the first button opens
//! a question with "do it" and "keep it" buttons, so one stray click or
//! key press never removes anything.

use leptos::html;
use leptos::prelude::*;

/// A button that asks before running `on_confirm`.
///
/// Opening the question moves focus to the safe choice; Escape or the
/// keep button closes it and returns focus to the trigger. When the
/// confirmed action removes this control (the row it acted on is gone),
/// focus moves to the `focus_after` element instead of being lost.
#[component]
pub fn Confirm(
    /// Unique id prefix for the question, e.g. `remove-root-3`.
    #[prop(into)]
    id: String,
    /// Visible trigger text, e.g. "Remove…".
    trigger: &'static str,
    /// Accessible name for the trigger, naming what it acts on.
    #[prop(into)]
    trigger_label: String,
    /// The question, saying exactly what will happen.
    #[prop(into)]
    question: Signal<String>,
    /// Text on the button that goes ahead.
    confirm: &'static str,
    /// Text on the button that backs out.
    keep: &'static str,
    #[prop(into)] on_confirm: Callback<()>,
    /// True while the action runs; the buttons are disabled meanwhile.
    #[prop(into)]
    busy: Signal<bool>,
    /// Id of the section heading to focus if a confirmed action removes
    /// this control, since the trigger is gone then. Give the heading
    /// `tabindex="-1"`.
    #[prop(optional, into)]
    focus_after: Option<String>,
) -> impl IntoView {
    let open = RwSignal::new(false);
    let trigger_ref = NodeRef::<html::Button>::new();
    let keep_ref = NodeRef::<html::Button>::new();
    let question_id = format!("{id}-question");
    let confirmed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // When this control goes away after "do it", focus would fall back to
    // the page body. Browser only.
    #[cfg(not(feature = "ssr"))]
    {
        let confirmed = confirmed.clone();
        on_cleanup(move || {
            if let Some(heading) = focus_after
                && confirmed.load(std::sync::atomic::Ordering::Relaxed)
            {
                request_animation_frame(move || focus_if_lost(&heading));
            }
        });
    }
    #[cfg(feature = "ssr")]
    let _ = focus_after;
    let mark_confirmed =
        Callback::new(move |()| confirmed.store(true, std::sync::atomic::Ordering::Relaxed));

    // Move focus with the step. Browser only.
    #[cfg(not(feature = "ssr"))]
    Effect::new(move |was_open: Option<bool>| {
        let now = open.get();
        if was_open.is_some_and(|was| was != now) {
            request_animation_frame(move || {
                let target = if now {
                    keep_ref.get_untracked().map(|b| b.focus())
                } else {
                    trigger_ref.get_untracked().map(|b| b.focus())
                };
                let _ = target;
            });
        }
        now
    });

    view! {
        <span class="confirm">
            <Show
                when=move || open.get()
                fallback=move || {
                    view! {
                        <button
                            type="button"
                            class="button button-danger"
                            node_ref=trigger_ref
                            aria-label=trigger_label.clone()
                            disabled=move || busy.get()
                            on:click=move |_| open.set(true)
                        >
                            {trigger}
                        </button>
                    }
                }
            >
                <span
                    class="confirm-step"
                    role="group"
                    aria-labelledby=question_id.clone()
                    on:keydown=move |ev| {
                        if ev.key() == "Escape" {
                            open.set(false);
                        }
                    }
                >
                    <span class="confirm-question" id=question_id.clone()>
                        {move || question.get()}
                    </span>
                    <span class="confirm-buttons">
                        <button
                            type="button"
                            class="button button-danger-solid"
                            disabled=move || busy.get()
                            on:click=move |_| {
                                mark_confirmed.run(());
                                open.set(false);
                                on_confirm.run(());
                            }
                        >
                            {confirm}
                        </button>
                        <button
                            type="button"
                            class="button"
                            node_ref=keep_ref
                            on:click=move |_| open.set(false)
                        >
                            {keep}
                        </button>
                    </span>
                </span>
            </Show>
        </span>
    }
}

/// Focuses the element with `id` if nothing has focus (it fell back to
/// the body when the focused element was removed). Does nothing when the
/// element is gone too, e.g. after leaving the page.
#[cfg(not(feature = "ssr"))]
fn focus_if_lost(id: &str) {
    use web_sys::wasm_bindgen::JsCast;
    let Some(document) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let lost = document
        .active_element()
        .is_none_or(|el| el.tag_name().eq_ignore_ascii_case("body"));
    if lost
        && let Some(target) = document
            .get_element_by_id(id)
            .and_then(|el| el.dyn_into::<web_sys::HtmlElement>().ok())
    {
        let _ = target.focus();
    }
}
