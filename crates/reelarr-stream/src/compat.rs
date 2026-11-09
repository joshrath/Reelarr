//! Can this browser play this file directly?
//!
//! The rules follow what each browser engine documents for `<video>`. They
//! are deliberately conservative: anything not listed is reported as
//! unsupported, with a reason the player can show, rather than failing
//! mid-playback. HEVC is only offered to Safari (WebKit).

use reelarr_types::{MediaProbe, MediaStream, StreamKind, UnsupportedKind, UnsupportedReason};

/// Browser engine family, from the `User-Agent` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Browser {
    /// Safari and every iOS/iPadOS browser (all use WebKit).
    Safari,
    /// Chrome, Edge, Opera, Brave and other Chromium browsers.
    Chromium,
    Firefox,
    /// Unknown clients only get the most widely supported formats.
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientProfile {
    pub browser: Browser,
}

impl ClientProfile {
    pub fn from_user_agent(ua: &str) -> Self {
        let has = |s: &str| ua.contains(s);
        let browser = if has("iPhone") || has("iPad") || has("iPod") {
            Browser::Safari
        } else if has("Firefox/") {
            Browser::Firefox
        } else if has("Chrome/") || has("Chromium/") || has("Edg/") {
            Browser::Chromium
        } else if has("Safari/") && has("Version/") {
            Browser::Safari
        } else {
            Browser::Other
        };
        Self { browser }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Container {
    Mp4,
    WebM,
}

impl Container {
    fn mime(self) -> &'static str {
        match self {
            Container::Mp4 => "video/mp4",
            Container::WebM => "video/webm",
        }
    }
}

/// Check direct play. Returns the MIME type for the `<video>` source, or
/// every reason it cannot be played.
///
/// `extension` is needed because ffprobe reports `matroska,webm` for both
/// Matroska and WebM files.
pub fn check(
    probe: Option<&MediaProbe>,
    extension: Option<&str>,
    client: ClientProfile,
) -> Result<&'static str, Vec<UnsupportedReason>> {
    let Some(probe) = probe else {
        return Err(vec![reason(
            UnsupportedKind::NotProbed,
            None,
            "This file has not been analysed yet, so playback support is unknown. Try again after the next library scan.".into(),
        )]);
    };
    let ext = extension.map(str::to_ascii_lowercase);
    let mut reasons = Vec::new();

    let container = container(probe, ext.as_deref());
    if container.is_none() {
        let name = probe.container.join(",");
        reasons.push(reason(
            UnsupportedKind::Container,
            (!name.is_empty()).then(|| name.clone()),
            format!(
                "Browsers cannot play the {} container directly. Playing it will need remuxing, which isn't available yet.",
                container_label(&probe.container, ext.as_deref())
            ),
        ));
    }

    let video = probe
        .streams
        .iter()
        .find(|s| s.kind == StreamKind::Video && !is_cover_art(s));
    if let Some(v) = video
        && let Some(message) = video_problem(v, container, client.browser)
    {
        reasons.push(reason(
            UnsupportedKind::VideoCodec,
            v.codec.clone(),
            message,
        ));
    }

    let audio = probe
        .streams
        .iter()
        .filter(|s| s.kind == StreamKind::Audio)
        .find(|s| s.default)
        .or_else(|| probe.streams.iter().find(|s| s.kind == StreamKind::Audio));
    if let Some(a) = audio
        && let Some(message) = audio_problem(a, container, client.browser)
    {
        reasons.push(reason(
            UnsupportedKind::AudioCodec,
            a.codec.clone(),
            message,
        ));
    }

    match (container, reasons.is_empty()) {
        (Some(c), true) => Ok(c.mime()),
        _ => Err(reasons),
    }
}

fn container(probe: &MediaProbe, ext: Option<&str>) -> Option<Container> {
    let has = |n: &str| probe.container.iter().any(|c| c == n);
    if has("mp4") || has("mov") {
        // ffprobe's "mov,mp4,m4a,3gp,3g2,mj2"; a QuickTime .mov is fine too.
        Some(Container::Mp4)
    } else if has("webm") && ext == Some("webm") {
        Some(Container::WebM)
    } else {
        None
    }
}

