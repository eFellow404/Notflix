//! Notflix — Lightweight local TV show player built for plain folders of DVD-ripped video files.

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod chapters;
pub mod error;
pub mod intelligence;
pub mod intro_detector;
pub mod linked_list;
pub mod media;
pub mod server;
pub mod storage;
pub mod updater;
pub(crate) mod util;

pub use util::strip_windows_verbatim_prefix;
