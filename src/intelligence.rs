//! Intelligent scanner and episode ordering engine for DVD-ripped TV folders.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use walkdir::WalkDir;

use crate::error::{NotflixError, Result};
use crate::util::{canonicalize_clean, expand_tilde};

/// Supported video file extensions (lowercase, with leading dot).
pub const VIDEO_EXTENSIONS: &[&str] = &[
    ".mp4", ".mkv", ".m4v", ".webm", ".avi", ".mov", ".mpg", ".mpeg", ".vob", ".ts", ".m2ts",
    ".ogv", ".wmv", ".flv",
];

const VIDEO_EXTENSIONS_BARE: &[&str] = &[
    "mp4", "mkv", "m4v", "webm", "avi", "mov", "mpg", "mpeg", "vob", "ts", "m2ts", "ogv", "wmv",
    "flv",
];

const IGNORED_DIRS: &[&str] = &[
    "__MACOSX",
    "@eaDir",
    ".Trashes",
    "$RECYCLE.BIN",
    ".git",
    "AUDIO_TS",
];

static EXTRA_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)(?:^|[\s._\-/(\[])(?:",
        r"extras?|bonus|featurettes?|behind[\s._-]?the[\s._-]?scenes|",
        r"deleted[\s._-]?scenes?|bloopers?|gag[\s._-]?reels?|outtakes?|",
        r"trailers?|promos?|commentary|making[\s._-]?of|interviews?|",
        r"special[\s._-]?features?|dvd[\s._-]?menu|menus?|sample",
        r")(?:$|[\s._\-/)\]])",
    ))
    .expect("valid EXTRA_RE")
});

static PILOT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:^|[\s._\-(\[])(?:pilot|prologue)(?:$|[\s._\-)\]])").expect("valid PILOT_RE")
});

// DVD menu VOBs like VIDEO_TS.VOB or VTS_01_0.VOB
static DVD_MENU_VOB_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:video_ts|vts_\d+_0)$").expect("valid DVD_MENU_VOB_RE"));

// Noise tokens removed prior to fallback numeric parsing
static CRC_BRACKET_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\[\(][0-9a-fA-F]{6,10}[\]\)]").expect("valid CRC_BRACKET_RE"));

static RESOLUTION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:2160|1440|1080|720|576|480|360)[pi]\b|\b(?:4k|uhd|fhd)\b")
        .expect("valid RESOLUTION_RE")
});

static CODEC_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)\b(?:x264|x265|h\.?264|h\.?265|hevc|av1|xvid|divx|mpeg[24]?|",
        r"ac3|aac|dts(?:-hd)?|ddp?5\.?1|dd5\.?1|5\.1|7\.1|2\.0|10bit|8bit|",
        r"dvdrip|bdrip|brrip|bluray|blu-ray|webrip|web-dl|webdl|hdtv|remux|",
        r"proper|repack|ntsc|pal|uncut|extended|directors?[\s._-]?cut)\b",
    ))
    .expect("valid CODEC_RE")
});

static YEAR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|[\s._\-(\[])(?:19[5-9]\d|20[0-3]\d)(?:$|[\s._\-)\]])").expect("valid YEAR_RE")
});

// Folder context patterns
static FOLDER_SEASON_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:^|[\s._\-])(?:season|series|book|volume|vol\.?|year|s)[\s._\-]?(\d{1,3})(?:$|[\s._\-])",
    )
    .expect("valid FOLDER_SEASON_RE")
});

static FOLDER_DISC_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:^|[\s._\-])(?:disc|disk|dvd|cd|d)[\s._\-]?(\d{1,3}|[a-d])(?:$|[\s._\-])")
        .expect("valid FOLDER_DISC_RE")
});

static SHORT_SEASON_FOLDER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^s0?(\d{1,2})$").expect("valid SHORT_SEASON_FOLDER_RE"));

// Ranked filename patterns
static SXXEXX_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:^|[\s._\-(\[])s(?:eason)?[\s._\-]?(\d{1,3})[\s._\-]*e(?:p(?:isode)?)?[\s._\-]?(\d{1,4})(?:[\s._\-]*-?e?(\d{1,4}))?(?:$|[\s._\-)\]])",
    )
    .expect("valid SXXEXX_RE")
});

