//! What the Special Order Counter shows. Plain data and copy, so it can be
//! tested without a browser.
//!
//! The API has no title search yet: there is a `MovieSearchResult` type in
//! `reelarr-types`, but no route serves it, and nothing adds a movie to the
//! library. So the counter is the desk from the design with a working
//! search field (kept in the page's `?q=`), and it says plainly that
//! nothing was looked up. It never shows sample results.
//!
//! The one thing it does ask the server is whether you're signed in, from
//! `GET /api/v1/auth/session`.

use reelarr_types::SessionInfo;

use crate::api::ApiFailure;
use crate::playback::{Notice, notice_for_failure};

/// The counter page.
pub const COUNTER_PATH: &str = "/counter";

/// The session endpoint. Always answers 200; `user` is empty when you
/// aren't signed in.
pub const SESSION_PATH: &str = "/api/v1/auth/session";

/// Where order slips will be tracked.
pub const BACK_ROOM_PATH: &str = "/back-room";

/// Longest query kept, in characters.
pub const MAX_QUERY_CHARS: usize = 200;

/// Tidies a typed query: trims it, folds runs of whitespace into one space
/// and caps the length.
pub fn normalize_query(raw: &str) -> String {
    raw.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_QUERY_CHARS)
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// The counter's address for a query: `/counter`, or `/counter?q=…`.
pub fn counter_href(query: &str) -> String {
    let query = normalize_query(query);
    if query.is_empty() {
        COUNTER_PATH.to_string()
    } else {
        format!("{COUNTER_PATH}?q={}", encode_query_value(&query))
    }
}

/// Percent-encodes a query-string value. Unreserved characters stay as they
/// are and spaces become `+`.
fn encode_query_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Which shelves to ask about. Only movies are stocked in Phase 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    #[default]
    All,
    Movies,
}

impl Format {
    pub const ALL: [Format; 2] = [Format::All, Format::Movies];

    pub fn label(self) -> &'static str {
        match self {
            Format::All => "All formats",
            Format::Movies => "Movies (VHS)",
        }
    }
}

/// Whether the desk is open to you.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeskState {
    /// Waiting for the session answer.
    Checking,
    /// Signed in.
    Open,
    /// Not signed in, or the session has ended.
    SignIn,
    /// The session couldn't be checked.
    TryAgain,
}

pub fn desk_state(result: Result<SessionInfo, ApiFailure>) -> DeskState {
    match result {
        Ok(SessionInfo { user: Some(_), .. }) => DeskState::Open,
        Ok(SessionInfo { user: None, .. }) => DeskState::SignIn,
        Err(failure) => match notice_for_failure(&failure) {
            Notice::SignIn => DeskState::SignIn,
            _ => DeskState::TryAgain,
        },
    }
}

/// The message in the results area for a signed-in visitor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Results {
    /// Nothing typed yet.
    Idle,
    /// A query was entered, but there's no search to run it against.
    NotSearched(String),
}

pub const SEARCH_UNAVAILABLE_MESSAGE: &str = "Title search isn't hooked up yet. Once Reelarr can look movies up, matches from your shelves and the catalog will be listed here.";

impl Results {
    pub fn for_query(query: &str) -> Self {
        let query = normalize_query(query);
        if query.is_empty() {
            Results::Idle
        } else {
            Results::NotSearched(query)
        }
    }

    /// The status sticker over the result.
    pub fn badge(&self) -> &'static str {
        match self {
            Results::Idle => "Counter open",
            Results::NotSearched(_) => "Not looked up",
        }
    }

    pub fn title(&self) -> String {
        match self {
            Results::Idle => "Ask the clerk for a title".to_string(),
            Results::NotSearched(query) => format!("“{query}”"),
        }
    }

    pub fn message(&self) -> &'static str {
        match self {
            Results::Idle => SEARCH_UNAVAILABLE_MESSAGE,
            Results::NotSearched(_) => {
                "Title search isn't available yet, so Reelarr didn't look this up. There are no results to show."
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reelarr_types::UserInfo;

    fn session(user: Option<UserInfo>) -> SessionInfo {
        SessionInfo {
            user,
            csrf_token: "token".into(),
        }
    }

    #[test]
    fn queries_are_trimmed_and_folded() {
        assert_eq!(normalize_query("  blue \t  velvet\n"), "blue velvet");
        assert_eq!(normalize_query("   "), "");
    }

    #[test]
    fn long_queries_are_capped() {
        let long = "a".repeat(MAX_QUERY_CHARS + 50);
        assert_eq!(normalize_query(&long).chars().count(), MAX_QUERY_CHARS);
        // Capping never leaves a trailing space.
        let spaced = format!("{} b", "a".repeat(MAX_QUERY_CHARS - 1));
        assert_eq!(normalize_query(&spaced), "a".repeat(MAX_QUERY_CHARS - 1));
    }

    #[test]
    fn hrefs_encode_the_query() {
        assert_eq!(counter_href(""), "/counter");
        assert_eq!(counter_href("  "), "/counter");
        assert_eq!(
            counter_href("stop making sense"),
            "/counter?q=stop+making+sense"
        );
        assert_eq!(counter_href("a&b=c?#"), "/counter?q=a%26b%3Dc%3F%23");
        assert_eq!(counter_href("amélie"), "/counter?q=am%C3%A9lie");
        assert_eq!(counter_href("100%"), "/counter?q=100%25");
    }

    #[test]
    fn signed_in_opens_the_desk() {
        let user = UserInfo {
            id: "1".into(),
            username: "admin".into(),
        };
        assert_eq!(desk_state(Ok(session(Some(user)))), DeskState::Open);
    }

    #[test]
    fn anonymous_session_asks_to_sign_in() {
        assert_eq!(desk_state(Ok(session(None))), DeskState::SignIn);
        assert_eq!(
            desk_state(Err(ApiFailure::Status {
                status: 401,
                code: Some("unauthorized".into())
            })),
            DeskState::SignIn
        );
    }

    #[test]
    fn other_failures_offer_try_again() {
        for failure in [
            ApiFailure::Network,
            ApiFailure::Status {
                status: 500,
                code: Some("lookup_failed".into()),
            },
            ApiFailure::Status {
                status: 503,
                code: Some("unavailable".into()),
            },
            ApiFailure::Status {
                status: 404,
                code: None,
            },
        ] {
            assert_eq!(desk_state(Err(failure)), DeskState::TryAgain);
        }
    }

    #[test]
    fn results_say_nothing_was_looked_up() {
        assert_eq!(Results::for_query("  "), Results::Idle);
        let results = Results::for_query(" robocop ");
        assert_eq!(results, Results::NotSearched("robocop".into()));
        assert_eq!(results.title(), "“robocop”");
        assert!(results.message().contains("didn't look this up"));
        assert!(Results::Idle.message().contains("isn't hooked up yet"));
    }

    #[test]
    fn formats_are_movies_only_for_now() {
        assert_eq!(Format::default(), Format::All);
        let labels: Vec<_> = Format::ALL.iter().map(|f| f.label()).collect();
        assert_eq!(labels, ["All formats", "Movies (VHS)"]);
    }
}
