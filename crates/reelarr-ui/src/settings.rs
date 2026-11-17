//! What Employees Only shows for the settings the API has: library folders
//! (`/api/v1/library/roots`) and the Usenet server (`/api/v1/settings/nntp`).
//! Plain data, copy and form checks, so they can be tested without a
//! browser.
//!
//! The NNTP password is write-only. The API answers with `has_password`
//! only; the form never holds the saved password, and an empty password
//! field means "keep the saved one".

use reelarr_types::{
    DEFAULT_RESCAN_INTERVAL_SECS, LibraryRootInfo, LibraryRootKind, NewLibraryRoot,
    nntp::{
        NNTP_DEFAULT_CONNECT_TIMEOUT_SECS, NNTP_DEFAULT_CONNECTIONS, NNTP_DEFAULT_IO_TIMEOUT_SECS,
        NNTP_DEFAULT_PORT, NNTP_DEFAULT_RETRIES, NNTP_MAX_CONNECTIONS, NNTP_MAX_RETRIES,
        NNTP_MAX_TIMEOUT_SECS, NNTP_PLAINTEXT_PORT, NntpServerSettings, NntpServerUpdate,
        NntpTestOutcome, NntpTestResult, PasswordUpdate,
    },
};

use crate::api::SendFailure;
use crate::library::FolderHealth;
use crate::staff::{action_error, utc_datetime};

pub use crate::library::ROOTS_PATH;
pub const NNTP_PATH: &str = "/api/v1/settings/nntp";
pub const NNTP_TEST_PATH: &str = "/api/v1/settings/nntp/test";

/// Error codes whose server message the NNTP form may repeat: static
/// validation text that never contains the values sent.
pub const NNTP_SHOWN_CODES: &[&str] = &["invalid_settings", "password_required"];

/// `POST /api/v1/library/roots/{id}/check`.
pub fn root_check_path(id: i64) -> String {
    format!("{ROOTS_PATH}/{id}/check")
}

/// `DELETE /api/v1/library/roots/{id}`. The server refuses to drop a
/// folder with files on record unless `forget_files` is set.
pub fn root_remove_path(id: i64, forget_files: bool) -> String {
    if forget_files {
        format!("{ROOTS_PATH}/{id}?forget_files=true")
    } else {
        format!("{ROOTS_PATH}/{id}")
    }
}

pub fn kind_label(kind: LibraryRootKind) -> &'static str {
    match kind {
        LibraryRootKind::Library => "Library",
        LibraryRootKind::ManualImport => "Manual import",
    }
}

pub fn kind_value(kind: LibraryRootKind) -> &'static str {
    match kind {
        LibraryRootKind::Library => "library",
        LibraryRootKind::ManualImport => "manual_import",
    }
}

pub fn kind_from_value(value: &str) -> Option<LibraryRootKind> {
    [LibraryRootKind::Library, LibraryRootKind::ManualImport]
        .into_iter()
        .find(|k| kind_value(*k) == value)
}

/// "Rescans every 6 hours", "No scheduled rescans".
pub fn rescan_label(interval_secs: Option<u64>) -> String {
    match interval_secs {
        None => "No scheduled rescans".into(),
        Some(secs) => format!("Rescans every {}", duration_words(secs)),
    }
}

fn duration_words(secs: u64) -> String {
    let unit = |n: u64, one: &str| {
        if n == 1 {
            one.to_string()
        } else {
            format!("{n} {one}s")
        }
    };
    if secs >= 86_400 && secs.is_multiple_of(86_400) {
        unit(secs / 86_400, "day")
    } else if secs >= 3600 && secs.is_multiple_of(3600) {
        unit(secs / 3600, "hour")
    } else if secs >= 60 && secs.is_multiple_of(60) {
        unit(secs / 60, "minute")
    } else {
        unit(secs, "second")
    }
}

/// The rescan choices offered when adding a folder, as `(seconds, label)`.
/// The server's own default comes first.
pub const RESCAN_CHOICES: [(u64, &str); 5] = [
    (DEFAULT_RESCAN_INTERVAL_SECS, "Every 6 hours (default)"),
    (3600, "Every hour"),
    (12 * 3600, "Every 12 hours"),
    (86_400, "Every day"),
    (7 * 86_400, "Every week"),
];

