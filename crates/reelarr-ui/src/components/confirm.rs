//! An inline confirm step for destructive actions: the first button opens
//! a question with "do it" and "keep it" buttons, so one stray click or
//! key press never removes anything.

use leptos::html;
use leptos::prelude::*;

/// A button that asks before running `on_confirm`.
///
/// Opening the question moves focus to the safe choice; Escape or the
/// keep button closes it and returns focus to the trigger.
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
    question: String,
    /// Text on the button that goes ahead.
    confirm: &'static str,
    /// Text on the button that backs out.
    keep: &'static str,
    #[prop(into)] on_confirm: Callback<()>,
    /// True while the action runs; the buttons are disabled meanwhile.
    #[prop(into)]
    busy: Signal<bool>,
) -> impl IntoView {
    let open = RwSignal::new(false);
    let trigger_ref = NodeRef::<html::Button>::new();
    let keep_ref = NodeRef::<html::Button>::new();
    let question_id = format!("{id}-question");

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
                        {question.clone()}
                    </span>
                    <span class="confirm-buttons">
                        <button
                            type="button"
                            class="button button-danger-solid"
                            disabled=move || busy.get()
                            on:click=move |_| {
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