fn container_label(names: &[String], ext: Option<&str>) -> String {
    if names.iter().any(|n| n == "matroska") {
        "MKV (Matroska)".into()
    } else if names.iter().any(|n| n == "avi") {
        "AVI".into()
    } else if names.iter().any(|n| n == "mpegts") {
        "MPEG-TS".into()
    } else if let Some(ext) = ext {
        ext.to_ascii_uppercase()
    } else {
        "this".into()
    }
}

fn is_cover_art(s: &MediaStream) -> bool {
    matches!(s.codec.as_deref(), Some("mjpeg" | "png" | "bmp" | "gif"))
}

fn video_problem(v: &MediaStream, c: Option<Container>, b: Browser) -> Option<String> {
    use Browser::*;
    let codec = v.codec.as_deref().unwrap_or("unknown");
    let ok = match codec {
        "h264" => {
            let profile = v.profile.as_deref().unwrap_or("").to_ascii_lowercase();
            if profile.contains("10") || profile.contains("4:2:2") || profile.contains("4:4:4") {
                return Some(format!(
                    "This video is H.264 {}, which browsers can't decode. It will need transcoding, which isn't available yet.",
                    v.profile.as_deref().unwrap_or("")
                ));
            }
            c != Some(Container::WebM)
        }
        "hevc" => b == Safari && c == Some(Container::Mp4),
        "av1" => matches!(b, Chromium | Firefox),
        "vp9" => matches!(b, Chromium | Firefox),
        "vp8" => matches!(b, Chromium | Firefox) && c == Some(Container::WebM),
        _ => false,
    };
    (!ok).then(|| {
        let name = codec_label(codec);
        if codec == "hevc" {
            format!(
                "This video is {name}, which only Safari can play directly. Other browsers will need transcoding, which isn't available yet."
            )
        } else {
            format!(
                "This browser can't play {name} video directly. It will need transcoding, which isn't available yet."
            )
        }
    })
}

fn audio_problem(a: &MediaStream, c: Option<Container>, b: Browser) -> Option<String> {
    use Browser::*;
    let codec = a.codec.as_deref().unwrap_or("unknown");
    let ok = match codec {
        "aac" | "mp3" => c != Some(Container::WebM),
        "opus" => matches!(b, Chromium | Firefox),
        "vorbis" => matches!(b, Chromium | Firefox) && c == Some(Container::WebM),
        "flac" => matches!(b, Chromium | Firefox | Safari) && c == Some(Container::Mp4),
        "ac3" | "eac3" => b == Safari && c == Some(Container::Mp4),
        _ => false,
    };
    (!ok).then(|| {
        format!(
            "This browser can't play {} audio directly. It will need transcoding, which isn't available yet.",
            codec_label(codec)
        )
    })
}

fn codec_label(codec: &str) -> String {
    match codec {
        "h264" => "H.264".into(),
        "hevc" => "HEVC (H.265)".into(),
        "av1" => "AV1".into(),
        "vp9" => "VP9".into(),
        "vp8" => "VP8".into(),
        "mpeg4" => "MPEG-4 Part 2 (DivX/Xvid)".into(),
        "aac" => "AAC".into(),
        "mp3" => "MP3".into(),
        "ac3" => "Dolby Digital (AC-3)".into(),
        "eac3" => "Dolby Digital Plus (E-AC-3)".into(),
        "dts" => "DTS".into(),
        "truehd" => "Dolby TrueHD".into(),
        "flac" => "FLAC".into(),
        "opus" => "Opus".into(),
        "vorbis" => "Vorbis".into(),
        other => other.to_ascii_uppercase(),
    }
}