/// One library folder row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootRow {
    pub id: i64,
    pub path: String,
    pub kind: &'static str,
    pub rescan: String,
    pub health: FolderHealth,
    pub files: u64,
    pub checked: String,
    /// A `Library` folder (not manual import): the only kind that can be
    /// the main library folder.
    pub is_library: bool,
    /// Picked as the main library folder (`is_default`).
    pub is_default: bool,
}

impl RootRow {
    pub fn of(info: &LibraryRootInfo) -> Self {
        Self {
            id: info.root.id.0,
            path: info.root.path.clone(),
            kind: kind_label(info.root.kind),
            rescan: rescan_label(info.root.rescan_interval_secs),
            health: FolderHealth::from_root(&info.health),
            files: info.file_count,
            checked: match info.health_checked_at {
                Some(at) => format!("Checked {}", utc_datetime(at)),
                None => "Never checked".into(),
            },
            is_library: info.root.kind == LibraryRootKind::Library,
            is_default: info.root.is_default && info.root.kind == LibraryRootKind::Library,
        }
    }

    pub fn files_label(&self) -> String {
        match self.files {
            1 => "1 file on record".into(),
            n => format!("{n} files on record"),
        }
    }

    /// The confirm question for removing this folder. Says so when it's
    /// the main library folder, and what happens to imports then.
    pub fn remove_question(&self, main: &MainFolder) -> String {
        let mut question = match self.files {
            0 => format!(
                "Remove {} from Reelarr? Nothing on disk is touched.",
                self.path
            ),
            n => format!(
                "Remove {}? Reelarr forgets the {} on record under it. The files on disk are not touched.",
                self.path,
                if n == 1 {
                    "1 file".to_string()
                } else {
                    format!("{n} files")
                }
            ),
        };
        match main {
            MainFolder::Chosen(id) if *id == self.id => question.push_str(
                " It's the main library folder: new imports wait until you pick another one.",
            ),
            MainFolder::Implicit(id) if *id == self.id => question.push_str(
                " It's your only library folder: new imports wait until you add another one.",
            ),
            _ => {}
        }
        question
    }

    /// Whether removing needs `forget_files`.
    pub fn forget_files(&self) -> bool {
        self.files > 0
    }
}

/// Which library folder new imports go into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MainFolder {
    /// No library folders (manual-import folders don't count).
    NoLibrary,
    /// One library folder and no pick: it's used without asking.
    Implicit(i64),
    /// The folder picked as main (`is_default`).
    Chosen(i64),
    /// Two or more library folders and none picked: imports wait.
    NeedsPick,
}

impl MainFolder {
    pub fn of(roots: &[LibraryRootInfo]) -> Self {
        let library: Vec<_> = roots
            .iter()
            .filter(|i| i.root.kind == LibraryRootKind::Library)
            .collect();
        if let Some(main) = library.iter().find(|i| i.root.is_default) {
            return Self::Chosen(main.root.id.0);
        }
        match library.as_slice() {
            [] => Self::NoLibrary,
            [only] => Self::Implicit(only.root.id.0),
            _ => Self::NeedsPick,
        }
    }

    /// The folder imports go into, picked or implicit.
    pub fn id(self) -> Option<i64> {
        match self {
            Self::Implicit(id) | Self::Chosen(id) => Some(id),
            Self::NoLibrary | Self::NeedsPick => None,
        }
    }

    /// Whether `row` gets a "Make main library folder" button: library
    /// folders only, when there's a choice to make, never the current one.
    pub fn can_pick(self, row: &RootRow) -> bool {
        row.is_library
            && match self {
                Self::NeedsPick => true,
                Self::Chosen(id) => id != row.id,
                Self::NoLibrary | Self::Implicit(_) => false,
            }
    }

    /// The tag on the main folder's row, in words.
    pub fn tag(self, row: &RootRow) -> Option<&'static str> {
        match self {
            Self::Chosen(id) if id == row.id => Some("Main library folder"),
            Self::Implicit(id) if id == row.id => Some("Main library"),
            _ => None,
        }
    }
}

