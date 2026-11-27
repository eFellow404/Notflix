//! Subtitle auto-discovery, language/tag parsing, and SRT-to-WebVTT conversion.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{NotflixError, Result};

/// Metadata for a discovered local subtitle track associated with a video file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubtitleTrack {
    pub index: usize,
    pub label: String,
    pub lang: String,
    pub format: String,
    pub filename: String,
    #[serde(skip)]
    pub path: PathBuf,
}

/// Immediate subdirectory names scanned for subtitle files (matched case-insensitively).
const SUBTITLE_DIR_NAMES: &[&str] = &["subs", "subtitles", "sub", "cc"];

/// Mapping from recognized ISO-639-1/2 codes and English names to `(iso_639_1, display_name)`.
const LANGUAGE_TABLE: &[(&str, &str, &str)] = &[
    ("en", "en", "English"),
    ("eng", "en", "English"),
    ("english", "en", "English"),
    ("es", "es", "Spanish"),
    ("spa", "es", "Spanish"),
    ("spanish", "es", "Spanish"),
    ("fr", "fr", "French"),
    ("fre", "fr", "French"),
    ("fra", "fr", "French"),
    ("french", "fr", "French"),
    ("de", "de", "German"),
    ("ger", "de", "German"),
    ("deu", "de", "German"),
    ("german", "de", "German"),
    ("it", "it", "Italian"),
    ("ita", "it", "Italian"),
    ("italian", "it", "Italian"),
    ("pt", "pt", "Portuguese"),
    ("por", "pt", "Portuguese"),
    ("portuguese", "pt", "Portuguese"),
    ("ja", "ja", "Japanese"),
    ("jpn", "ja", "Japanese"),
    ("japanese", "ja", "Japanese"),
    ("ko", "ko", "Korean"),
    ("kor", "ko", "Korean"),
    ("korean", "ko", "Korean"),
    ("zh", "zh", "Chinese"),
    ("chi", "zh", "Chinese"),
    ("zho", "zh", "Chinese"),
    ("chinese", "zh", "Chinese"),
    ("nl", "nl", "Dutch"),
    ("dut", "nl", "Dutch"),
    ("nld", "nl", "Dutch"),
    ("dutch", "nl", "Dutch"),
    ("ru", "ru", "Russian"),
    ("rus", "ru", "Russian"),
    ("russian", "ru", "Russian"),
    ("pl", "pl", "Polish"),
    ("pol", "pl", "Polish"),
    ("polish", "pl", "Polish"),
    ("sv", "sv", "Swedish"),
    ("swe", "sv", "Swedish"),
    ("swedish", "sv", "Swedish"),
    ("ar", "ar", "Arabic"),
    ("ara", "ar", "Arabic"),
    ("arabic", "ar", "Arabic"),
    ("hin", "hi", "Hindi"),
    ("hindi", "hi", "Hindi"),
    ("tr", "tr", "Turkish"),
    ("tur", "tr", "Turkish"),
    ("turkish", "tr", "Turkish"),
];

struct DiscoveredSubtitle {
    track: SubtitleTrack,
    is_forced: bool,
    path_key: String,
}

fn subtitle_format(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?;
    if ext.eq_ignore_ascii_case("vtt") {
        Some("vtt")
    } else if ext.eq_ignore_ascii_case("srt") {
        Some("srt")
    } else {
        None
    }
}

fn is_subtitle_subdir(name: &str) -> bool {
    SUBTITLE_DIR_NAMES
        .iter()
        .any(|candidate| name.eq_ignore_ascii_case(candidate))
}

/// If `sub_stem` equals `video_stem` (case-insensitively) or begins with `<video_stem>`
/// followed by `.`, `_`, `-`, or ` `, return the suffix after `<video_stem>`.
fn extract_matching_suffix(
    sub_stem: &str,
    video_stem: &str,
    video_stem_lower: &str,
) -> Option<String> {
    let sub_lower = sub_stem.to_lowercase();
    if sub_lower == video_stem_lower {
        return Some(String::new());
    }

    let rest_lower = sub_lower.strip_prefix(video_stem_lower)?;
    if !rest_lower.starts_with(['.', '_', '-', ' ']) {
        return None;
    }

    // Preserve original casing of the suffix when byte boundaries align cleanly.
    if sub_stem.is_char_boundary(video_stem.len())
        && sub_stem[..video_stem.len()].to_lowercase() == video_stem_lower
    {
        Some(sub_stem[video_stem.len()..].to_string())
    } else {
        Some(rest_lower.to_string())
    }
}

fn lookup_unambiguous_language(token_lower: &str) -> Option<(&'static str, &'static str)> {
    LANGUAGE_TABLE
        .iter()
        .find(|&&(key, _, _)| key == token_lower)
        .map(|&(_, code, name)| (code, name))
}

