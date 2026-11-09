//! What the Store Floor and the clamshell case show for library answers.
//! Plain data and copy, so it can be tested without a browser.
//!
//! The API has no library listing yet (no movie list or movie detail
//! route), so the shelf shows what the server does know: the library
//! folders from `GET /api/v1/library/roots`, their health and how many
//! files each holds. A case is opened by media file id, the id the
//! playback endpoint takes.

use reelarr_types::{
    LibraryRootInfo, LibraryRootKind, MediaFile, MediaFileStatus, MediaKind, PlaybackInfo,
    PlaybackMethod, RootHealth, UnsupportedKind,
};

use crate::api::ApiFailure;
use crate::playback::{Notice, OFFLINE_MESSAGE, notice_for_failure};

/// The library folders endpoint.
pub const ROOTS_PATH: &str = "/api/v1/library/roots";

/// Where library folders are managed.
pub const SETTINGS_PATH: &str = "/employees-only";

/// The clamshell case page for a media file.
pub fn case_path(id: i64) -> String {
    format!("/case/{id}")
}

/// The player page for a media file.
pub fn watch_path(id: i64) -> String {
    format!("/watch/{id}")
}

/// A library folder's health, as the shelf shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FolderHealth {
    Online,
    /// Not checked yet. Treated as available, like the server does.
    Unchecked,
    /// Unreachable. `since` is a `YYYY-MM-DD` date (UTC).
    Offline {
        since: String,
        reason: String,
    },
    PermissionDenied {
        reason: String,
    },
}

impl FolderHealth {
    pub fn from_root(health: &RootHealth) -> Self {
        match health {
            RootHealth::Online => Self::Online,
            RootHealth::Unknown => Self::Unchecked,
            RootHealth::Offline { since, reason } => Self::Offline {
                since: utc_date(*since),
                reason: reason.clone(),
            },
            RootHealth::PermissionDenied { reason } => Self::PermissionDenied {
                reason: reason.clone(),
            },
        }
    }

    /// Short chip text. Every state has its own words, not just a colour.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Online => "Online",
            Self::Unchecked => "Not checked yet",
            Self::Offline { .. } => "Offline",
            Self::PermissionDenied { .. } => "No permission",
        }
    }

    pub fn is_available(&self) -> bool {
        matches!(self, Self::Online | Self::Unchecked)
    }

    /// A sentence for a folder that can't be read, or `None` if it can.
    pub fn problem(&self) -> Option<String> {
        match self {
            Self::Online | Self::Unchecked => None,
            Self::Offline { since, reason } => Some(format!(
                "Offline since {since}: {} Its tapes stay on file and come back \
                 when the folder does.",
                sentence(reason)
            )),
            Self::PermissionDenied { reason } => Some(format!(
                "Reelarr can't read this folder: {} Check the folder's permissions.",
                sentence(reason)
            )),
        }
    }
}

/// The server's reason as a sentence: ends with a full stop unless it
/// already ends with punctuation.
fn sentence(reason: &str) -> String {
    let reason = reason.trim();
    if reason.ends_with(['.', '?', '!']) {
        reason.to_string()
    } else {
        format!("{reason}.")
    }
}

/// One library folder on the Store Floor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    pub id: i64,
    pub path: String,
    pub health: FolderHealth,
    /// Files recorded under the folder, in any status.
    pub file_count: u64,
}

impl Folder {
    pub fn from_info(info: &LibraryRootInfo) -> Self {
        Self {
            id: info.root.id.0,
            path: info.root.path.clone(),
            health: FolderHealth::from_root(&info.health),
            file_count: info.file_count,
        }
    }

    /// "1 file on record", "12 files on record".
    pub fn count_label(&self) -> String {
        count_label(self.file_count)
    }
}

fn count_label(count: u64) -> String {
    match count {
        1 => "1 file on record".into(),
        n => format!("{n} files on record"),
    }
}

/// What the Store Floor shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FloorState {
    /// Waiting for the library folders.
    Loading,
    /// The folders could not be read; show this instead.
    Notice(Notice),
    /// The library folders. Manual-import folders aren't shelves, so they
    /// are left out.
    Floor(Vec<Folder>),
}

