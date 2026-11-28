//! Media MIME helpers, HTTP 206 byte-range video streaming, audio track probing,
//! on-the-fly ffmpeg remuxing, and local artwork discovery.

mod artwork;
mod probe;
mod streaming;

use crate::ffmpeg_bin::resolve_ffmpeg;

pub use artwork::{find_or_generate_episode_thumbnail, find_show_poster, guess_image_mime};
pub use probe::{
    is_browser_compatible_audio_codec, parse_ffprobe_audio_streams, probe_audio_tracks, AudioTrack,
};
pub use streaming::{
    guess_video_mime, parse_range_header, stream_remuxed_video, stream_video_file,
};

/// 256 KiB chunks for smooth seeking and low memory footprint.
pub const CHUNK_SIZE: usize = 256 * 1024;

/// Return `true` if `ffmpeg` is available (system `PATH` or auto-downloaded).
pub fn detect_ffmpeg() -> bool {
    resolve_ffmpeg().is_some()
}
