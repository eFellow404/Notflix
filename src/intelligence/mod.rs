//! Intelligent scanner and episode ordering engine for DVD-ripped TV folders.

mod consensus;
mod patterns;

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use walkdir::WalkDir;

use crate::error::{NotflixError, Result};
use crate::util::{canonicalize_clean, expand_tilde};
use consensus::{
    apply_three_digit_see_consensus, extract_sibling_diff_numbers, find_constant_sibling_numbers,
};
use patterns::{
    clean_display_title, extract_folder_context, is_extra_stem, is_pilot_stem, match_dvd_pattern,
    match_explicit_se, match_fallback_number, strip_noise,
};

/// Supported video file extensions (lowercase, with leading dot).
pub const VIDEO_EXTENSIONS: &[&str] = &[
    ".mp4", ".mkv", ".m4v", ".webm", ".avi", ".mov", ".mpg", ".mpeg", ".vob", ".ts", ".m2ts",
    ".ogv", ".wmv", ".flv",
];

const IGNORED_DIRS: &[&str] = &[
    "__MACOSX",
    "@eaDir",
    ".Trashes",
    "$RECYCLE.BIN",
    ".git",
    "AUDIO_TS",
];

/// Returns `true` if `ext` is a recognized video file extension.
///
/// Accepts either leading-dot (`".mp4"`) or bare (`"mp4"`) forms, case-insensitively.
pub fn is_video_extension(ext: &str) -> bool {
    let trimmed = ext.trim();
    let bare = trimmed.strip_prefix('.').unwrap_or(trimmed);
    !bare.is_empty()
        && VIDEO_EXTENSIONS
            .iter()
            .any(|&candidate| bare.eq_ignore_ascii_case(&candidate[1..]))
}

/// Token produced by [`natural_sort_key`] for natural alphanumeric ordering.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum NaturalToken {
    Text(String),
    Number(u64),
}

/// Split `text` into lowercase text chunks and numeric chunks for natural ordering.
///
/// Matches Python's `re.split(r"(\d+)", text)` alternating layout (`Text`, `Number`, `Text`, ...)
/// so text is always compared with text and numbers with numbers. Digit runs exceeding `u64::MAX`
/// safely fall back to `NaturalToken::Text` without panicking.
pub fn natural_sort_key(text: &str) -> Vec<NaturalToken> {
    let mut tokens = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut text_start = 0;

    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            tokens.push(NaturalToken::Text(text[text_start..i].to_lowercase()));
            let num_start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let num_str = &text[num_start..i];
            match num_str.parse::<u64>() {
                Ok(n) => tokens.push(NaturalToken::Number(n)),
                Err(_) => tokens.push(NaturalToken::Text(num_str.to_string())),
            }
            text_start = i;
        } else {
            i += 1;
        }
    }
    tokens.push(NaturalToken::Text(text[text_start..].to_lowercase()));
    tokens
}

/// Intermediate representation produced by the intelligent file analyzer.
#[derive(Debug, Clone)]
pub struct AnalyzedEpisode {
    pub file_path: String,
    pub relative_path: String,
    pub filename: String,
    pub parent_rel: String,
    pub file_size: u64,
    pub title: String,
    pub season: Option<i64>,
    pub episode_number: Option<i64>,
    pub disc: Option<i64>,
    pub is_extra: bool,
    pub is_pilot: bool,
    pub confidence: f64,
    pub sort_reason: String,
    pub raw_numeric_candidate: Option<i64>,
    pub explicit_season_episode: bool,
}

fn resolve_folder(folder_path: &Path) -> Result<PathBuf> {
    let expanded = expand_tilde(folder_path);
    if !expanded.exists() || !expanded.is_dir() {
        return Err(NotflixError::NotFound(format!(
            "Folder does not exist: {}",
            folder_path.display()
        )));
    }
    Ok(canonicalize_clean(&expanded).unwrap_or(expanded))
}

fn path_to_posix(path: &Path) -> String {
    let mut out = String::new();
    for component in path.components() {
        let segment = match component {
            Component::Normal(os) => os.to_string_lossy(),
            Component::CurDir => Cow::Borrowed("."),
            Component::ParentDir => Cow::Borrowed(".."),
            _ => continue,
        };
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(&segment);
    }
    if out.is_empty() {
        ".".to_string()
    } else {
        out
    }
}

