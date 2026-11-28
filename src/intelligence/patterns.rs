//! Regex patterns and single-file filename/folder matchers for episode ordering.

use std::collections::HashSet;
use std::path::{Component, Path};
use std::sync::LazyLock;

use regex::Regex;

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

pub(super) static YEAR_RE: LazyLock<Regex> = LazyLock::new(|| {
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
    Regex::new(
        r"(?i)(?:disc|disk|dvd|d)[\s._\-]?(\d{1,2})[\s._\-]*(?:title|track|tr|ep|t)[\s._\-]?(\d{1,3})",
    )
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
pub(super) static DIGITS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d+").expect("valid DIGITS_RE"));

pub(super) static SHORT_NUM_RE: LazyLock<Regex> =
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

pub(super) fn is_extra_stem(stem: &str) -> bool {
    EXTRA_RE.is_match(stem) || DVD_MENU_VOB_RE.is_match(stem)
}

pub(super) fn is_pilot_stem(stem: &str) -> bool {
    PILOT_RE.is_match(stem)
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

pub(super) fn strip_noise(stem: &str) -> String {
    let cleaned = CRC_BRACKET_RE.replace_all(stem, " ");
    let cleaned = RESOLUTION_RE.replace_all(&cleaned, " ");
    CODEC_RE.replace_all(&cleaned, " ").into_owned()
}

pub(super) fn extract_folder_context(rel_path: &Path) -> (Option<i64>, Option<i64>, bool) {
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

pub(super) fn clean_display_title(
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

/// Match explicit S01E02 / Season 1 Episode 2 / 1x02 markers.
pub(super) fn match_explicit_se(cleaned: &str) -> Option<(i64, i64, f64, String)> {
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
pub(super) fn match_dvd_pattern(
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
pub(super) fn match_fallback_number(
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
