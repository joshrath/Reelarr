//! Shared plumbing for the staff areas (The Back Room and Employees Only):
//! the sign-in gate, the words for a failed change, and number formatting.
//! Plain data and copy, so it can be tested without a browser.

use reelarr_types::SessionInfo;

use crate::api::{ApiFailure, SendFailure};
use crate::library::utc_date;
use crate::playback::{Notice, notice_for_failure};

/// Signed-in check: `GET /api/v1/auth/session`.
pub const SESSION_PATH: &str = "/api/v1/auth/session";

/// Whether the staff door is open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// Waiting for the session check.
    Checking,
    /// Signed in, as this user name.
    Open(String),
    /// Not signed in, or the session has ended.
    SignIn,
    /// The session couldn't be checked.
    TryAgain,
}

pub fn gate(result: Result<SessionInfo, ApiFailure>) -> Gate {
    match result {
        Ok(SessionInfo {
            user: Some(user), ..
        }) => Gate::Open(user.username),
        Ok(SessionInfo { user: None, .. }) => Gate::SignIn,
        Err(failure) => match notice_for_failure(&failure) {
            Notice::SignIn => Gate::SignIn,
            _ => Gate::TryAgain,
        },
    }
}

/// What a section shows when its list couldn't be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadProblem {
    /// 401: the session ended.
    SignIn,
    /// 503: that part of Reelarr is offline.
    Offline,
    /// 500, anything else, or no answer.
    TryAgain,
}

impl LoadProblem {
    pub fn from_failure(failure: &ApiFailure) -> Self {
        match notice_for_failure(failure) {
            Notice::SignIn => Self::SignIn,
            Notice::Offline => Self::Offline,
            _ => Self::TryAgain,
        }
    }

    /// A sentence about `what` (e.g. "the job list").
    pub fn message(self, what: &str) -> String {
        match self {
            Self::SignIn => SESSION_ENDED_MESSAGE.into(),
            Self::Offline => format!("Reelarr can't reach {what} right now. It's offline."),
            Self::TryAgain => {
                format!("Something went wrong reading {what}. Try again in a moment.")
            }
        }
    }
}

pub const SESSION_ENDED_MESSAGE: &str = "Your session has ended. Sign in again to continue.";
pub const CSRF_MESSAGE: &str =
    "Reelarr couldn't confirm that request came from this page. Reload the page and try again.";
pub const OFFLINE_ACTION_MESSAGE: &str =
    "That part of Reelarr is offline right now. Try again once it's back.";
pub const TRY_AGAIN_ACTION_MESSAGE: &str = "Something went wrong. Try again in a moment.";

/// Words for a failed change. `shown` lists the error codes whose server
/// message is fit to show as-is (validation and conflicts); everything
/// else, including any 500, gets a plain message without server details.
pub fn action_error(failure: &SendFailure, shown: &[&str]) -> String {
    match (failure.code(), failure.status()) {
        (Some("unauthorized"), _) | (None, Some(401)) => SESSION_ENDED_MESSAGE.into(),
        (Some("csrf_rejected"), _) | (None, Some(403)) => CSRF_MESSAGE.into(),
        (Some("unavailable"), _) | (None, Some(503)) => OFFLINE_ACTION_MESSAGE.into(),
        (Some(code), Some(status)) if status < 500 && shown.contains(&code) => failure
            .message
            .as_deref()
            .map(sentence)
            .unwrap_or_else(|| TRY_AGAIN_ACTION_MESSAGE.into()),
        _ => TRY_AGAIN_ACTION_MESSAGE.into(),
    }
}

/// Whether a failed change means the visitor has to sign in again.
pub fn needs_sign_in(failure: &SendFailure) -> bool {
    matches!(
        (failure.code(), failure.status()),
        (Some("unauthorized"), _) | (None, Some(401))
    )
}

/// A message as a sentence: capitalized, ending in a full stop.
pub fn sentence(message: &str) -> String {
    let message = message.trim();
    let mut chars = message.chars();
    let mut out = match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect::<String>(),
        None => return String::new(),
    };
    if !out.ends_with(['.', '?', '!']) {
        out.push('.');
    }
    out
}

/// A byte count in decimal units: `512 B`, `1.4 MB`, `3.0 GB`.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1000.0;
    let mut unit = 0;
    while value >= 999.95 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// A transfer rate, or `None` when nothing is moving.
pub fn format_rate(bytes_per_second: u64) -> Option<String> {
    (bytes_per_second > 0).then(|| format!("{}/s", format_bytes(bytes_per_second)))
}