fn analyze_single_file(
    root: &Path,
    file_path: &Path,
    file_size: u64,
    is_symlink: bool,
    stem: &str,
    cleaned_stem: &str,
    constant_sibling_nums: &HashSet<String>,
) -> AnalyzedEpisode {
    let rel = file_path.strip_prefix(root).unwrap_or(file_path);
    let filename = file_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    let (mut season, mut disc, folder_extra) = extract_folder_context(rel);
    let mut is_extra = folder_extra || is_extra_stem(stem);
    let is_pilot = is_pilot_stem(stem);

    let mut episode_number: Option<i64> = None;
    let mut confidence = 0.45;
    let mut sort_reason = "Natural filename order".to_string();
    let mut raw_numeric: Option<i64> = None;
    let mut explicit_se = false;

    if let Some((s, ep, conf, reason)) = match_explicit_se(cleaned_stem) {
        season = Some(s);
        episode_number = Some(ep);
        confidence = conf;
        sort_reason = reason;
        explicit_se = true;
    } else if let Some((d, ep, marks_extra, conf, reason)) = match_dvd_pattern(cleaned_stem, disc) {
        disc = d;
        episode_number = Some(ep);
        is_extra = is_extra || marks_extra;
        confidence = conf;
        sort_reason = reason;
    } else if let Some((ep, raw, conf, reason)) =
        match_fallback_number(cleaned_stem, season, constant_sibling_nums)
    {
        episode_number = Some(ep);
        raw_numeric = raw;
        confidence = conf;
        sort_reason = reason;
    } else if is_pilot && !is_extra {
        episode_number = Some(0);
        confidence = 0.82;
        sort_reason = "Detected Pilot episode".to_string();
    }

    if is_extra && confidence < 0.70 {
        sort_reason = "Detected Extra / Bonus feature".to_string();
        confidence = 0.85;
    }

    // `root` is already canonicalized and `WalkDir` does not follow symlinked directories,
    // so only symlinked video files need per-file `canonicalize_clean`.
    let resolved_path = if is_symlink {
        canonicalize_clean(file_path).unwrap_or_else(|_| file_path.to_path_buf())
    } else {
        file_path.to_path_buf()
    };
    let relative_path = path_to_posix(rel);
    let parent_rel = rel
        .parent()
        .map(path_to_posix)
        .unwrap_or_else(|| ".".to_string());

    AnalyzedEpisode {
        file_path: resolved_path.to_string_lossy().into_owned(),
        relative_path,
        filename,
        parent_rel,
        file_size,
        title: String::new(),
        season,
        episode_number,
        disc,
        is_extra,
        is_pilot,
        confidence,
        sort_reason,
        raw_numeric_candidate: raw_numeric,
        explicit_season_episode: explicit_se,
    }
}

/// Discover video files under `root`, returning `(path, file_size, is_symlink)` tuples
/// so regular files avoid redundant `metadata` and `canonicalize_clean` syscalls later.
fn discover_video_files_in_root(root: &Path) -> Vec<(PathBuf, u64, bool)> {
    let mut found = Vec::new();
    let walker = WalkDir::new(root)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| {
            if entry.depth() == 0 {
                return true;
            }
            let name = entry.file_name().to_string_lossy();
            if name.starts_with('.') {
                return false;
            }
            if entry.file_type().is_dir() && IGNORED_DIRS.contains(&name.as_ref()) {
                return false;
            }
            true
        });

    for entry in walker {
        let Ok(entry) = entry else {
            continue;
        };
        let ft = entry.file_type();
        let is_symlink = ft.is_symlink();
        if !ft.is_file() && !is_symlink {
            continue;
        }
        let is_video = entry
            .path()
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(is_video_extension);
        if !is_video {
            continue;
        }
        let file_size = if is_symlink {
            match std::fs::metadata(entry.path()) {
                Ok(meta) if meta.is_file() => meta.len(),
                _ => continue,
            }
        } else {
            entry.metadata().map(|m| m.len()).unwrap_or(0)
        };
        found.push((entry.into_path(), file_size, is_symlink));
    }

    found
}