/// Parse language code, friendly display label, and whether the track is forced
/// from a subtitle filename suffix (or whole stem inside a nested stem folder).
fn parse_language_and_tags(raw_suffix: &str) -> (String, String, bool) {
    let tokens: Vec<&str> = raw_suffix
        .split(['.', '_', '-', ' ', '(', ')', '[', ']'])
        .filter(|t| !t.is_empty())
        .collect();

    let mut matched_lang: Option<(&'static str, &'static str)> = None;
    let mut is_sdh = false;
    let mut is_forced = false;
    let mut is_commentary = false;
    let mut has_hi_token = false;
    let mut fallback_tokens: Vec<&str> = Vec::new();

    for &tok in &tokens {
        let lower = tok.to_ascii_lowercase();
        match lower.as_str() {
            "sdh" | "cc" => {
                is_sdh = true;
            }
            "forced" => {
                is_forced = true;
            }
            "commentary" => {
                is_commentary = true;
            }
            "hi" => {
                has_hi_token = true;
            }
            _ => {
                if let Some(lang_pair) = lookup_unambiguous_language(&lower) {
                    if matched_lang.is_none() {
                        matched_lang = Some(lang_pair);
                    }
                } else {
                    fallback_tokens.push(tok);
                }
            }
        }
    }

    // Disambiguate `hi`: when another language is already present (e.g. `en.hi.srt`),
    // `hi` denotes Hearing Impaired (SDH); otherwise treat it as ISO-639-1 Hindi.
    if has_hi_token {
        if matched_lang.is_some() {
            is_sdh = true;
        } else {
            matched_lang = Some(("hi", "Hindi"));
        }
    }

    let (lang, mut label) = match matched_lang {
        Some((code, name)) => (code.to_string(), name.to_string()),
        None => {
            let cleaned = fallback_tokens.join(" ");
            let base = if cleaned.is_empty() {
                "Subtitles".to_string()
            } else {
                cleaned
            };
            ("und".to_string(), base)
        }
    };

    if is_sdh {
        label.push_str(" (SDH)");
    }
    if is_forced {
        label.push_str(" (Forced)");
    }
    if is_commentary {
        label.push_str(" (Commentary)");
    }

    (lang, label, is_forced)
}

fn collect_matching_files_in_dir(
    dir: &Path,
    video_stem: &str,
    video_stem_lower: &str,
    match_all: bool,
    seen: &mut HashSet<String>,
    out: &mut Vec<DiscoveredSubtitle>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(format) = subtitle_format(&path) else {
            continue;
        };
        let Some(sub_stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(filename) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };

        let suffix = if match_all {
            // Inside `<sub_dir>/<video_stem>/`, strip `<video_stem>` if present,
            // otherwise parse the whole stem (e.g. `2_English.srt`).
            extract_matching_suffix(sub_stem, video_stem, video_stem_lower)
                .unwrap_or_else(|| sub_stem.to_string())
        } else {
            match extract_matching_suffix(sub_stem, video_stem, video_stem_lower) {
                Some(s) => s,
                None => continue,
            }
        };

        let path_key = path.to_string_lossy().to_lowercase();
        if !seen.insert(path_key.clone()) {
            continue;
        }

        let (lang, label, is_forced) = parse_language_and_tags(&suffix);
        out.push(DiscoveredSubtitle {
            track: SubtitleTrack {
                index: 0,
                label,
                lang,
                format: format.to_string(),
                filename: filename.to_string(),
                path,
            },
            is_forced,
            path_key,
        });
    }
}

/// Auto-discover `.srt` and `.vtt` subtitle tracks associated with `video_path`.
///
/// Searches `video_path`'s parent directory, immediate subdirectories named
/// `subs`/`subtitles`/`sub`/`cc` (case-insensitively), and nested
/// `<sub_dir>/<video_stem>/` folders.
pub fn discover_subtitles(video_path: &Path) -> Vec<SubtitleTrack> {
    let Some(raw_parent) = video_path.parent() else {
        return Vec::new();
    };
    let parent = if raw_parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        raw_parent
    };

    let Some(video_stem) = video_path.file_stem().and_then(|s| s.to_str()) else {
        return Vec::new();
    };
    if video_stem.is_empty() {
        return Vec::new();
    }
    let video_stem_lower = video_stem.to_lowercase();

    let mut seen = HashSet::new();
    let mut discovered = Vec::new();

    // 1. Video's parent directory
    collect_matching_files_in_dir(
        parent,
        video_stem,
        &video_stem_lower,
        false,
        &mut seen,
        &mut discovered,
    );

    // 2. Immediate subtitle subdirectories (`Subs/`, `Subtitles/`, `Sub/`, `CC/`)
    //    and any nested `<sub_dir>/<video_stem>/` folder inside them.
    if let Ok(parent_entries) = std::fs::read_dir(parent) {
        for entry in parent_entries.flatten() {
            let sub_dir = entry.path();
            if !sub_dir.is_dir() {
                continue;
            }
            let Some(dir_name) = sub_dir.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if !is_subtitle_subdir(dir_name) {
                continue;
            }

            collect_matching_files_in_dir(
                &sub_dir,
                video_stem,
                &video_stem_lower,
                false,
                &mut seen,
                &mut discovered,
            );

            if let Ok(sub_entries) = std::fs::read_dir(&sub_dir) {
                for sub_entry in sub_entries.flatten() {
                    let nested = sub_entry.path();
                    if !nested.is_dir() {
                        continue;
                    }
                    let Some(nested_name) = nested.file_name().and_then(|s| s.to_str()) else {
                        continue;
                    };
                    if nested_name.to_lowercase() == video_stem_lower {
                        collect_matching_files_in_dir(
                            &nested,
                            video_stem,
                            &video_stem_lower,
                            true,
                            &mut seen,
                            &mut discovered,
                        );
                    }
                }
            }
        }
    }

    // Deterministic sort: English first, non-forced before forced, then alphabetical by filename.
    discovered.sort_by(|a, b| {
        let a_not_en = a.track.lang != "en";
        let b_not_en = b.track.lang != "en";
        a_not_en
            .cmp(&b_not_en)
            .then_with(|| a.is_forced.cmp(&b.is_forced))
            .then_with(|| {
                a.track
                    .filename
                    .to_lowercase()
                    .cmp(&b.track.filename.to_lowercase())
            })
            .then_with(|| a.path_key.cmp(&b.path_key))
    });

    discovered
        .into_iter()
        .enumerate()
        .map(|(idx, mut item)| {
            item.track.index = idx;
            item.track
        })
        .collect()
}