pub const PICK_MAIN_TITLE: &str = "Pick a main library folder";
pub const PICK_MAIN_WAITING: &str = "Waiting for you to pick a main library folder — finished downloads stay ready for import until you do.";

/// The prompt when several library folders exist and none is main.
pub fn pick_main_message(roots: &[LibraryRootInfo]) -> String {
    let n = roots
        .iter()
        .filter(|i| i.root.kind == LibraryRootKind::Library)
        .count();
    format!(
        "You have {n} library folders. Which one is the main library folder, the one new imports go into? {PICK_MAIN_WAITING}"
    )
}

/// What The Back Room says when downloads are waiting to be imported, by
/// the state of the library folders: `(title, message)`, or `None` when
/// imports have somewhere to go.
pub fn import_wait(main: MainFolder) -> Option<(&'static str, &'static str)> {
    match main {
        MainFolder::NeedsPick => Some((PICK_MAIN_TITLE, PICK_MAIN_WAITING)),
        MainFolder::NoLibrary => Some((
            "Add a library folder",
            "Waiting for a library folder — finished downloads stay ready for import until you add one.",
        )),
        MainFolder::Implicit(_) | MainFolder::Chosen(_) => None,
    }
}

/// `PUT /api/v1/library/roots/{id}/default`.
pub fn root_default_path(id: i64) -> String {
    format!("{ROOTS_PATH}/{id}/default")
}

/// Plain words for a failed folder change. Server text is only repeated
/// for duplicates and overlaps, which quote the path that was typed.
pub fn root_error(failure: &SendFailure) -> String {
    let message = failure.message.as_deref().unwrap_or_default();
    match failure.code() {
        Some("invalid_path") if message.contains("not a folder") => {
            "That path is a file, not a folder.".into()
        }
        Some("invalid_path") if message.contains(". or ..") => {
            "The path can't contain . or .. parts.".into()
        }
        Some("invalid_path") if message.contains("absolute") => {
            "Use the full path inside the Reelarr container, starting with /.".into()
        }
        Some("invalid_path") => "Reelarr can't use that path. Check it and try again.".into(),
        Some("default_not_library") => {
            "Only a library folder can be the main library folder, not a manual-import folder."
                .into()
        }
        Some("not_found") => {
            "That folder isn't set up in Reelarr anymore. The list has been refreshed.".into()
        }
        Some("root_has_files") => {
            "Files were found in this folder since the list was read. Check the count and confirm again."
                .into()
        }
        _ => action_error(failure, &["duplicate_root", "overlapping_root"]),
    }
}

/// Checks a folder path the way the server will, so obvious mistakes are
/// caught before sending. Returns the problem in words.
pub fn validate_root_path(path: &str) -> Result<String, &'static str> {
    let path = path.trim();
    if path.is_empty() {
        return Err("Enter the folder's path.");
    }
    if !path.starts_with('/') {
        return Err("Use the full path inside the Reelarr container, starting with /.");
    }
    if path.split('/').any(|part| part == "." || part == "..") {
        return Err("The path can't contain . or .. parts.");
    }
    Ok(path.to_string())
}

/// The add-folder request, or the problem with the form. `is_default` is
/// only sent for a library folder.
pub fn new_root(
    path: &str,
    kind: &str,
    rescan: &str,
    is_default: bool,
) -> Result<NewLibraryRoot, &'static str> {
    let path = validate_root_path(path)?;
    let kind = kind_from_value(kind).ok_or("Choose what the folder holds.")?;
    let rescan_interval_secs = match rescan.parse::<u64>() {
        Ok(secs) if RESCAN_CHOICES.iter().any(|(s, _)| *s == secs) => Some(secs),
        _ => return Err("Choose how often to rescan."),
    };
    Ok(NewLibraryRoot {
        path,
        kind,
        rescan_interval_secs,
        is_default: is_default && kind == LibraryRootKind::Library,
    })
}

/// A field on the NNTP form, for pointing at the one with a problem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NntpField {
    Host,
    Port,
    Username,
    Password,
    Connections,
    ConnectTimeout,
    IoTimeout,
    Retries,
}