static NXN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:^|[\s._\-(\[])(\d{1,2})x(\d{1,3})(?:$|[\s._\-)\]])").expect("valid NXN_RE")
});

static VERBOSE_SE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)season[\s._\-]+(\d{1,3})[\s._\-]+(?:ep(?:isode)?|part|pt)[\s._\-]+(\d{1,4})")
        .expect("valid VERBOSE_SE_RE")
});

static DISC_TITLE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:disc|disk|dvd|d)[\s._\-]?(\d{1,2})[\s._\-]*(?:title|track|tr|ep|t)[\s._\-]?(\d{1,3})")
        .expect("valid DISC_TITLE_RE")
});

static MAKEMKV_TITLE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:^|[\s._\-])(?:title|track|chapter|clip|t)[\s._\-]?t?(\d{1,3})(?:$|[\s._\-])",
    )
    .expect("valid MAKEMKV_TITLE_RE")
});

static VTS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bvts[\s._\-]?(\d{1,2})[\s._\-]?(\d{1,2})\b").expect("valid VTS_RE")
});

static EPISODE_ONLY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:^|[\s._\-(\[])(?:episode|ep\.?|e|part|pt\.?|#|no\.?)[\s._\-]?(\d{1,4}|[ivxlc]{1,5})(?:$|[\s._\-)\]])",
    )
    .expect("valid EPISODE_ONLY_RE")
});

static LEADING_NUM_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[\s._\-]*(\d{1,4})(?:[\s._\-]+|$)").expect("valid LEADING_NUM_RE")
});

static TRAILING_OR_DELIM_NUM_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|[\s._\-(\[])(\d{1,4})(?:$|[\s._\-)\]])")
        .expect("valid TRAILING_OR_DELIM_NUM_RE")
});

// General tokenizing and display-title cleanup patterns
static DIGITS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+").expect("valid DIGITS_RE"));

static SHORT_NUM_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d{1,4}").expect("valid SHORT_NUM_RE"));

static DOT_UNDERSCORE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[._]+").expect("valid DOT_UNDERSCORE_RE"));

static LEADING_SEP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*[-–—:]+\s*").expect("valid LEADING_SEP_RE"));

static TRAILING_SEP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s*[-–—:]+\s*$").expect("valid TRAILING_SEP_RE"));

static MULTI_SPACE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s{2,}").expect("valid MULTI_SPACE_RE"));

static WORD_LETTERS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[a-zA-Z]{2,}").expect("valid WORD_LETTERS_RE"));

static LEADING_NUM_TITLE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\d{1,4}\s*[-–—:\s]\s*(.+)$").expect("valid LEADING_NUM_TITLE_RE")
});

