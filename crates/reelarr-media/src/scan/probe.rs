//! Reading media details with ffprobe.
//!
//! ffprobe always runs as a separate process and is never linked.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use reelarr_types::{MediaProbe, MediaStream, StreamKind};
use serde::Deserialize;
use tokio::process::Command;

/// Errors from probing a file.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("could not start ffprobe ({binary}): {source}")]
    Spawn {
        binary: String,
        source: std::io::Error,
    },
    #[error("ffprobe timed out after {0:?}")]
    Timeout(Duration),
    #[error("ffprobe failed: {0}")]
    Failed(String),
    #[error("could not parse ffprobe output: {0}")]
    Parse(String),
}

/// Something that can read technical details of a media file.
pub trait Prober: Send + Sync {
    fn probe(&self, path: &Path) -> impl Future<Output = Result<MediaProbe, ProbeError>> + Send;
}

/// [`Prober`] that runs the `ffprobe` binary.
#[derive(Debug, Clone)]
pub struct FfprobeProber {
    pub binary: PathBuf,
    pub timeout: Duration,
}

impl Default for FfprobeProber {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("ffprobe"),
            timeout: Duration::from_secs(60),
        }
    }
}

impl Prober for FfprobeProber {
    async fn probe(&self, path: &Path) -> Result<MediaProbe, ProbeError> {
        let child = Command::new(&self.binary)
            .args([
                "-v",
                "error",
                "-print_format",
                "json",
                "-show_format",
                "-show_streams",
            ])
            .arg("--")
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| ProbeError::Spawn {
                binary: self.binary.display().to_string(),
                source,
            })?;

        let output = tokio::time::timeout(self.timeout, child.wait_with_output())
            .await
            .map_err(|_| ProbeError::Timeout(self.timeout))?
            .map_err(|e| ProbeError::Failed(e.to_string()))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let message = stderr.trim();
            return Err(ProbeError::Failed(if message.is_empty() {
                format!("exit status {}", output.status)
            } else {
                message.chars().take(500).collect()
            }));
        }
        parse_ffprobe_json(&output.stdout)
    }
}

/// Parse the JSON printed by
/// `ffprobe -print_format json -show_format -show_streams`.
pub fn parse_ffprobe_json(bytes: &[u8]) -> Result<MediaProbe, ProbeError> {
    let raw: RawOutput =
        serde_json::from_slice(bytes).map_err(|e| ProbeError::Parse(e.to_string()))?;
    let format = raw.format.unwrap_or_default();
    Ok(MediaProbe {
        container: format
            .format_name
            .map(|n| {
                n.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        duration_ms: format.duration.as_deref().and_then(seconds_to_ms),
        bit_rate: format
            .bit_rate
            .as_deref()
            .and_then(|b| b.trim().parse().ok()),
        streams: raw.streams.into_iter().map(map_stream).collect(),
    })
}

fn seconds_to_ms(value: &str) -> Option<u64> {
    let secs: f64 = value.trim().parse().ok()?;
    (secs.is_finite() && secs >= 0.0).then(|| (secs * 1000.0).round() as u64)
}

fn map_stream(raw: RawStream) -> MediaStream {
    let kind = match raw.codec_type.as_deref() {
        Some("video") => StreamKind::Video,
        Some("audio") => StreamKind::Audio,
        Some("subtitle") => StreamKind::Subtitle,
        Some("attachment") => StreamKind::Attachment,
        Some("data") => StreamKind::Data,
        _ => StreamKind::Other,
    };
    let disposition = raw.disposition.unwrap_or_default();
    MediaStream {
        index: raw.index,
        kind,
        codec: raw.codec_name.filter(|c| !c.is_empty()),
        profile: raw.profile.filter(|p| !p.is_empty()),
        width: raw.width.filter(|w| *w > 0),
        height: raw.height.filter(|h| *h > 0),
        channels: raw.channels.filter(|c| *c > 0),
        language: raw
            .tags
            .and_then(|t| t.language)
            .map(|l| l.trim().to_ascii_lowercase())
            .filter(|l| !l.is_empty() && l != "und"),
        default: disposition.default == 1,
        forced: disposition.forced == 1,
    }
}

#[derive(Debug, Deserialize)]
struct RawOutput {
    #[serde(default)]
    streams: Vec<RawStream>,
    format: Option<RawFormat>,
}

#[derive(Debug, Default, Deserialize)]
struct RawFormat {
    format_name: Option<String>,
    duration: Option<String>,
    bit_rate: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawStream {
    index: u32,
    codec_type: Option<String>,
    codec_name: Option<String>,
    profile: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    channels: Option<u32>,
    tags: Option<RawTags>,
    disposition: Option<RawDisposition>,
}

#[derive(Debug, Deserialize)]
struct RawTags {
    language: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawDisposition {
    #[serde(default)]
    default: u8,
    #[serde(default)]
    forced: u8,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_parsing() {
        assert_eq!(seconds_to_ms("1.000000"), Some(1000));
        assert_eq!(seconds_to_ms("5423.4567"), Some(5_423_457));
        assert_eq!(seconds_to_ms("N/A"), None);
        assert_eq!(seconds_to_ms("-1"), None);
    }

    #[test]
    fn tolerates_missing_format_and_fields() {
        let probe = parse_ffprobe_json(br#"{"streams":[{"index":0}]}"#).unwrap();
        assert!(probe.container.is_empty());
        assert_eq!(probe.duration_ms, None);
        assert_eq!(probe.streams[0].kind, StreamKind::Other);
        assert!(parse_ffprobe_json(b"not json").is_err());
    }
}
