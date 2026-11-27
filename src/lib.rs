//! Notflix — Lightweight local TV show player built for plain folders of DVD-ripped video files.

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const BUILD_SHA: &str = env!("NOTFLIX_BUILD_SHA");

pub mod chapters;
pub mod error;
pub mod intelligence;
pub mod intro_detector;
pub mod linked_list;
pub mod media;
pub mod server;
pub mod storage;
pub mod subtitles;
pub mod updater;
pub(crate) mod util;

pub use util::{open_browser, strip_windows_verbatim_prefix};
