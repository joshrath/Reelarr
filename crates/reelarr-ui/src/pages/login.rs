//! Sign-in page.
//!
//! UI only for now: authentication lands with issue #7, which will provide
//! `POST /api/v1/auth/login`. Until then submitting shows a clear notice
//! instead of sending credentials anywhere.

use leptos::prelude::*;
use leptos_meta::Title;

/// Message shown while sign-in is not wired to the server yet.
pub const AUTH_PENDING_MESSAGE: &str =
    "Sign-in isn't available yet. Account setup and sessions are coming soon.";

#[component]
pub fn LoginPage() -> impl IntoView {
    let username = RwSignal::new(String::new());
    let password = RwSignal::new(String::new());
    let error = RwSignal::new(None::<&'static str>);

    let on_submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        error.set(Some(
            validate(&username.get(), &password.get()).unwrap_or(AUTH_PENDING_MESSAGE),
        ));
    };

    view! {
        <Title text="Sign in" />
        <main id="main-content" class="auth-main">
            <form class="auth-card" on:submit=on_submit novalidate=true aria-describedby="login-error">
                <h1>"Sign in to Reelarr"</h1>
                <div class="field">
                    <label for="username">"Username"</label>
                    <input
                        id="username"
                        name="username"
                        type="text"
                        autocomplete="username"
                        required=true
                        bind:value=username
                    />
                </div>
                <div class="field">
                    <label for="password">"Password"</label>
                    <input
                        id="password"
                        name="password"
                        type="password"
                        autocomplete="current-password"
                        required=true
                        bind:value=password
                    />
                </div>
                <p id="login-error" class="form-error" role="alert" aria-live="assertive">
                    {move || error.get()}
                </p>
                <button type="submit" class="button primary">"Sign in"</button>
            </form>
        </main>
    }
}

/// Client-side checks before anything is sent. Returns an error message.
pub fn validate(username: &str, password: &str) -> Option<&'static str> {
    match (username.trim().is_empty(), password.is_empty()) {
        (true, true) => Some("Enter your username and password."),
        (true, false) => Some("Enter your username."),
        (false, true) => Some("Enter your password."),
        (false, false) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_requires_both_fields() {
        assert!(validate("", "").is_some());
        assert_eq!(validate(" ", "x"), Some("Enter your username."));
        assert_eq!(validate("admin", ""), Some("Enter your password."));
        assert_eq!(validate("admin", "secret"), None);
    }
}