fn is_srt_timestamp(token: &str) -> bool {
    !token.is_empty()
        && token.contains(':')
        && token
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b':' | b',' | b'.'))
}

fn convert_timing_line(line: &str) -> Option<String> {
    let (left, right) = line.split_once("-->")?;
    let start_raw = left.trim();
    if !is_srt_timestamp(start_raw) {
        return None;
    }

    let right_trimmed = right.trim_start();
    let (end_raw, settings_tail) = match right_trimmed.find(char::is_whitespace) {
        Some(pos) => (&right_trimmed[..pos], &right_trimmed[pos..]),
        None => (right_trimmed, ""),
    };
    if !is_srt_timestamp(end_raw) {
        return None;
    }

    let start_vtt = start_raw.replace(',', ".");
    let end_vtt = end_raw.replace(',', ".");
    let trailing = settings_tail.trim_end();
    if trailing.is_empty() {
        Some(format!("{start_vtt} --> {end_vtt}"))
    } else {
        Some(format!("{start_vtt} --> {end_vtt}{trailing}"))
    }
}

/// Convert SubRip (`.srt`) text to WebVTT (`.vtt`) format.
///
/// Strips UTF-8 BOM, normalizes line endings to `\n`, converts comma decimal
/// separators to dots only on cue timing lines (preserving cue settings and
/// leaving dialogue commas untouched), and prepends `"WEBVTT\n\n"`.
pub fn srt_to_vtt(srt: &str) -> String {
    let without_bom = srt.strip_prefix('\u{feff}').unwrap_or(srt);
    let normalized = without_bom.replace("\r\n", "\n").replace('\r', "\n");
    let body = normalized.trim_start_matches('\n');

    let mut out = String::with_capacity(body.len() + 16);
    out.push_str("WEBVTT\n\n");

    for (i, line) in body.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if line.contains("-->") {
            if let Some(converted) = convert_timing_line(line) {
                out.push_str(&converted);
                continue;
            }
        }
        out.push_str(line);
    }

    out
}

/// Load a discovered subtitle file from disk and return its contents as WebVTT.
///
/// Decodes UTF-8 when valid, falling back to ISO-8859-1 (Latin-1) byte-to-char
/// decoding for legacy DVD subtitle rips.
pub fn load_subtitle_as_vtt(track: &SubtitleTrack) -> Result<String> {
    let bytes = std::fs::read(&track.path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            NotflixError::NotFound(format!("Subtitle file not found: {}", track.filename))
        } else {
            NotflixError::Io(err)
        }
    })?;

    let text = match String::from_utf8(bytes) {
        Ok(utf8) => utf8,
        Err(err) => err
            .into_bytes()
            .into_iter()
            .map(|b| b as char)
            .collect::<String>(),
    };

    if track.format.eq_ignore_ascii_case("srt") {
        return Ok(srt_to_vtt(&text));
    }

    if track.format.eq_ignore_ascii_case("vtt") {
        let without_bom = text.strip_prefix('\u{feff}').unwrap_or(&text);
        let normalized = without_bom.replace("\r\n", "\n").replace('\r', "\n");
        if normalized.starts_with("WEBVTT") {
            return Ok(normalized);
        }
        return Ok(format!("WEBVTT\n\n{}", normalized.trim_start_matches('\n')));
    }

    Err(NotflixError::BadRequest(format!(
        "Unsupported subtitle format: {}",
        track.format
    )))
}