/// Maps the library folders request's outcome to the Store Floor.
pub fn floor_state(result: Result<Vec<LibraryRootInfo>, ApiFailure>) -> FloorState {
    match result {
        Ok(roots) => FloorState::Floor(
            roots
                .iter()
                .filter(|info| info.root.kind == LibraryRootKind::Library)
                .map(Folder::from_info)
                .collect(),
        ),
        Err(failure) => FloorState::Notice(match notice_for_failure(&failure) {
            // There's no single tape behind a folder list.
            Notice::NotFound => Notice::TryAgain,
            notice => notice,
        }),
    }
}

/// A summary line for the shelf, given the library folders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShelfSummary {
    /// No library folders are set up.
    NoFolders,
    /// Folders exist but no files are recorded under them.
    NoFiles,
    /// Files are on record, but the listing isn't available to show them.
    OnRecord { files: u64, unreachable: usize },
    /// Every folder that has files is unreachable.
    AllUnreachable { files: u64 },
}

pub fn shelf_summary(folders: &[Folder]) -> ShelfSummary {
    if folders.is_empty() {
        return ShelfSummary::NoFolders;
    }
    let files: u64 = folders.iter().map(|f| f.file_count).sum();
    if files == 0 {
        return ShelfSummary::NoFiles;
    }
    let unreachable = folders.iter().filter(|f| !f.health.is_available()).count();
    let reachable_files: u64 = folders
        .iter()
        .filter(|f| f.health.is_available())
        .map(|f| f.file_count)
        .sum();
    if reachable_files == 0 {
        ShelfSummary::AllUnreachable { files }
    } else {
        ShelfSummary::OnRecord { files, unreachable }
    }
}

impl ShelfSummary {
    pub fn title(&self) -> &'static str {
        match self {
            Self::NoFolders => "The shelves are bare",
            Self::NoFiles => "No tapes on the shelves yet",
            Self::OnRecord { .. } => "Shelf listing not available yet",
            Self::AllUnreachable { .. } => "The library is offline",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::NoFolders => "No library folders are set up yet. Add one in Employees Only \
                 and its movies will be stocked here."
                .into(),
            Self::NoFiles => "Your library folders are set up, but no movie files have been \
                 recorded in them yet."
                .into(),
            Self::OnRecord { files, unreachable } => {
                let mut message = format!(
                    "Reelarr has {} in your library folders, but this version can't list \
                     them on the shelves yet.",
                    count_label(*files)
                );
                match unreachable {
                    0 => {}
                    1 => message.push_str(" One library folder can't be read right now."),
                    n => {
                        message.push_str(&format!(" {n} library folders can't be read right now."))
                    }
                }
                message
            }
            Self::AllUnreachable { files } => format!(
                "Reelarr has {}, but none of the folders holding them can be read \
                 right now. Check that the shares are mounted.",
                count_label(*files)
            ),
        }
    }
}

/// Whether a tape can go in the deck, and if not, why. Built from a media
/// file and its folder's health.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TapeStatus {
    /// Present on a reachable folder.
    InStock,
    /// The file wasn't found by the last scan. `since` is `YYYY-MM-DD`.
    Missing { since: String },
    /// Its library folder is offline or unreadable.
    FolderOffline,
    /// A DVD or Blu-ray rip (image or folder): listed, not yet playable.
    Disc,
    /// An `.iso` that isn't a recognizable disc.
    UnrecognizedImage,
}

impl TapeStatus {
    /// The status of a library file. A missing file wins over its folder's
    /// health; an offline folder wins over the kind of file.
    pub fn of(file: &MediaFile, folder: Option<&RootHealth>) -> Self {
        if let MediaFileStatus::Missing { since } = file.status {
            return Self::Missing {
                since: utc_date(since),
            };
        }
        if folder.is_some_and(|h| !h.is_available()) {
            return Self::FolderOffline;
        }
        match file.kind {
            MediaKind::File => Self::InStock,
            MediaKind::DiscImage | MediaKind::DiscFolder => Self::Disc,
            MediaKind::UnrecognizedImage => Self::UnrecognizedImage,
        }
    }

    /// Sticker text for the case.
    pub fn label(&self) -> &'static str {
        match self {
            Self::InStock => "In stock",
            Self::Missing { .. } => "Missing",
            Self::FolderOffline => "Library offline",
            Self::Disc => "Not yet playable",
            Self::UnrecognizedImage => "Unrecognized disc image",
        }
    }

    /// Whether the tape can be put in the deck right now.
    pub fn playable(&self) -> bool {
        *self == Self::InStock
    }
}