/// The NNTP form as typed. The password field starts empty, always.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NntpDraft {
    pub enabled: bool,
    pub host: String,
    pub port: String,
    pub username: String,
    /// A new password, or empty to keep (or not set) one.
    pub password: String,
    /// Forget the saved password.
    pub clear_password: bool,
    pub max_connections: String,
    pub connect_timeout_secs: String,
    pub io_timeout_secs: String,
    pub max_retries: String,
}

impl Default for NntpDraft {
    fn default() -> Self {
        Self {
            enabled: true,
            host: String::new(),
            port: NNTP_DEFAULT_PORT.to_string(),
            username: String::new(),
            password: String::new(),
            clear_password: false,
            max_connections: NNTP_DEFAULT_CONNECTIONS.to_string(),
            connect_timeout_secs: NNTP_DEFAULT_CONNECT_TIMEOUT_SECS.to_string(),
            io_timeout_secs: NNTP_DEFAULT_IO_TIMEOUT_SECS.to_string(),
            max_retries: NNTP_DEFAULT_RETRIES.to_string(),
        }
    }
}

/// A problem with the NNTP form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftError {
    pub field: NntpField,
    pub message: &'static str,
}

impl NntpDraft {
    /// The form for saved settings (or the defaults if there are none).
    /// Never carries a password: the API doesn't return one.
    pub fn from_settings(settings: Option<&NntpServerSettings>) -> Self {
        let Some(s) = settings else {
            return Self::default();
        };
        Self {
            enabled: s.enabled,
            host: s.host.clone(),
            port: s.port.to_string(),
            username: s.username.clone().unwrap_or_default(),
            password: String::new(),
            clear_password: false,
            max_connections: s.max_connections.to_string(),
            connect_timeout_secs: s.connect_timeout_secs.to_string(),
            io_timeout_secs: s.io_timeout_secs.to_string(),
            max_retries: s.max_retries.to_string(),
        }
    }

    /// What happens to the password: a typed one replaces it, the clear
    /// box forgets it, and otherwise the saved one is kept.
    pub fn password_update(&self) -> PasswordUpdate {
        if !self.password.is_empty() {
            PasswordUpdate::Set(self.password.clone())
        } else if self.clear_password {
            PasswordUpdate::Clear
        } else {
            PasswordUpdate::Keep
        }
    }

    /// The request body for save and test, after the same range checks the
    /// server makes.
    pub fn to_update(&self) -> Result<NntpServerUpdate, DraftError> {
        let err = |field, message| DraftError { field, message };
        let host = self.host.trim();
        if host.is_empty() || host.len() > 253 {
            return Err(err(
                NntpField::Host,
                "Enter the server's host name, like the one your provider gave you.",
            ));
        }
        if host.contains(['/', ':', ' ']) && host.parse::<std::net::Ipv6Addr>().is_err() {
            return Err(err(
                NntpField::Host,
                "Enter just the host name, without a scheme, port or path.",
            ));
        }
        let port = match self.port.trim().parse::<u16>() {
            Ok(0) | Err(_) => return Err(err(NntpField::Port, "The port must be 1–65535.")),
            Ok(NNTP_PLAINTEXT_PORT) => {
                return Err(err(
                    NntpField::Port,
                    "Port 119 is unencrypted. Reelarr needs a TLS port, usually 563.",
                ));
            }
            Ok(port) => port,
        };
        let number =
            |value: &str, field, min: u32, max: u32, message| match value.trim().parse::<u32>() {
                Ok(n) if (min..=max).contains(&n) => Ok(n),
                _ => Err(err(field, message)),
            };
        let max_connections = number(
            &self.max_connections,
            NntpField::Connections,
            1,
            u32::from(NNTP_MAX_CONNECTIONS),
            "Connections must be 1–100.",
        )? as u16;
        let connect_timeout_secs = number(
            &self.connect_timeout_secs,
            NntpField::ConnectTimeout,
            1,
            NNTP_MAX_TIMEOUT_SECS,
            "The connect timeout must be 1–300 seconds.",
        )?;
        let io_timeout_secs = number(
            &self.io_timeout_secs,
            NntpField::IoTimeout,
            1,
            NNTP_MAX_TIMEOUT_SECS,
            "The read timeout must be 1–300 seconds.",
        )?;
        let max_retries = number(
            &self.max_retries,
            NntpField::Retries,
            0,
            u32::from(NNTP_MAX_RETRIES),
            "Retries must be 0–10.",
        )? as u8;
        let printable =
            |s: &str| (1..=496).contains(&s.len()) && s.chars().all(|c| c.is_ascii_graphic());
        let username = self.username.trim();
        if !username.is_empty() && !printable(username) {
            return Err(err(
                NntpField::Username,
                "The username must be printable characters without spaces.",
            ));
        }
        if !self.password.is_empty() && !printable(&self.password) {
            return Err(err(
                NntpField::Password,
                "The password must be printable characters without spaces.",
            ));
        }
        if username.is_empty() && !self.password.is_empty() {
            return Err(err(
                NntpField::Username,
                "A password needs a username. Enter both, or neither to connect without signing in.",
            ));
        }
        Ok(NntpServerUpdate {
            enabled: self.enabled,
            host: host.to_string(),
            port,
            username: (!username.is_empty()).then(|| username.to_string()),
            password: self.password_update(),
            max_connections,
            connect_timeout_secs,
            io_timeout_secs,
            max_retries,
        })
    }
}

