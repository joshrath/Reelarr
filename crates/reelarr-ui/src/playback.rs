//! What the VCR player shows for a playback answer. Plain data and copy, so
//! it can be tested without a browser.

use reelarr_types::{PlaybackInfo, PlaybackMethod, UnsupportedKind};

use crate::api::ApiFailure;

/// Where to send someone whose session has ended.
pub const SIGN_IN_PATH: &str = "/login";

/// The playback endpoint for a media file.
pub fn playback_path(id: i64) -> String {
    format!("/api/v1/media/{id}/playback")
}

/// What the deck has loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeckState {
    /// Waiting for the playback answer.
    Loading,
    /// A tape the browser can play directly.
    Ready {
        /// The stream URL exactly as the server gave it.
        stream_url: String,
        duration_ms: Option<u64>,
    },
    /// Nothing to play; show this message on the screen instead.
    Notice(Notice),
}

/// A message shown on the screen in place of the picture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// 401: the session is missing or has expired.
    SignIn,
    /// 404, or an id that isn't a number.
    NotFound,
    /// 503: the library folder is offline.
    Offline,
    /// 500 or anything unexpected.
    TryAgain,
    /// A DVD or Blu-ray rip (ISO image or disc folder). Holds the server's
    /// message.
    Disc(String),
    /// The browser can't play this file yet. Holds the server's reasons.
    Unsupported(Vec<String>),
    /// The `<video>` stopped with this `MediaError` code while the playback
    /// answer is still fine, so the session isn't the cause.
    Stopped(u16),
}

pub const OFFLINE_MESSAGE: &str =
    "This movie's library folder is offline. Check that the share is mounted.";
pub const TRY_AGAIN_MESSAGE: &str =
    "Something went wrong loading this tape. Try again in a moment.";
pub const SIGN_IN_MESSAGE: &str = "Your session has ended. Sign in again to play this tape.";
pub const NOT_FOUND_MESSAGE: &str = "There's no tape with that number on the shelves.";
pub const UNSUPPORTED_MESSAGE: &str =
    "Your browser can't play this file as it is, and converting it isn't available yet.";

impl Notice {
    /// Short heading for the screen.
    pub fn title(&self) -> &'static str {
        match self {
            Notice::SignIn => "Sign in to watch",
            Notice::NotFound => "Tape not found",
            Notice::Offline => "Library offline",
            Notice::TryAgain => "Couldn't load the tape",
            Notice::Disc(_) => "Can't play this tape",
            Notice::Unsupported(_) => "Can't play this tape",
            Notice::Stopped(_) => "Playback stopped",
        }
    }

    /// The main message.
    pub fn message(&self) -> String {
        match self {
            Notice::SignIn => SIGN_IN_MESSAGE.into(),
            Notice::NotFound => NOT_FOUND_MESSAGE.into(),
            Notice::Offline => OFFLINE_MESSAGE.into(),
            Notice::TryAgain => TRY_AGAIN_MESSAGE.into(),
            Notice::Disc(message) => message.clone(),
            Notice::Unsupported(_) => UNSUPPORTED_MESSAGE.into(),
            Notice::Stopped(code) => media_error_message(*code),
        }
    }

    /// Extra detail lines, e.g. which codec is unsupported.
    pub fn details(&self) -> &[String] {
        match self {
            Notice::Unsupported(reasons) => reasons,
            _ => &[],
        }
    }
}

/// Maps the playback request's outcome to what the deck shows.
pub fn deck_state(result: Result<PlaybackInfo, ApiFailure>) -> DeckState {
    match result {
        Ok(info) => from_info(info),
        Err(failure) => DeckState::Notice(notice_for_failure(&failure)),
    }
}

fn from_info(info: PlaybackInfo) -> DeckState {
    if let Some(disc) = info.unsupported.iter().find(|r| {
        matches!(
            r.kind,
            UnsupportedKind::DiscImage | UnsupportedKind::DiscFolder
        )
    }) {
        return DeckState::Notice(Notice::Disc(disc.message.clone()));
    }
    match (info.method, info.stream_url) {
        (PlaybackMethod::DirectPlay, Some(stream_url)) => DeckState::Ready {
            stream_url,
            duration_ms: info.duration_ms,
        },
        (PlaybackMethod::DirectPlay, None) => DeckState::Notice(Notice::TryAgain),
        (PlaybackMethod::Unsupported, _) => DeckState::Notice(Notice::Unsupported(
            info.unsupported.into_iter().map(|r| r.message).collect(),
        )),
    }
}

