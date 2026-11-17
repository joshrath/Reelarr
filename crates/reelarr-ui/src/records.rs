//! What The Record Corner says. Music libraries aren't supported yet, so
//! the page is an honest empty crate: no albums, covers, artists, labels or
//! format badges, and no API calls. Plain data, so it can be tested
//! without a browser.

/// The library folders panel on Employees Only, where folders are added.
pub const LIBRARY_FOLDERS_PATH: &str = "/employees-only#roots-title";

pub const TITLE: &str = "The Record Corner";
/// The themed line under the page title.
pub const SUBTITLE: &str = "Hi-Fi Audio & Vinyl Vault";
pub const OVERLINE: &str = "Music aisle";
pub const LEDE: &str = "Where your music will be racked up next to the movies: albums and tracks from your own folders.";

/// The status chip in the header.
pub const STATUS_LABEL: &str = "Music library";
pub const STATUS_VALUE: &str = "Not supported yet";

/// The empty crate.
pub const EMPTY_TITLE: &str = "The crates are empty";
pub const EMPTY_MESSAGE: &str = "Music libraries aren't supported yet, so there's nothing to flip through here. Reelarr's library folders hold movies for now.";
pub const EMPTY_LINK_LABEL: &str = "Library folders in Employees Only";

/// The plain note under the crate.
pub const NOTE: &str = "The Record Corner fills in once music library support lands.";

/// Every line of copy on the page, for checks.
pub const ALL_COPY: &[&str] = &[
    TITLE,
    SUBTITLE,
    OVERLINE,
    LEDE,
    STATUS_LABEL,
    STATUS_VALUE,
    EMPTY_TITLE,
    EMPTY_MESSAGE,
    EMPTY_LINK_LABEL,
    NOTE,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn says_music_is_not_supported_and_points_to_library_folders() {
        assert!(EMPTY_MESSAGE.contains("Music libraries aren't supported yet"));
        assert!(NOTE.contains("once music library support lands"));
        assert_eq!(LIBRARY_FOLDERS_PATH, "/employees-only#roots-title");
        assert!(EMPTY_LINK_LABEL.contains("Employees Only"));
    }

    #[test]
    fn no_sample_records_or_made_up_details() {
        // Bits of the design's sample crate: catalogue numbers, formats,
        // pressing badges and the tape deck and EQ controls.
        for made_up in [
            "RPM",
            "FLAC",
            "DSD",
            "24/",
            "180g",
            "PRESS",
            "Records",
            "ARCHIVE TAPE",
            "#0941",
            "LOAD CASSETTE",
            "STAFF CURATED",
            "EQ",
            "CrO2",
            "METAL",
            "BYPASS",
            "Hz",
        ] {
            for line in ALL_COPY {
                assert!(
                    !line.to_lowercase().contains(&made_up.to_lowercase()),
                    "{made_up:?} in {line:?}"
                );
            }
        }
        // Nothing is linked to an issue in the copy.
        assert!(ALL_COPY.iter().all(|l| !l.contains('#')));
    }
}