/// Words about the saved password. It is never shown.
pub fn password_status(saved: Option<&NntpServerSettings>) -> &'static str {
    match saved {
        Some(s) if s.has_password => {
            "A password is saved. It's never shown here. Leave this blank to keep it."
        }
        Some(_) => "No password is saved.",
        None => "Only needed if your provider requires a sign-in.",
    }
}

/// The test result in plain words: `(ok, title, detail)`. The detail is
/// the server's own message, which is static and never quotes settings.
pub fn test_result(result: &NntpTestResult) -> (bool, &'static str, String) {
    let title = match result.outcome {
        NntpTestOutcome::Connected => "Connected",
        NntpTestOutcome::TlsRejected => "Secure connection failed",
        NntpTestOutcome::AuthRejected => "Sign-in rejected",
        NntpTestOutcome::Timeout => "Timed out",
        NntpTestOutcome::Unreachable => "Couldn't reach the server",
        NntpTestOutcome::ServerUnavailable => "Server unavailable",
        NntpTestOutcome::ProtocolError => "Not a Usenet server",
        NntpTestOutcome::TrustRootsUnavailable => "No trusted certificates",
    };
    (result.ok, title, crate::staff::sentence(&result.message))
}

/// The status line over the NNTP form.
pub fn nntp_summary(saved: Option<&NntpServerSettings>) -> String {
    match saved {
        None => "No Usenet server is set up yet. Downloads can't start until one is.".into(),
        Some(s) if !s.enabled => format!("{} is saved but switched off.", s.host),
        Some(s) => format!(
            "Downloading through {}:{} with up to {} connection{}.",
            s.host,
            s.port,
            s.max_connections,
            if s.max_connections == 1 { "" } else { "s" }
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reelarr_types::{LibraryRoot, LibraryRootId, RootHealth};

    fn info(files: u64, health: RootHealth) -> LibraryRootInfo {
        LibraryRootInfo {
            root: LibraryRoot {
                id: LibraryRootId(3),
                path: "/srv/folder-a".into(),
                kind: LibraryRootKind::ManualImport,
                rescan_interval_secs: Some(DEFAULT_RESCAN_INTERVAL_SECS),
                is_default: false,
            },
            health,
            health_checked_at: Some(0),
            file_count: files,
        }
    }

    fn saved(has_password: bool) -> NntpServerSettings {
        NntpServerSettings {
            enabled: true,
            host: "news.invalid".into(),
            port: 563,
            username: Some("member".into()),
            has_password,
            max_connections: 8,
            connect_timeout_secs: 15,
            io_timeout_secs: 15,
            max_retries: 2,
        }
    }

    #[test]
    fn root_paths() {
        assert_eq!(root_check_path(3), "/api/v1/library/roots/3/check");
        assert_eq!(root_remove_path(3, false), "/api/v1/library/roots/3");
        assert_eq!(
            root_remove_path(3, true),
            "/api/v1/library/roots/3?forget_files=true"
        );
    }

    #[test]
    fn root_rows_say_everything_in_words() {
        let row = RootRow::of(&info(
            2,
            RootHealth::Offline {
                since: 1_700_000_000,
                reason: "mount point is empty".into(),
            },
        ));
        assert_eq!(row.kind, "Manual import");
        assert_eq!(row.rescan, "Rescans every 6 hours");
        assert_eq!(row.health.label(), "Offline");
        assert!(row.health.problem().unwrap().contains("2023-11-14"));
        assert_eq!(row.files_label(), "2 files on record");
        assert_eq!(row.checked, "Checked 1970-01-01 00:00 UTC");
        assert!(row.forget_files());
        let q = row.remove_question(&MainFolder::NoLibrary);
        assert!(q.contains("forgets the 2 files"));
        assert!(q.contains("not touched"));

        let empty = RootRow::of(&info(0, RootHealth::Unknown));
        assert!(!empty.forget_files());
        assert_eq!(empty.health.label(), "Not checked yet");
        assert!(
            !empty
                .remove_question(&MainFolder::NoLibrary)
                .contains("forgets")
        );
        let mut never = info(1, RootHealth::Online);
        never.health_checked_at = None;
        never.root.rescan_interval_secs = None;
        let never = RootRow::of(&never);
        assert_eq!(never.checked, "Never checked");
        assert_eq!(never.rescan, "No scheduled rescans");
        assert_eq!(never.files_label(), "1 file on record");
    }

    fn root(id: i64, kind: LibraryRootKind, is_default: bool) -> LibraryRootInfo {
        let mut i = info(0, RootHealth::Online);
        i.root.id = LibraryRootId(id);
        i.root.path = format!("/srv/folder-{id}");
        i.root.kind = kind;
        i.root.is_default = is_default;
        i
    }

    #[test]
    fn main_folder_prompt_logic() {
        use LibraryRootKind::{Library, ManualImport};
        assert_eq!(MainFolder::of(&[]), MainFolder::NoLibrary);
        // Manual-import folders never count.
        assert_eq!(
            MainFolder::of(&[root(1, ManualImport, false)]),
            MainFolder::NoLibrary
        );
        // One library folder: main without asking, even beside imports.
        let one = [root(1, Library, false), root(2, ManualImport, false)];
        assert_eq!(MainFolder::of(&one), MainFolder::Implicit(1));
        let main = MainFolder::of(&one);
        assert_eq!(main.id(), Some(1));
        let rows: Vec<_> = one.iter().map(RootRow::of).collect();
        assert!(!main.can_pick(&rows[0]));
        assert!(!main.can_pick(&rows[1]));
        assert_eq!(main.tag(&rows[0]), Some("Main library"));
        assert_eq!(main.tag(&rows[1]), None);
        assert!(
            rows[0]
                .remove_question(&main)
                .contains("only library folder")
        );
        // Two library folders, no pick: ask, on library rows only.
        let many = [
            root(1, Library, false),
            root(2, Library, false),
            root(3, ManualImport, false),
        ];
        let main = MainFolder::of(&many);
        assert_eq!(main, MainFolder::NeedsPick);
        assert_eq!(main.id(), None);
        let rows: Vec<_> = many.iter().map(RootRow::of).collect();
        assert!(main.can_pick(&rows[0]) && main.can_pick(&rows[1]));
        assert!(!main.can_pick(&rows[2]));
        assert!(rows.iter().all(|r| main.tag(r).is_none()));
        let prompt = pick_main_message(&many);
        assert!(prompt.contains("2 library folders"));
        assert!(prompt.contains("stay ready for import"));
        // Picked: marked in words, the others can still be picked.
        let chosen = [root(1, Library, false), root(2, Library, true)];
        let main = MainFolder::of(&chosen);
        assert_eq!(main, MainFolder::Chosen(2));
        let rows: Vec<_> = chosen.iter().map(RootRow::of).collect();
        assert!(main.can_pick(&rows[0]));
        assert!(!main.can_pick(&rows[1]));
        assert_eq!(main.tag(&rows[1]), Some("Main library folder"));
        assert!(rows[1].is_default);
        let q = rows[1].remove_question(&main);
        assert!(q.contains("main library folder"));
        assert!(q.contains("wait until you pick another"));
        assert!(!rows[0].remove_question(&main).contains("main library"));
        // A stray default flag on a manual-import folder is ignored.
        let stray = [root(1, ManualImport, true), root(2, Library, false)];
        assert_eq!(MainFolder::of(&stray), MainFolder::Implicit(2));
        assert!(!RootRow::of(&stray[0]).is_default);
        assert_eq!(root_default_path(4), "/api/v1/library/roots/4/default");
        // The Back Room only speaks up when imports have nowhere to go.
        assert_eq!(
            import_wait(MainFolder::NeedsPick).unwrap().1,
            PICK_MAIN_WAITING
        );
        assert!(
            import_wait(MainFolder::NoLibrary)
                .unwrap()
                .1
                .contains("add one")
        );
        assert_eq!(import_wait(MainFolder::Implicit(1)), None);
        assert_eq!(import_wait(MainFolder::Chosen(1)), None);
    }

    #[test]
    fn folder_errors_in_plain_words() {
        let fail = |status, code: &str, message: &str| SendFailure {
            failure: crate::api::ApiFailure::Status {
                status,
                code: Some(code.into()),
            },
            message: Some(message.into()),
        };
        assert_eq!(
            root_error(&fail(
                400,
                "invalid_path",
                "/srv/x.mkv exists but is not a folder"
            )),
            "That path is a file, not a folder."
        );
        assert!(
            root_error(&fail(
                400,
                "invalid_path",
                "library folder must be an absolute path, got \"x\""
            ))
            .contains("starting with /")
        );
        assert!(root_error(&fail(400, "default_not_library", "x")).contains("manual-import"));
        assert!(
            root_error(&fail(404, "not_found", "library folder not found")).contains("anymore")
        );
        assert_eq!(
            root_error(&fail(
                409,
                "duplicate_root",
                "/srv/a is already a library folder"
            )),
            "/srv/a is already a library folder."
        );
        assert!(!root_error(&fail(500, "internal", "db")).contains("db"));
    }

    #[test]
    fn rescan_words() {
        assert_eq!(rescan_label(Some(3600)), "Rescans every hour");
        assert_eq!(rescan_label(Some(86_400)), "Rescans every day");
        assert_eq!(rescan_label(Some(7 * 86_400)), "Rescans every 7 days");
        assert_eq!(rescan_label(Some(90)), "Rescans every 90 seconds");
        assert_eq!(rescan_label(Some(120)), "Rescans every 2 minutes");
        assert_eq!(RESCAN_CHOICES[0].0, DEFAULT_RESCAN_INTERVAL_SECS);
    }

    #[test]
    fn root_form_checks_the_path() {
        assert_eq!(validate_root_path("  /srv/a  ").unwrap(), "/srv/a");
        assert!(validate_root_path("").is_err());
        assert!(validate_root_path("srv/a").is_err());
        assert!(validate_root_path("/srv/../etc").is_err());
        assert!(validate_root_path("/srv/./a").is_err());
        let new = new_root("/srv/a", "library", "3600", true).unwrap();
        assert_eq!(new.kind, LibraryRootKind::Library);
        assert_eq!(new.rescan_interval_secs, Some(3600));
        assert!(new.is_default);
        // Never asks the server to make a manual-import folder main.
        assert!(
            !new_root("/srv/a", "manual_import", "3600", true)
                .unwrap()
                .is_default
        );
        assert!(new_root("/srv/a", "shelf", "3600", false).is_err());
        assert!(new_root("/srv/a", "library", "5", false).is_err());
        assert_eq!(
            kind_from_value("manual_import"),
            Some(LibraryRootKind::ManualImport)
        );
    }

    #[test]
    fn saved_settings_never_fill_the_password() {
        let draft = NntpDraft::from_settings(Some(&saved(true)));
        assert_eq!(draft.password, "");
        assert_eq!(draft.username, "member");
        assert_eq!(draft.host, "news.invalid");
        assert_eq!(draft.password_update(), PasswordUpdate::Keep);
        // The update's Debug never prints a typed password either.
        let mut typed = draft.clone();
        typed.password = "hunter2-secret".into();
        let update = typed.to_update().unwrap();
        assert_eq!(
            update.password,
            PasswordUpdate::Set("hunter2-secret".into())
        );
        assert!(!format!("{update:?}").contains("hunter2"));
        let mut clear = draft;
        clear.clear_password = true;
        assert_eq!(clear.password_update(), PasswordUpdate::Clear);
        // A typed password wins over the clear box.
        clear.password = "new".into();
        assert_eq!(clear.password_update(), PasswordUpdate::Set("new".into()));
    }

    #[test]
    fn defaults_match_the_api() {
        let update = NntpDraft {
            host: "news.invalid".into(),
            ..NntpDraft::default()
        }
        .to_update()
        .unwrap();
        assert_eq!(update, NntpServerUpdate::new("news.invalid"));
    }

    #[test]
    fn nntp_form_checks_ranges() {
        let ok = NntpDraft {
            host: "news.invalid".into(),
            ..NntpDraft::default()
        };
        let field = |draft: NntpDraft| draft.to_update().unwrap_err().field;
        assert_eq!(
            field(NntpDraft {
                host: " ".into(),
                ..ok.clone()
            }),
            NntpField::Host
        );
        assert_eq!(
            field(NntpDraft {
                host: "nntps://news.invalid".into(),
                ..ok.clone()
            }),
            NntpField::Host
        );
        assert!(
            NntpDraft {
                host: "::1".into(),
                ..ok.clone()
            }
            .to_update()
            .is_ok()
        );
        for port in ["0", "70000", "x", "119"] {
            assert_eq!(
                field(NntpDraft {
                    port: port.into(),
                    ..ok.clone()
                }),
                NntpField::Port,
                "{port}"
            );
        }
        assert_eq!(
            field(NntpDraft {
                max_connections: "101".into(),
                ..ok.clone()
            }),
            NntpField::Connections
        );
        assert_eq!(
            field(NntpDraft {
                connect_timeout_secs: "0".into(),
                ..ok.clone()
            }),
            NntpField::ConnectTimeout
        );
        assert_eq!(
            field(NntpDraft {
                io_timeout_secs: "301".into(),
                ..ok.clone()
            }),
            NntpField::IoTimeout
        );
        assert_eq!(
            field(NntpDraft {
                max_retries: "11".into(),
                ..ok.clone()
            }),
            NntpField::Retries
        );
        assert_eq!(
            field(NntpDraft {
                password: "p".into(),
                ..ok.clone()
            }),
            NntpField::Username
        );
        assert_eq!(
            field(NntpDraft {
                username: "has space".into(),
                ..ok.clone()
            }),
            NntpField::Username
        );
        assert_eq!(
            field(NntpDraft {
                username: "u".into(),
                password: "has space".into(),
                ..ok.clone()
            }),
            NntpField::Password
        );
        let blank_user = NntpDraft {
            username: "  ".into(),
            ..ok
        }
        .to_update()
        .unwrap();
        assert_eq!(blank_user.username, None);
    }

    #[test]
    fn password_words_never_include_it() {
        assert!(password_status(Some(&saved(true))).contains("never shown"));
        assert_eq!(
            password_status(Some(&saved(false))),
            "No password is saved."
        );
        assert!(password_status(None).contains("Only needed"));
    }

    #[test]
    fn test_results_in_plain_words() {
        let (ok, title, detail) = test_result(&NntpTestResult {
            ok: false,
            outcome: NntpTestOutcome::AuthRejected,
            message: "The server rejected the username or password".into(),
        });
        assert!(!ok);
        assert_eq!(title, "Sign-in rejected");
        assert_eq!(detail, "The server rejected the username or password.");
        let (ok, title, _) = test_result(&NntpTestResult {
            ok: true,
            outcome: NntpTestOutcome::Connected,
            message: "Connected and signed in".into(),
        });
        assert!(ok);
        assert_eq!(title, "Connected");
    }

    #[test]
    fn nntp_summary_lines() {
        assert!(nntp_summary(None).contains("No Usenet server"));
        assert_eq!(
            nntp_summary(Some(&saved(true))),
            "Downloading through news.invalid:563 with up to 8 connections."
        );
        let mut off = saved(false);
        off.enabled = false;
        assert!(nntp_summary(Some(&off)).contains("switched off"));
    }
}