/// What a clamshell case shows, from the playback answer for its media file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaseState {
    Loading,
    /// The tape is on file.
    Found(CaseTape),
    /// No case to show: not found, offline, signed out or an error.
    Notice(Notice),
}

/// A tape the server knows about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseTape {
    pub id: i64,
    pub runtime_minutes: Option<u64>,
    /// MIME type the browser would play, e.g. `video/mp4`.
    pub format: Option<String>,
    pub deck: DeckReadiness,
}

/// Whether the tape plays in this browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeckReadiness {
    /// The browser can play it as-is.
    Ready,
    /// A disc rip; holds the server's message.
    Disc(String),
    /// The browser can't play it yet; holds the server's reasons.
    Unsupported(Vec<String>),
}

impl DeckReadiness {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Ready => "In stock • Deck ready",
            Self::Disc(_) => "Not yet playable",
            Self::Unsupported(_) => "Can't play in this browser",
        }
    }
}

/// Copy for a case whose number isn't on file. Missing files answer 404
/// too, so this says so.
pub const CASE_NOT_FOUND_MESSAGE: &str = "There's no tape with that number on the shelves, \
     or its file has gone missing from the library folder.";

/// The main message for a case notice. Same words as the player, except
/// for not found.
pub fn case_notice_message(notice: &Notice) -> String {
    match notice {
        Notice::NotFound => CASE_NOT_FOUND_MESSAGE.into(),
        Notice::Offline => OFFLINE_MESSAGE.into(),
        other => other.message(),
    }
}

/// Maps the playback request's outcome for a case's media file.
pub fn case_state(id: Option<i64>, result: Result<PlaybackInfo, ApiFailure>) -> CaseState {
    let Some(id) = id else {
        return CaseState::Notice(Notice::NotFound);
    };
    match result {
        Ok(info) => {
            let disc = info.unsupported.iter().find(|r| {
                matches!(
                    r.kind,
                    UnsupportedKind::DiscImage | UnsupportedKind::DiscFolder
                )
            });
            let deck = match (disc, info.method) {
                (Some(disc), _) => DeckReadiness::Disc(disc.message.clone()),
                (None, PlaybackMethod::DirectPlay) => DeckReadiness::Ready,
                (None, PlaybackMethod::Unsupported) => DeckReadiness::Unsupported(
                    info.unsupported.iter().map(|r| r.message.clone()).collect(),
                ),
            };
            CaseState::Found(CaseTape {
                id,
                runtime_minutes: info.duration_ms.map(|ms| (ms + 30_000) / 60_000),
                format: info.mime_type,
                deck,
            })
        }
        Err(failure) => CaseState::Notice(notice_for_failure(&failure)),
    }
}

