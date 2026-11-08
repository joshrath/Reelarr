//! Destination naming for imported movies.

use reelarr_types::{MediaProbe, StreamKind};

/// Default folder and file template. `/` separates the folder from the file
/// name; the extension is appended automatically.
pub const DEFAULT_TEMPLATE: &str = "{Title} ({Year})/{Title} ({Year}) [{Quality}]";

/// Values available to a rename template.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NamingValues {
    pub title: String,
    pub year: Option<u16>,
    /// Quality label such as `1080p`.
    pub quality: Option<String>,
    pub tmdb_id: Option<u64>,
    pub imdb_id: Option<String>,
}

/// Render a template into relative path components (folder parts, then the
/// file stem). Supported tokens: `{Title}`, `{Year}`, `{Quality}`,
/// `{TmdbId}`, `{ImdbId}`. Unknown values are left empty, and the brackets
/// around an empty token are removed. Each component is sanitized for
/// Windows and SMB.
pub fn render(template: &str, values: &NamingValues) -> Vec<String> {
    let year = values.year.map(|y| y.to_string()).unwrap_or_default();
    let tmdb = values.tmdb_id.map(|t| t.to_string()).unwrap_or_default();
    template
        .split('/')
        .map(|part| {
            let filled = part
                .replace("{Title}", &values.title)
                .replace("{Year}", &year)
                .replace("{Quality}", values.quality.as_deref().unwrap_or(""))
                .replace("{TmdbId}", &tmdb)
                .replace("{ImdbId}", values.imdb_id.as_deref().unwrap_or(""));
            sanitize_component(&tidy(&filled))
        })
        .filter(|c| !c.is_empty())
        .collect()
}

/// Remove empty `()`, `[]` and `{}` pairs and collapse whitespace.
fn tidy(value: &str) -> String {
    let mut s = value.to_owned();
    loop {
        let before = s.len();
        for empty in ["()", "[]", "{}", "( )", "[ ]"] {
            s = s.replace(empty, "");
        }
        if s.len() == before {
            break;
        }
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Make one path component safe on Linux, Windows and SMB shares.
pub fn sanitize_component(value: &str) -> String {
    let mut out: String = value
        .chars()
        .map(|c| match c {
            ':' => '-',
            '<' | '>' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    // Windows drops trailing dots and spaces.
    while out.ends_with(['.', ' ']) {
        out.pop();
    }
    let out = out.trim_start().to_owned();
    let stem = out.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    if reserved { format!("_{out}") } else { out }
}

/// A resolution label from probe data, e.g. `2160p`, `1080p`, `720p`, `SD`.
pub fn quality_label(probe: &MediaProbe) -> Option<String> {
    let video = probe.streams.iter().find(|s| s.kind == StreamKind::Video)?;
    let (w, h) = (video.width.unwrap_or(0), video.height.unwrap_or(0));
    if w == 0 && h == 0 {
        return None;
    }
    // Width handles scope (letterboxed) encodes with reduced height.
    Some(
        if w >= 3200 || h >= 1800 {
            "2160p"
        } else if w >= 1700 || h >= 1000 {
            "1080p"
        } else if w >= 1200 || h >= 700 {
            "720p"
        } else {
            "SD"
        }
        .to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use reelarr_types::MediaStream;

    fn values() -> NamingValues {
        NamingValues {
            title: "Movie: The Sequel?".into(),
            year: Some(2004),
            quality: Some("1080p".into()),
            tmdb_id: Some(7),
            imdb_id: None,
        }
    }

    #[test]
    fn renders_default_template() {
        assert_eq!(
            render(DEFAULT_TEMPLATE, &values()),
            vec![
                "Movie- The Sequel_ (2004)",
                "Movie- The Sequel_ (2004) [1080p]"
            ]
        );
    }

    #[test]
    fn drops_empty_tokens() {
        let v = NamingValues {
            title: "Plain".into(),
            ..NamingValues::default()
        };
        assert_eq!(render(DEFAULT_TEMPLATE, &v), vec!["Plain", "Plain"]);
        assert_eq!(
            render("{Title} {ImdbId}/{Title} [{TmdbId}]", &v),
            vec!["Plain", "Plain"]
        );
    }

    #[test]
    fn sanitizes_reserved_and_trailing() {
        assert_eq!(sanitize_component("CON"), "_CON");
        assert_eq!(sanitize_component("com1.txt"), "_com1.txt");
        assert_eq!(sanitize_component("Console"), "Console");
        assert_eq!(sanitize_component("End. "), "End");
        assert_eq!(sanitize_component("a\u{0}b"), "a_b");
        assert_eq!(sanitize_component(".."), "");
    }

    #[test]
    fn quality_from_probe() {
        let probe = |w, h| MediaProbe {
            streams: vec![MediaStream {
                index: 0,
                kind: StreamKind::Video,
                codec: None,
                profile: None,
                width: Some(w),
                height: Some(h),
                channels: None,
                language: None,
                default: true,
                forced: false,
            }],
            ..MediaProbe::default()
        };
        assert_eq!(quality_label(&probe(3840, 1600)).as_deref(), Some("2160p"));
        assert_eq!(quality_label(&probe(1920, 800)).as_deref(), Some("1080p"));
        assert_eq!(quality_label(&probe(1280, 720)).as_deref(), Some("720p"));
        assert_eq!(quality_label(&probe(720, 480)).as_deref(), Some("SD"));
        assert_eq!(quality_label(&MediaProbe::default()), None);
    }
}