/// Recursively discover video files inside `folder_path`.
///
/// Prunes hidden directories (starting with `.`) and ignored system directories,
/// skips hidden files, and returns paths in deterministic sorted order.
pub fn discover_video_files(folder_path: &Path) -> Result<Vec<PathBuf>> {
    let root = resolve_folder(folder_path)?;
    Ok(discover_video_files_in_root(&root)
        .into_iter()
        .map(|(path, _, _)| path)
        .collect())
}

/// If a Pilot has no season but other episodes have seasons, align Pilot with the earliest season.
fn align_pilot_seasons(analyzed: &mut [AnalyzedEpisode]) {
    let min_season = analyzed
        .iter()
        .filter(|it| !it.is_extra)
        .filter_map(|it| it.season)
        .min();
    if let Some(min_s) = min_season {
        for it in analyzed {
            if it.is_pilot && it.season.is_none() && !it.is_extra {
                it.season = Some(min_s);
            }
        }
    }
}

type EpisodeSortKey = (u8, (u8, i64), (u8, i64), (u8, i64), Vec<NaturalToken>);

fn episode_sort_key(it: &AnalyzedEpisode) -> EpisodeSortKey {
    let extra_rank = u8::from(it.is_extra);
    let season_key = match it.season {
        Some(s) => (0u8, s),
        None => (1u8, 0),
    };
    let disc_key = match it.disc {
        Some(d) => (0u8, d),
        None => (1u8, 0),
    };
    let ep_key = match it.episode_number {
        Some(e) => (0u8, e),
        None => (1u8, 0),
    };
    (
        extra_rank,
        season_key,
        disc_key,
        ep_key,
        natural_sort_key(&it.relative_path),
    )
}

/// Scan a TV show folder and return episodes intelligently ordered.
pub fn scan_and_order_folder(folder_path: &Path) -> Result<Vec<AnalyzedEpisode>> {
    let root = resolve_folder(folder_path)?;
    let video_files = discover_video_files_in_root(&root);
    if video_files.is_empty() {
        return Ok(Vec::new());
    }

    // Compute stems and noise-stripped stems once per discovered file.
    let stems: Vec<String> = video_files
        .iter()
        .map(|(vp, _, _)| {
            vp.file_stem()
                .or_else(|| vp.file_name())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
        .collect();
    let cleaned_stems: Vec<String> = stems.iter().map(|s| strip_noise(s)).collect();

    // Group file indices by parent folder to identify constant show-title numbers (e.g. "24", "100")
    let mut indices_by_parent: HashMap<&Path, Vec<usize>> = HashMap::new();
    for (idx, (vp, _, _)) in video_files.iter().enumerate() {
        let parent = vp.parent().unwrap_or(&root);
        indices_by_parent.entry(parent).or_default().push(idx);
    }

    let constant_by_parent: HashMap<&Path, HashSet<String>> = indices_by_parent
        .iter()
        .map(|(&parent, indices)| {
            let sibling_cleaned: Vec<&str> = indices
                .iter()
                .map(|&idx| cleaned_stems[idx].as_str())
                .collect();
            (parent, find_constant_sibling_numbers(&sibling_cleaned))
        })
        .collect();

    let empty_set = HashSet::new();
    let mut analyzed: Vec<AnalyzedEpisode> = video_files
        .iter()
        .enumerate()
        .map(|(idx, (vp, file_size, is_symlink))| {
            let parent = vp.parent().unwrap_or(&root);
            let constants = constant_by_parent.get(parent).unwrap_or(&empty_set);
            analyze_single_file(
                &root,
                vp,
                *file_size,
                *is_symlink,
                &stems[idx],
                &cleaned_stems[idx],
                constants,
            )
        })
        .collect();

    // Folder-level consensus passes
    extract_sibling_diff_numbers(
        &mut analyzed,
        &cleaned_stems,
        &indices_by_parent,
        &constant_by_parent,
    );
    apply_three_digit_see_consensus(&mut analyzed);

    // Synthesize display titles once using final post-consensus metadata
    for ((item, stem), cleaned) in analyzed
        .iter_mut()
        .zip(stems.iter())
        .zip(cleaned_stems.iter())
    {
        item.title = clean_display_title(
            stem,
            cleaned,
            item.season,
            item.episode_number,
            item.disc,
            item.is_extra,
        );
    }

    align_pilot_seasons(&mut analyzed);
    analyzed.sort_by_cached_key(episode_sort_key);

    Ok(analyzed)
}