/// A Unix time as a `YYYY-MM-DD` date in UTC.
pub fn utc_date(unix_seconds: i64) -> String {
    // Civil-from-days over the proleptic Gregorian calendar.
    let days = unix_seconds.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use reelarr_types::{
        LibraryRoot, LibraryRootId, MediaFileId, UnsupportedReason, playback::PlaybackInfo,
    };

    fn root(id: i64, kind: LibraryRootKind, health: RootHealth, files: u64) -> LibraryRootInfo {
        LibraryRootInfo {
            root: LibraryRoot {
                id: LibraryRootId(id),
                path: format!("/media/movies-{id}"),
                kind,
                rescan_interval_secs: None,
            },
            health,
            health_checked_at: None,
            file_count: files,
        }
    }

    fn offline() -> RootHealth {
        RootHealth::Offline {
            since: 1_762_646_400,
            reason: "not mounted".into(),
        }
    }

    fn folders(roots: Vec<LibraryRootInfo>) -> Vec<Folder> {
        match floor_state(Ok(roots)) {
            FloorState::Floor(folders) => folders,
            other => panic!("expected folders, got {other:?}"),
        }
    }

    fn file(status: MediaFileStatus, kind: MediaKind) -> MediaFile {
        MediaFile {
            id: MediaFileId(3),
            movie_id: None,
            root_id: LibraryRootId(1),
            path: "/media/movies/a.mp4".into(),
            size_bytes: 1,
            modified_at: 0,
            status,
            probe: None,
            kind,
        }
    }

    fn playback(method: PlaybackMethod, unsupported: Vec<UnsupportedReason>) -> PlaybackInfo {
        let direct = method == PlaybackMethod::DirectPlay;
        PlaybackInfo {
            media_file_id: MediaFileId(9),
            method,
            stream_url: direct.then(|| "/api/v1/media/9/stream".into()),
            mime_type: direct.then(|| "video/mp4".into()),
            unsupported,
            duration_ms: Some(5_400_000),
        }
    }

    #[test]
    fn no_folders_is_a_bare_shelf_that_points_to_settings() {
        let summary = shelf_summary(&folders(Vec::new()));
        assert_eq!(summary, ShelfSummary::NoFolders);
        assert!(summary.message().contains("Employees Only"));
    }

    #[test]
    fn manual_import_folders_are_not_shelves() {
        let f = folders(vec![root(
            1,
            LibraryRootKind::ManualImport,
            RootHealth::Online,
            4,
        )]);
        assert!(f.is_empty());
    }

    #[test]
    fn empty_folders_say_no_tapes_yet() {
        let f = folders(vec![root(
            1,
            LibraryRootKind::Library,
            RootHealth::Online,
            0,
        )]);
        assert_eq!(shelf_summary(&f), ShelfSummary::NoFiles);
    }

    #[test]
    fn files_on_record_are_counted_not_hidden() {
        let f = folders(vec![
            root(1, LibraryRootKind::Library, RootHealth::Online, 2),
            root(2, LibraryRootKind::Library, offline(), 1),
        ]);
        let summary = shelf_summary(&f);
        assert_eq!(
            summary,
            ShelfSummary::OnRecord {
                files: 3,
                unreachable: 1
            }
        );
        assert!(summary.message().contains("3 files on record"));
        assert!(
            summary
                .message()
                .contains("One library folder can't be read")
        );
    }

    #[test]
    fn an_offline_library_is_not_an_empty_one() {
        let f = folders(vec![root(1, LibraryRootKind::Library, offline(), 1)]);
        let summary = shelf_summary(&f);
        assert_eq!(summary, ShelfSummary::AllUnreachable { files: 1 });
        assert!(summary.message().contains("1 file on record"));
        assert!(summary.message().contains("mounted"));
    }

    #[test]
    fn folder_health_has_words_and_reasons() {
        let f = Folder::from_info(&root(1, LibraryRootKind::Library, offline(), 0));
        assert_eq!(f.health.label(), "Offline");
        let problem = f.health.problem().unwrap();
        assert!(problem.contains("2025-11-09"), "{problem}");
        assert!(problem.contains("not mounted"));
        assert!(problem.contains("stay on file"));
        assert!(problem.contains(": not mounted. Its tapes"));
        let asked = FolderHealth::Offline {
            since: "2026-11-09".into(),
            reason: "folder not found; is the share mounted?".into(),
        };
        assert!(asked.problem().unwrap().contains("mounted? Its tapes"));

        let denied = FolderHealth::from_root(&RootHealth::PermissionDenied {
            reason: "EACCES".into(),
        });
        assert_eq!(denied.label(), "No permission");
        assert!(denied.problem().unwrap().contains("EACCES"));

        assert_eq!(
            FolderHealth::from_root(&RootHealth::Unknown).label(),
            "Not checked yet"
        );
        assert!(
            FolderHealth::from_root(&RootHealth::Unknown)
                .problem()
                .is_none()
        );
        assert!(FolderHealth::Online.is_available());
    }

    #[test]
    fn folder_list_errors_map_like_the_player() {
        let err = |status, code: &str| ApiFailure::Status {
            status,
            code: Some(code.into()),
        };
        assert_eq!(
            floor_state(Err(err(401, "unauthorized"))),
            FloorState::Notice(Notice::SignIn)
        );
        assert_eq!(
            floor_state(Err(err(500, "internal"))),
            FloorState::Notice(Notice::TryAgain)
        );
        assert_eq!(
            floor_state(Err(err(404, "not_found"))),
            FloorState::Notice(Notice::TryAgain)
        );
        assert_eq!(
            floor_state(Err(ApiFailure::Network)),
            FloorState::Notice(Notice::TryAgain)
        );
    }

    #[test]
    fn tape_status_shows_missing_offline_and_disc() {
        let present = MediaFileStatus::Present;
        assert_eq!(
            TapeStatus::of(&file(present, MediaKind::File), Some(&RootHealth::Online)),
            TapeStatus::InStock
        );
        assert!(TapeStatus::InStock.playable());
        let missing = TapeStatus::of(
            &file(MediaFileStatus::Missing { since: 0 }, MediaKind::File),
            Some(&offline()),
        );
        assert_eq!(
            missing,
            TapeStatus::Missing {
                since: "1970-01-01".into()
            }
        );
        assert_eq!(missing.label(), "Missing");
        assert_eq!(
            TapeStatus::of(&file(present, MediaKind::DiscImage), Some(&offline())),
            TapeStatus::FolderOffline
        );
        for kind in [MediaKind::DiscImage, MediaKind::DiscFolder] {
            let status = TapeStatus::of(&file(present, kind), None);
            assert_eq!(status, TapeStatus::Disc);
            assert_eq!(status.label(), "Not yet playable");
            assert!(!status.playable());
        }
        assert_eq!(
            TapeStatus::of(&file(present, MediaKind::UnrecognizedImage), None),
            TapeStatus::UnrecognizedImage
        );
    }

    #[test]
    fn case_for_a_direct_play_tape_is_deck_ready() {
        let state = case_state(
            Some(9),
            Ok(playback(PlaybackMethod::DirectPlay, Vec::new())),
        );
        assert_eq!(
            state,
            CaseState::Found(CaseTape {
                id: 9,
                runtime_minutes: Some(90),
                format: Some("video/mp4".into()),
                deck: DeckReadiness::Ready,
            })
        );
    }

    #[test]
    fn case_for_disc_rips_keeps_the_server_message() {
        for (kind, message) in [
            (UnsupportedKind::DiscImage, "Disc image (not yet playable)"),
            (
                UnsupportedKind::DiscFolder,
                "DVD/Blu-ray folder (not yet playable)",
            ),
        ] {
            let state = case_state(
                Some(9),
                Ok(playback(
                    PlaybackMethod::Unsupported,
                    vec![UnsupportedReason {
                        kind,
                        name: None,
                        message: message.into(),
                    }],
                )),
            );
            let CaseState::Found(tape) = state else {
                panic!("expected a tape, got {state:?}");
            };
            assert_eq!(tape.deck, DeckReadiness::Disc(message.into()));
            assert_eq!(tape.deck.label(), "Not yet playable");
        }
    }

    #[test]
    fn case_for_an_unsupported_file_lists_the_reasons() {
        let state = case_state(
            Some(9),
            Ok(playback(
                PlaybackMethod::Unsupported,
                vec![UnsupportedReason {
                    kind: UnsupportedKind::VideoCodec,
                    name: Some("hevc".into()),
                    message: "HEVC video isn't supported".into(),
                }],
            )),
        );
        let CaseState::Found(tape) = state else {
            panic!("expected a tape, got {state:?}");
        };
        assert_eq!(
            tape.deck,
            DeckReadiness::Unsupported(vec!["HEVC video isn't supported".into()])
        );
    }

    #[test]
    fn case_errors_say_not_found_offline_or_sign_in() {
        let err = |status, code: &str| ApiFailure::Status {
            status,
            code: Some(code.into()),
        };
        assert_eq!(
            case_state(None, Ok(playback(PlaybackMethod::DirectPlay, Vec::new()))),
            CaseState::Notice(Notice::NotFound)
        );
        assert_eq!(
            case_state(Some(4), Err(err(404, "not_found"))),
            CaseState::Notice(Notice::NotFound)
        );
        assert_eq!(
            case_notice_message(&Notice::NotFound),
            CASE_NOT_FOUND_MESSAGE
        );
        assert_eq!(
            case_state(Some(4), Err(err(503, "unavailable"))),
            CaseState::Notice(Notice::Offline)
        );
        assert_eq!(
            case_notice_message(&Notice::Offline),
            "This movie's library folder is offline. Check that the share is mounted."
        );
        assert_eq!(
            case_state(Some(4), Err(err(401, "unauthorized"))),
            CaseState::Notice(Notice::SignIn)
        );
        assert_eq!(
            case_state(Some(4), Err(err(500, "lookup_failed"))),
            CaseState::Notice(Notice::TryAgain)
        );
    }

    #[test]
    fn dates_are_utc_days() {
        assert_eq!(utc_date(0), "1970-01-01");
        assert_eq!(utc_date(951_782_400), "2000-02-29");
        assert_eq!(utc_date(1_762_646_400), "2025-11-09");
        assert_eq!(utc_date(-86_400), "1969-12-31");
    }

    #[test]
    fn paths() {
        assert_eq!(case_path(7), "/case/7");
        assert_eq!(watch_path(7), "/watch/7");
    }
}