fn reason(kind: UnsupportedKind, name: Option<String>, message: String) -> UnsupportedReason {
    UnsupportedReason {
        kind,
        name,
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAFARI: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_5) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Safari/605.1.15";
    const CHROME: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";
    const EDGE: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36 Edg/130.0.0.0";
    const FIREFOX: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:132.0) Gecko/20100101 Firefox/132.0";
    const IOS_CHROME: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/130.0 Mobile/15E148 Safari/604.1";

    fn client(ua: &str) -> ClientProfile {
        ClientProfile::from_user_agent(ua)
    }

    fn stream(kind: StreamKind, codec: &str) -> MediaStream {
        MediaStream {
            index: 0,
            kind,
            codec: Some(codec.into()),
            profile: None,
            width: None,
            height: None,
            channels: None,
            language: None,
            default: true,
            forced: false,
        }
    }

    fn probe(container: &str, video: &str, audio: &str) -> MediaProbe {
        MediaProbe {
            container: container.split(',').map(str::to_owned).collect(),
            duration_ms: Some(1),
            bit_rate: None,
            streams: vec![
                stream(StreamKind::Video, video),
                stream(StreamKind::Audio, audio),
            ],
        }
    }

    const MP4: &str = "mov,mp4,m4a,3gp,3g2,mj2";
    const MKV: &str = "matroska,webm";

    #[test]
    fn detects_browsers() {
        assert_eq!(client(SAFARI).browser, Browser::Safari);
        assert_eq!(client(CHROME).browser, Browser::Chromium);
        assert_eq!(client(EDGE).browser, Browser::Chromium);
        assert_eq!(client(FIREFOX).browser, Browser::Firefox);
        assert_eq!(client(IOS_CHROME).browser, Browser::Safari);
        assert_eq!(client("curl/8.0").browser, Browser::Other);
    }

    #[test]
    fn h264_aac_mp4_plays_everywhere() {
        let p = probe(MP4, "h264", "aac");
        for ua in [SAFARI, CHROME, FIREFOX, "curl/8"] {
            assert_eq!(check(Some(&p), Some("mp4"), client(ua)), Ok("video/mp4"));
        }
    }

    #[test]
    fn hevc_only_on_safari() {
        let p = probe(MP4, "hevc", "aac");
        assert_eq!(
            check(Some(&p), Some("mp4"), client(SAFARI)),
            Ok("video/mp4")
        );
        let err = check(Some(&p), Some("mp4"), client(CHROME)).unwrap_err();
        assert_eq!(err.len(), 1);
        assert_eq!(err[0].kind, UnsupportedKind::VideoCodec);
        assert!(err[0].message.contains("only Safari"));
    }

    #[test]
    fn matroska_is_not_direct_play_but_webm_is() {
        let p = probe(MKV, "h264", "aac");
        let err = check(Some(&p), Some("mkv"), client(CHROME)).unwrap_err();
        assert_eq!(err[0].kind, UnsupportedKind::Container);
        assert!(err[0].message.contains("MKV"));

        let webm = probe(MKV, "vp9", "opus");
        assert_eq!(
            check(Some(&webm), Some("webm"), client(FIREFOX)),
            Ok("video/webm")
        );
        assert!(check(Some(&webm), Some("webm"), client(SAFARI)).is_err());
    }

    #[test]
    fn reports_every_reason() {
        let p = probe("avi", "mpeg4", "dts");
        let err = check(Some(&p), Some("avi"), client(CHROME)).unwrap_err();
        let kinds: Vec<_> = err.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            [
                UnsupportedKind::Container,
                UnsupportedKind::VideoCodec,
                UnsupportedKind::AudioCodec
            ]
        );
        assert!(err[2].message.contains("DTS"));
    }

    #[test]
    fn dolby_audio_only_on_safari_and_10bit_h264_nowhere() {
        let p = probe(MP4, "h264", "eac3");
        assert!(check(Some(&p), Some("mp4"), client(SAFARI)).is_ok());
        assert!(check(Some(&p), Some("mp4"), client(FIREFOX)).is_err());

        let mut p = probe(MP4, "h264", "aac");
        p.streams[0].profile = Some("High 10".into());
        assert!(check(Some(&p), Some("mp4"), client(SAFARI)).is_err());
    }

    #[test]
    fn uses_default_audio_and_skips_cover_art() {
        let mut p = probe(MP4, "h264", "dts");
        p.streams[1].default = false;
        let mut aac = stream(StreamKind::Audio, "aac");
        aac.default = true;
        p.streams.push(aac);
        p.streams.insert(0, stream(StreamKind::Video, "mjpeg"));
        assert!(check(Some(&p), Some("mp4"), client(CHROME)).is_ok());
    }

    #[test]
    fn unprobed_files_are_reported() {
        let err = check(None, Some("mp4"), client(CHROME)).unwrap_err();
        assert_eq!(err[0].kind, UnsupportedKind::NotProbed);
    }
}