/// Maps an API failure to a notice. The error `code` wins; the status is the
/// fallback when there's no JSON body.
pub fn notice_for_failure(failure: &ApiFailure) -> Notice {
    let ApiFailure::Status { status, code } = failure else {
        return Notice::TryAgain;
    };
    match (code.as_deref(), status) {
        (Some("unauthorized"), _) | (None, 401) => Notice::SignIn,
        (Some("not_found"), _) | (None, 404) => Notice::NotFound,
        (Some("unavailable"), _) | (None, 503) => Notice::Offline,
        _ => Notice::TryAgain,
    }
}

/// What the deck shows after the `<video>` reports a `MediaError`, once the
/// playback answer has been fetched again.
///
/// The browser reports a stream that fails with 401 (a session that expired
/// mid-film) as a generic media error, so the re-fetch tells the cases
/// apart: only a 401 asks to sign in, other failures get the usual notices,
/// and a playback answer that is still fine means the tape itself stopped.
pub fn notice_after_media_error(code: u16, recheck: Result<PlaybackInfo, ApiFailure>) -> Notice {
    match deck_state(recheck) {
        DeckState::Notice(notice) => notice,
        DeckState::Loading | DeckState::Ready { .. } => Notice::Stopped(code),
    }
}

/// Copy for a `<video>` `MediaError` code.
pub fn media_error_message(code: u16) -> String {
    match code {
        1 => "Playback was stopped before the tape finished loading.",
        2 => "The connection dropped while loading the tape. Try again in a moment.",
        3 => "Your browser couldn't decode this tape.",
        4 => "The tape couldn't be loaded, or your browser can't play this file.",
        _ => "The tape stopped unexpectedly. Try again in a moment.",
    }
    .into()
}

/// A tape counter reading, `HH:MM:SS`. Unknown or negative times read
/// `--:--:--`.
pub fn format_counter(seconds: f64) -> String {
    if !seconds.is_finite() || seconds < 0.0 {
        return "--:--:--".into();
    }
    let total = seconds.floor() as u64;
    format!(
        "{:02}:{:02}:{:02}",
        total / 3600,
        (total % 3600) / 60,
        total % 60
    )
}