/// Returns `true` if `ext` is a recognized video file extension.
///
/// Accepts either leading-dot (`".mp4"`) or bare (`"mp4"`) forms, case-insensitively.
pub fn is_video_extension(ext: &str) -> bool {
    let trimmed = ext.trim();
    let bare = trimmed.strip_prefix('.').unwrap_or(trimmed);
    !bare.is_empty()
        && VIDEO_EXTENSIONS_BARE
            .iter()
            .any(|&candidate| bare.eq_ignore_ascii_case(candidate))
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

fn capture_i64(re: &Regex, text: &str, group: usize) -> Option<i64> {
    re.captures(text)?.get(group)?.as_str().parse::<i64>().ok()
}

fn capture_i64_pair(re: &Regex, text: &str, first: usize, second: usize) -> Option<(i64, i64)> {
    let caps = re.captures(text)?;
    let a = caps.get(first)?.as_str().parse::<i64>().ok()?;
    let b = caps.get(second)?.as_str().parse::<i64>().ok()?;
    Some((a, b))
}

fn parse_int_or_roman(token: &str) -> Option<i64> {
    let t = token.trim();
    if t.is_empty() {
        return None;
    }
    if t.bytes().all(|b| b.is_ascii_digit()) {
        return t.parse::<i64>().ok();
    }
    if t.len() > 5 || !t.is_ascii() {
        return None;
    }
    let mut buf = [0u8; 5];
    for (dst, &src) in buf.iter_mut().zip(t.as_bytes()) {
        *dst = src.to_ascii_lowercase();
    }
    match std::str::from_utf8(&buf[..t.len()]).ok()? {
        "i" => Some(1),
        "ii" => Some(2),
        "iii" => Some(3),
        "iv" => Some(4),
        "v" => Some(5),
        "vi" => Some(6),
        "vii" => Some(7),
        "viii" => Some(8),
        "ix" => Some(9),
        "x" => Some(10),
        "xi" => Some(11),
        "xii" => Some(12),
        "xiii" => Some(13),
        "xiv" => Some(14),
        "xv" => Some(15),
        "xvi" => Some(16),
        "xvii" => Some(17),
        "xviii" => Some(18),
        "xix" => Some(19),
        "xx" => Some(20),
        _ => None,
    }
}

fn parse_disc_token(token: &str) -> Option<i64> {
    let t = token.trim();
    if t.is_empty() {
        return None;
    }
    if t.bytes().all(|b| b.is_ascii_digit()) {
        return t.parse::<i64>().ok();
    }
    if let &[b] = t.as_bytes() {
        let lower = b.to_ascii_lowercase();
        if lower.is_ascii_lowercase() {
            return Some(i64::from(lower - b'a' + 1));
        }
    }
    None
}

fn strip_noise(stem: &str) -> String {
    let cleaned = CRC_BRACKET_RE.replace_all(stem, " ");
    let cleaned = RESOLUTION_RE.replace_all(&cleaned, " ");
    CODEC_RE.replace_all(&cleaned, " ").into_owned()
}

fn extract_folder_context(rel_path: &Path) -> (Option<i64>, Option<i64>, bool) {
    let mut season: Option<i64> = None;
    let mut disc: Option<i64> = None;
    let mut is_extra = false;

    // Inspect directory names only (excluding filename)
    let Some(parent) = rel_path.parent() else {
        return (season, disc, is_extra);
    };
    for component in parent.components() {
        let Component::Normal(os) = component else {
            continue;
        };
        let part = os.to_string_lossy();
        if EXTRA_RE.is_match(&part) {
            is_extra = true;
        }
        if let Some(parsed) = capture_i64(&FOLDER_SEASON_RE, &part, 1) {
            season = Some(parsed);
        }
        if let Some(parsed) = FOLDER_DISC_RE
            .captures(&part)
            .and_then(|caps| caps.get(1))
            .and_then(|m| parse_disc_token(m.as_str()))
        {
            disc = Some(parsed);
        }
        // Special case: folder literally named "S1", "S02", etc.
        if season.is_none() {
            season = capture_i64(&SHORT_SEASON_FOLDER_RE, part.trim(), 1);
        }
    }

    (season, disc, is_extra)
}

fn clean_display_title(
    stem: &str,
    cleaned_stem: &str,
    season: Option<i64>,
    episode_number: Option<i64>,
    disc: Option<i64>,
    is_extra: bool,
) -> String {
    // Remove explicit S01E02 / 1x02 / leading number prefixes if meaningful words follow
    let stripped = SXXEXX_RE.replace_all(cleaned_stem, " ");
    let stripped = NXN_RE.replace_all(&stripped, " ");
    let stripped = VERBOSE_SE_RE.replace_all(&stripped, " ");
    let stripped = DOT_UNDERSCORE_RE.replace_all(&stripped, " ");
    let stripped = LEADING_SEP_RE.replace_all(&stripped, "");
    let stripped = TRAILING_SEP_RE.replace_all(&stripped, "");
    let stripped = MULTI_SPACE_RE.replace_all(&stripped, " ");
    let stripped = stripped.trim();

    // If the stripped title still has descriptive words (not just bare numbers), use it
    if !stripped.is_empty() && WORD_LETTERS_RE.is_match(stripped) {
        // Remove leading bare episode number like "01 - Pilot" -> "Pilot"
        if let Some(rest) = LEADING_NUM_TITLE_RE
            .captures(stripped)
            .and_then(|caps| caps.get(1))
        {
            if WORD_LETTERS_RE.is_match(rest.as_str()) {
                return rest.as_str().trim().to_string();
            }
        }
        return stripped.to_string();
    }

    // Otherwise synthesize a neat human-readable title
    if is_extra {
        let cleaned_extra = DOT_UNDERSCORE_RE.replace_all(stem, " ");
        let trimmed = cleaned_extra.trim();
        return if trimmed.is_empty() {
            "Bonus Feature".to_string()
        } else {
            trimmed.to_string()
        };
    }
    if let (Some(s), Some(e)) = (season, episode_number) {
        return format!("Season {s}, Episode {e}");
    }
    if let (Some(d), Some(e)) = (disc, episode_number) {
        return format!("Disc {d}, Title {e:02}");
    }
    if let Some(e) = episode_number {
        return format!("Episode {e}");
    }
    let fallback = DOT_UNDERSCORE_RE.replace_all(stem, " ");
    let trimmed = fallback.trim();
    if trimmed.is_empty() {
        stem.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Identify numeric tokens that appear identically across >=75% of siblings.
///
/// Prevents show titles containing numbers (e.g. '24', 'The 100', 'Brooklyn 99',
/// or a series year like '2004') from being mistaken for episode numbers.
fn find_constant_sibling_numbers(cleaned_stems: &[&str]) -> HashSet<String> {
    if cleaned_stems.len() < 2 {
        return HashSet::new();
    }

    let mut token_doc_freq: HashMap<&str, usize> = HashMap::new();
    for cleaned in cleaned_stems {
        let unique_nums: HashSet<&str> = DIGITS_RE.find_iter(cleaned).map(|m| m.as_str()).collect();
        for num in unique_nums {
            *token_doc_freq.entry(num).or_insert(0) += 1;
        }
    }

    let threshold = (((cleaned_stems.len() as f64) * 0.75 + 0.999) as usize).max(2);
    token_doc_freq
        .into_iter()
        .filter(|&(_, freq)| freq >= threshold)
        .map(|(num, _)| num.to_string())
        .collect()
}

/// For unresolved siblings sharing a naming template, extract the varying number.
fn extract_sibling_diff_numbers(
    items: &mut [AnalyzedEpisode],
    cleaned_stems: &[String],
    indices_by_parent: &HashMap<&Path, Vec<usize>>,
    constant_by_parent: &HashMap<&Path, HashSet<String>>,
) {
    for (&parent, sibling_indices) in indices_by_parent {
        if sibling_indices.len() < 2 {
            continue;
        }
        let Some(constant_nums) = constant_by_parent.get(parent) else {
            continue;
        };

        for &idx in sibling_indices {
            let item = &mut items[idx];
            if item.explicit_season_episode || item.episode_number.is_some() {
                continue;
            }

            let cleaned = YEAR_RE.replace_all(&cleaned_stems[idx], " ");
            let last_candidate = SHORT_NUM_RE
                .find_iter(&cleaned)
                .map(|m| m.as_str())
                .filter(|tok| !constant_nums.contains(*tok))
                .last();

            if let Some(chosen) = last_candidate.and_then(|tok| tok.parse::<i64>().ok()) {
                item.episode_number = Some(chosen);
                item.raw_numeric_candidate = Some(chosen);
                item.confidence = item.confidence.max(0.84);
                item.sort_reason = format!("Sibling diff token #{chosen}");
            }
        }
    }
}

/// Detect whether 3-digit episode numbers (101, 102, 201, 202) encode Season+Episode.
fn apply_three_digit_see_consensus(items: &mut [AnalyzedEpisode]) {
    let candidate_indices: Vec<usize> = items
        .iter()
        .enumerate()
        .filter_map(|(idx, it)| {
            let raw = it.raw_numeric_candidate?;
            (!it.explicit_season_episode && !it.is_extra && (101..=2999).contains(&raw))
                .then_some(idx)
        })
        .collect();

    if candidate_indices.is_empty() {
        return;
    }

    let mut min_ep_by_season: HashMap<i64, i64> = HashMap::new();
    let mut total_grouped = 0usize;
    let mut folder_matches = 0usize;

    for &idx in &candidate_indices {
        let it = &items[idx];
        let raw = it
            .raw_numeric_candidate
            .expect("candidate has raw_numeric_candidate");
        let h = raw / 100;
        let ep = raw % 100;
        if (1..=45).contains(&ep) {
            min_ep_by_season
                .entry(h)
                .and_modify(|min_ep| *min_ep = (*min_ep).min(ep))
                .or_insert(ep);
            total_grouped += 1;
            if it.season == Some(h) {
                folder_matches += 1;
            }
        }
    }

    // Valid SEE pattern if:
    // (a) multiple seasons are represented and episode offsets stay in a realistic season range, OR
    // (b) numbers consistently match their enclosing Season folder (e.g. Season 2/201.mp4).
    let multi_season = min_ep_by_season.len() >= 2
        && total_grouped == candidate_indices.len()
        && min_ep_by_season.values().all(|&min_ep| min_ep <= 5);
    let folder_confirmed =
        folder_matches >= (candidate_indices.len() / 2).max(1) && folder_matches > 0;

    if !(multi_season || folder_confirmed) {
        return;
    }

    for &idx in &candidate_indices {
        let it = &mut items[idx];
        if let Some(raw) = it.raw_numeric_candidate {
            let s = raw / 100;
            let e = raw % 100;
            if (1..=45).contains(&e) {
                it.season = Some(s);
                it.episode_number = Some(e);
                it.confidence = it.confidence.max(0.88);
                it.sort_reason = format!("Inferred S{s:02}E{e:02} from {raw} numbering");
            }
        }
    }
}

/// Match explicit S01E02 / Season 1 Episode 2 / 1x02 markers.
fn match_explicit_se(cleaned: &str) -> Option<(i64, i64, f64, String)> {
    if let Some((season, ep)) = capture_i64_pair(&SXXEXX_RE, cleaned, 1, 2) {
        return Some((season, ep, 0.98, format!("Matched S{season:02}E{ep:02}")));
    }
    if let Some((season, ep)) = capture_i64_pair(&VERBOSE_SE_RE, cleaned, 1, 2) {
        return Some((
            season,
            ep,
            0.97,
            format!("Matched Season {season} Episode {ep}"),
        ));
    }
    if let Some((season, ep)) = capture_i64_pair(&NXN_RE, cleaned, 1, 2) {
        return Some((season, ep, 0.96, format!("Matched {season}x{ep:02}")));
    }
    None
}

/// Match DVD / MakeMKV disc-title, VTS, and standalone title patterns.
fn match_dvd_pattern(
    cleaned: &str,
    folder_disc: Option<i64>,
) -> Option<(Option<i64>, i64, bool, f64, String)> {
    if let Some((disc, ep)) = capture_i64_pair(&DISC_TITLE_RE, cleaned, 1, 2) {
        return Some((
            Some(disc),
            ep,
            false,
            0.94,
            format!("Matched Disc {disc}, Title {ep:02}"),
        ));
    }
    if let Some((vts_title, vts_part)) = capture_i64_pair(&VTS_RE, cleaned, 1, 2) {
        return Some((
            folder_disc,
            vts_title * 100 + vts_part,
            vts_part == 0,
            0.92,
            format!("Matched DVD VTS_{vts_title:02}_{vts_part}"),
        ));
    }
    if let Some(ep) = capture_i64(&MAKEMKV_TITLE_RE, cleaned, 1) {
        let reason = match folder_disc {
            Some(d) => format!("Matched Disc {d}, MakeMKV title {ep:02}"),
            None => format!("Matched DVD title {ep:02}"),
        };
        return Some((folder_disc, ep, false, 0.91, reason));
    }
    None
}

/// Match explicit Episode/Part markers or leading/delimited numeric tokens.
fn match_fallback_number(
    cleaned: &str,
    season: Option<i64>,
    constant_sibling_nums: &HashSet<String>,
) -> Option<(i64, Option<i64>, f64, String)> {
    if let Some(parsed) = EPISODE_ONLY_RE
        .captures(cleaned)
        .and_then(|caps| caps.get(1))
        .and_then(|m| parse_int_or_roman(m.as_str()))
    {
        let reason = match season {
            Some(s) => format!("Folder Season {s} + Episode {parsed}"),
            None => format!("Matched Episode {parsed}"),
        };
        return Some((parsed, None, 0.90, reason));
    }

    let no_year = YEAR_RE.replace_all(cleaned, " ");
    if let Some(num) = LEADING_NUM_RE
        .captures(&no_year)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str())
        .filter(|tok| !constant_sibling_nums.contains(*tok))
        .and_then(|tok| tok.parse::<i64>().ok())
    {
        return Some((num, Some(num), 0.85, format!("Leading number #{num}")));
    }

    let last_match = TRAILING_OR_DELIM_NUM_RE
        .captures_iter(&no_year)
        .filter_map(|caps| caps.get(1).map(|m| m.as_str()))
        .filter(|tok| !constant_sibling_nums.contains(*tok))
        .last();

    if let Some(num) = last_match.and_then(|tok| tok.parse::<i64>().ok()) {
        return Some((
            num,
            Some(num),
            0.80,
            format!("Extracted episode number #{num}"),
        ));
    }

    None
}

fn analyze_single_file(
    root: &Path,
    file_path: &Path,
    stem: &str,
    cleaned_stem: &str,
    constant_sibling_nums: &HashSet<String>,
) -> AnalyzedEpisode {
    let rel = file_path.strip_prefix(root).unwrap_or(file_path);
    let filename = file_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let file_size = std::fs::metadata(file_path).map(|m| m.len()).unwrap_or(0);

    let (mut season, mut disc, folder_extra) = extract_folder_context(rel);
    let mut is_extra = folder_extra || EXTRA_RE.is_match(stem) || DVD_MENU_VOB_RE.is_match(stem);
    let is_pilot = PILOT_RE.is_match(stem);

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

    let resolved_path = canonicalize_clean(file_path).unwrap_or_else(|_| file_path.to_path_buf());
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

fn discover_video_files_in_root(root: &Path) -> Vec<PathBuf> {
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
        let is_file = ft.is_file() || (ft.is_symlink() && entry.path().is_file());
        if !is_file {
            continue;
        }
        let path = entry.into_path();
        let is_video = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(is_video_extension);
        if is_video {
            found.push(path);
        }
    }

    found
}

/// Recursively discover video files inside `folder_path`.
///
/// Prunes hidden directories (starting with `.`) and ignored system directories,
/// skips hidden files, and returns paths in deterministic sorted order.
pub fn discover_video_files(folder_path: &Path) -> Result<Vec<PathBuf>> {
    let root = resolve_folder(folder_path)?;
    Ok(discover_video_files_in_root(&root))
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
        .map(|vp| {
            vp.file_stem()
                .or_else(|| vp.file_name())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
        .collect();
    let cleaned_stems: Vec<String> = stems.iter().map(|s| strip_noise(s)).collect();

    // Group file indices by parent folder to identify constant show-title numbers (e.g. "24", "100")
    let mut indices_by_parent: HashMap<&Path, Vec<usize>> = HashMap::new();
    for (idx, vp) in video_files.iter().enumerate() {
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
        .map(|(idx, vp)| {
            let parent = vp.parent().unwrap_or(&root);
            let constants = constant_by_parent.get(parent).unwrap_or(&empty_set);
            analyze_single_file(&root, vp, &stems[idx], &cleaned_stems[idx], constants)
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

    // If a Pilot has no season but other episodes have seasons, align Pilot with the earliest season
    let min_season = analyzed
        .iter()
        .filter(|it| !it.is_extra)
        .filter_map(|it| it.season)
        .min();
    if let Some(min_s) = min_season {
        for it in &mut analyzed {
            if it.is_pilot && it.season.is_none() && !it.is_extra {
                it.season = Some(min_s);
            }
        }
    }

    analyzed.sort_by_cached_key(|it| {
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
    });

    Ok(analyzed)
}