/// Time left: `45s`, `12m 05s`, `2h 03m`.
pub fn format_eta(seconds: u64) -> String {
    let (h, m, s) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

/// A Unix time as `YYYY-MM-DD HH:MM UTC`.
pub fn utc_datetime(unix_seconds: i64) -> String {
    let secs = unix_seconds.rem_euclid(86_400);
    format!(
        "{} {:02}:{:02} UTC",
        utc_date(unix_seconds),
        secs / 3600,
        secs / 60 % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use reelarr_types::UserInfo;

    fn status(status: u16, code: &str, message: &str) -> SendFailure {
        SendFailure {
            failure: ApiFailure::Status {
                status,
                code: Some(code.into()),
            },
            message: Some(message.into()),
        }
    }

    #[test]
    fn gate_reads_the_session() {
        let user = UserInfo {
            id: "1".into(),
            username: "clerk".into(),
        };
        assert_eq!(
            gate(Ok(SessionInfo {
                user: Some(user),
                csrf_token: "t".into()
            })),
            Gate::Open("clerk".into())
        );
        assert_eq!(
            gate(Ok(SessionInfo {
                user: None,
                csrf_token: "t".into()
            })),
            Gate::SignIn
        );
        assert_eq!(
            gate(Err(ApiFailure::Status {
                status: 401,
                code: None
            })),
            Gate::SignIn
        );
        assert_eq!(gate(Err(ApiFailure::Network)), Gate::TryAgain);
        assert_eq!(
            gate(Err(ApiFailure::Status {
                status: 503,
                code: Some("unavailable".into())
            })),
            Gate::TryAgain
        );
    }

    #[test]
    fn load_problems_map_statuses() {
        let failure = |status, code: &str| ApiFailure::Status {
            status,
            code: Some(code.into()),
        };
        assert_eq!(
            LoadProblem::from_failure(&failure(401, "unauthorized")),
            LoadProblem::SignIn
        );
        assert_eq!(
            LoadProblem::from_failure(&failure(503, "unavailable")),
            LoadProblem::Offline
        );
        assert_eq!(
            LoadProblem::from_failure(&failure(500, "internal")),
            LoadProblem::TryAgain
        );
        assert_eq!(
            LoadProblem::from_failure(&ApiFailure::Network),
            LoadProblem::TryAgain
        );
        assert!(
            LoadProblem::Offline
                .message("the job list")
                .contains("offline")
        );
        // A 500's server text is never repeated.
        assert!(!LoadProblem::TryAgain.message("x").contains("database"));
    }

    #[test]
    fn action_errors_only_repeat_allowed_codes() {
        let shown = ["invalid_path"];
        assert_eq!(
            action_error(
                &status(400, "invalid_path", "library folder must be absolute"),
                &shown
            ),
            "Library folder must be absolute."
        );
        // Not on the list: generic.
        assert_eq!(
            action_error(&status(400, "invalid_body", "expected JSON"), &shown),
            TRY_AGAIN_ACTION_MESSAGE
        );
        // A 5xx never shows server text, even for a listed code.
        assert_eq!(
            action_error(&status(500, "invalid_path", "db exploded"), &shown),
            TRY_AGAIN_ACTION_MESSAGE
        );
        assert_eq!(
            action_error(&status(401, "unauthorized", "x"), &shown),
            SESSION_ENDED_MESSAGE
        );
        assert_eq!(
            action_error(&status(403, "csrf_rejected", "x"), &shown),
            CSRF_MESSAGE
        );
        assert_eq!(
            action_error(&status(503, "unavailable", "x"), &shown),
            OFFLINE_ACTION_MESSAGE
        );
        assert_eq!(
            action_error(&SendFailure::network(), &shown),
            TRY_AGAIN_ACTION_MESSAGE
        );
        assert!(needs_sign_in(&status(401, "unauthorized", "x")));
        assert!(!needs_sign_in(&status(403, "csrf_rejected", "x")));
    }

    #[test]
    fn sentences_are_capitalized_and_stopped() {
        assert_eq!(sentence("port must be 1–65535"), "Port must be 1–65535.");
        assert_eq!(sentence("Already done."), "Already done.");
        assert_eq!(sentence("  "), "");
    }

    #[test]
    fn sizes_rates_and_times() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1_000), "1.0 KB");
        assert_eq!(format_bytes(48_200_000), "48.2 MB");
        assert_eq!(format_bytes(999_960_000), "1.0 GB");
        assert_eq!(format_bytes(4_000_000_000_000), "4.0 TB");
        assert_eq!(format_rate(0), None);
        assert_eq!(format_rate(31_400_000).as_deref(), Some("31.4 MB/s"));
        assert_eq!(format_eta(45), "45s");
        assert_eq!(format_eta(725), "12m 05s");
        assert_eq!(format_eta(7_380), "2h 03m");
        assert_eq!(utc_datetime(0), "1970-01-01 00:00 UTC");
        assert_eq!(utc_datetime(1_700_000_000), "2023-11-14 22:13 UTC");
    }
}