/// The media id in a `/watch/:id` path segment, if it is a valid one.
pub fn parse_media_id(segment: &str) -> Option<i64> {
    segment.parse::<i64>().ok().filter(|id| *id > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reelarr_types::{MediaFileId, UnsupportedReason};

    fn status(status: u16, code: &str) -> ApiFailure {
        ApiFailure::Status {
            status,
            code: Some(code.into()),
        }
    }

    fn info(method: PlaybackMethod, unsupported: Vec<UnsupportedReason>) -> PlaybackInfo {
        let direct = method == PlaybackMethod::DirectPlay;
        PlaybackInfo {
            media_file_id: MediaFileId(7),
            method,
            stream_url: direct.then(|| "/api/v1/media/7/stream".into()),
            mime_type: direct.then(|| "video/mp4".into()),
            unsupported,
            duration_ms: Some(90_000),
        }
    }

    #[test]
    fn unauthorized_asks_to_sign_in() {
        let notice = notice_for_failure(&status(401, "unauthorized"));
        assert_eq!(notice, Notice::SignIn);
        assert!(notice.message().contains("Sign in again"));
        assert_eq!(
            notice_for_failure(&ApiFailure::Status {
                status: 401,
                code: None
            }),
            Notice::SignIn
        );
    }

    #[test]
    fn unavailable_says_the_share_is_offline() {
        let notice = notice_for_failure(&status(503, "unavailable"));
        assert_eq!(notice, Notice::Offline);
        assert_eq!(
            notice.message(),
            "This movie's library folder is offline. Check that the share is mounted."
        );
    }

    #[test]
    fn lookup_failed_is_a_generic_try_again() {
        let notice = notice_for_failure(&status(500, "lookup_failed"));
        assert_eq!(notice, Notice::TryAgain);
        assert!(notice.message().contains("Try again"));
        assert_eq!(notice_for_failure(&ApiFailure::Network), Notice::TryAgain);
        assert_eq!(
            notice_for_failure(&ApiFailure::Status {
                status: 502,
                code: None
            }),
            Notice::TryAgain
        );
    }

    #[test]
    fn not_found_says_so() {
        assert_eq!(
            notice_for_failure(&status(404, "not_found")),
            Notice::NotFound
        );
        assert_eq!(
            deck_state(Err(status(404, "not_found"))),
            DeckState::Notice(Notice::NotFound)
        );
    }

    #[test]
    fn disc_image_shows_the_server_message() {
        let state = deck_state(Ok(info(
            PlaybackMethod::Unsupported,
            vec![UnsupportedReason {
                kind: UnsupportedKind::DiscImage,
                name: None,
                message: "Disc image (not yet playable)".into(),
            }],
        )));
        let DeckState::Notice(notice) = state else {
            panic!("expected a notice, got {state:?}");
        };
        assert_eq!(notice.message(), "Disc image (not yet playable)");
    }

    #[test]
    fn disc_folder_shows_the_server_message() {
        let state = deck_state(Ok(info(
            PlaybackMethod::Unsupported,
            vec![UnsupportedReason {
                kind: UnsupportedKind::DiscFolder,
                name: None,
                message: "DVD/Blu-ray folder (not yet playable)".into(),
            }],
        )));
        let DeckState::Notice(notice) = state else {
            panic!("expected a notice, got {state:?}");
        };
        assert!(matches!(notice, Notice::Disc(_)));
        assert_eq!(notice.message(), "DVD/Blu-ray folder (not yet playable)");
    }

    #[test]
    fn unsupported_lists_the_reasons() {
        let state = deck_state(Ok(info(
            PlaybackMethod::Unsupported,
            vec![UnsupportedReason {
                kind: UnsupportedKind::VideoCodec,
                name: Some("hevc".into()),
                message: "HEVC video isn't supported by this browser".into(),
            }],
        )));
        let DeckState::Notice(notice) = state else {
            panic!("expected a notice, got {state:?}");
        };
        assert_eq!(
            notice.details(),
            ["HEVC video isn't supported by this browser".to_string()]
        );
    }

    #[test]
    fn direct_play_uses_the_stream_url_as_is() {
        assert_eq!(
            deck_state(Ok(info(PlaybackMethod::DirectPlay, Vec::new()))),
            DeckState::Ready {
                stream_url: "/api/v1/media/7/stream".into(),
                duration_ms: Some(90_000),
            }
        );
    }

    #[test]
    fn media_errors_dont_ask_to_sign_in() {
        for code in 0..=5 {
            let message = media_error_message(code);
            assert!(!message.is_empty());
            assert!(!message.to_lowercase().contains("sign in"), "{message}");
        }
    }

    #[test]
    fn media_error_asks_to_sign_in_only_when_the_recheck_is_401() {
        assert_eq!(
            notice_after_media_error(2, Err(status(401, "unauthorized"))),
            Notice::SignIn
        );
        assert_eq!(
            notice_after_media_error(
                2,
                Err(ApiFailure::Status {
                    status: 401,
                    code: None
                })
            ),
            Notice::SignIn
        );
    }

    #[test]
    fn media_error_maps_other_recheck_failures_through_the_usual_notices() {
        assert_eq!(
            notice_after_media_error(2, Err(status(503, "unavailable"))),
            Notice::Offline
        );
        assert_eq!(
            notice_after_media_error(2, Err(status(404, "not_found"))),
            Notice::NotFound
        );
        assert_eq!(
            notice_after_media_error(2, Err(status(500, "lookup_failed"))),
            Notice::TryAgain
        );
        assert_eq!(
            notice_after_media_error(2, Err(ApiFailure::Network)),
            Notice::TryAgain
        );
    }

    #[test]
    fn media_error_with_a_fine_recheck_says_the_tape_stopped() {
        let notice = notice_after_media_error(3, Ok(info(PlaybackMethod::DirectPlay, Vec::new())));
        assert_eq!(notice, Notice::Stopped(3));
        assert_eq!(notice.title(), "Playback stopped");
        assert_eq!(notice.message(), media_error_message(3));
    }

    #[test]
    fn media_error_with_an_unsupported_recheck_shows_the_reasons() {
        let notice = notice_after_media_error(
            4,
            Ok(info(
                PlaybackMethod::Unsupported,
                vec![UnsupportedReason {
                    kind: UnsupportedKind::AudioCodec,
                    name: Some("dts".into()),
                    message: "DTS audio isn't supported by this browser".into(),
                }],
            )),
        );
        assert_eq!(
            notice,
            Notice::Unsupported(vec!["DTS audio isn't supported by this browser".into()])
        );
    }

    #[test]
    fn counter_formats_hours() {
        assert_eq!(format_counter(3764.9), "01:02:44");
        assert_eq!(format_counter(0.0), "00:00:00");
        assert_eq!(format_counter(f64::NAN), "--:--:--");
        assert_eq!(format_counter(f64::INFINITY), "--:--:--");
    }

    #[test]
    fn media_ids_must_be_positive_numbers() {
        assert_eq!(parse_media_id("12"), Some(12));
        assert_eq!(parse_media_id("0"), None);
        assert_eq!(parse_media_id("abc"), None);
        assert_eq!(playback_path(12), "/api/v1/media/12/playback");
    }
}
