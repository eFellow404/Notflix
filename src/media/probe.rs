//! Audio stream inspection via `ffprobe`.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::ffmpeg_bin::resolve_ffprobe;

const FFPROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Metadata for an audio stream probed from a local media container via `ffprobe`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AudioTrack {
    pub index: usize,
    pub codec: String,
    pub channels: u32,
    pub language: String,
    pub title: String,
    pub browser_compatible: bool,
}

/// Return `true` if the given `ffprobe` audio codec name is natively supported by
/// standard HTML5 `<video>` browsers without server-side transcoding.
pub fn is_browser_compatible_audio_codec(codec: &str) -> bool {
    const COMPATIBLE: &[&str] = &[
        "aac",
        "mp3",
        "opus",
        "vorbis",
        "flac",
        "mp2",
        "pcm_s16le",
        "pcm_s24le",
        "pcm_f32le",
        "pcm_u8",
        "alac",
    ];
    let trimmed = codec.trim();
    COMPATIBLE.iter().any(|c| trimmed.eq_ignore_ascii_case(c))
}

#[derive(Deserialize)]
struct FfprobeOutput {
    #[serde(default)]
    streams: Vec<FfprobeStream>,
}

#[derive(Deserialize)]
struct FfprobeStream {
    #[serde(default)]
    codec_name: String,
    channels: Option<u32>,
    tags: Option<serde_json::Map<String, serde_json::Value>>,
}

fn non_empty_trimmed(val: &serde_json::Value) -> Option<String> {
    let trimmed = val.as_str()?.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn extract_tag_str(
    tags: Option<&serde_json::Map<String, serde_json::Value>>,
    key: &str,
) -> Option<String> {
    let map = tags?;
    if let Some(val) = map.get(key).and_then(non_empty_trimmed) {
        return Some(val);
    }
    for (k, v) in map {
        if k.eq_ignore_ascii_case(key) {
            if let Some(val) = non_empty_trimmed(v) {
                return Some(val);
            }
        }
    }
    None
}

/// Parse JSON output from
/// `ffprobe -v quiet -print_format json -show_streams -select_streams a <path>`
/// into ordered [`AudioTrack`] metadata entries.
pub fn parse_ffprobe_audio_streams(json_str: &str) -> Vec<AudioTrack> {
    let Ok(parsed) = serde_json::from_str::<FfprobeOutput>(json_str) else {
        return Vec::new();
    };

    parsed
        .streams
        .into_iter()
        .enumerate()
        .map(|(idx, stream)| {
            let codec = stream.codec_name.trim().to_ascii_lowercase();
            let channels = stream.channels.filter(|&c| c > 0).unwrap_or(2);
            let language = extract_tag_str(stream.tags.as_ref(), "language")
                .unwrap_or_else(|| "und".to_string());
            let title = extract_tag_str(stream.tags.as_ref(), "title").unwrap_or_default();
            let browser_compatible = is_browser_compatible_audio_codec(&codec);
            AudioTrack {
                index: idx,
                codec,
                channels,
                language,
                title,
                browser_compatible,
            }
        })
        .collect()
}

/// Probe audio streams for a local video file using `ffprobe` (5 s timeout).
///
/// Returns an empty list if `ffprobe` is unavailable, times out, or fails.
pub fn probe_audio_tracks(file_path: &Path) -> Vec<AudioTrack> {
    let Some(ffprobe_bin) = resolve_ffprobe() else {
        return Vec::new();
    };
    let mut cmd = std::process::Command::new(ffprobe_bin);
    cmd.args([
        "-v",
        "quiet",
        "-print_format",
        "json",
        "-show_streams",
        "-select_streams",
        "a",
    ])
    .arg(file_path);

    let outcome = crate::util::run_command_with_timeout(&mut cmd, FFPROBE_TIMEOUT, false);
    if !outcome.success() {
        return Vec::new();
    }
    let stdout = String::from_utf8_lossy(&outcome.stdout);
    parse_ffprobe_audio_streams(&stdout)
}
