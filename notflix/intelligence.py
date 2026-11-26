"""Intelligent scanner and episode ordering engine for DVD-ripped TV folders."""

from __future__ import annotations

import os
import re
from collections import Counter, defaultdict
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

VIDEO_EXTENSIONS: frozenset[str] = frozenset(
    {
        ".mp4",
        ".mkv",
        ".m4v",
        ".webm",
        ".avi",
        ".mov",
        ".mpg",
        ".mpeg",
        ".vob",
        ".ts",
        ".m2ts",
        ".ogv",
        ".wmv",
        ".flv",
    }
)

IGNORED_DIRS: frozenset[str] = frozenset(
    {"__MACOSX", "@eaDir", ".Trashes", "$RECYCLE.BIN", ".git", "AUDIO_TS"}
)

EXTRA_RE = re.compile(
    r"(?:^|[\s._\-/(\[])(?:"
    r"extras?|bonus|featurettes?|behind[\s._-]?the[\s._-]?scenes|"
    r"deleted[\s._-]?scenes?|bloopers?|gag[\s._-]?reels?|outtakes?|"
    r"trailers?|promos?|commentary|making[\s._-]?of|interviews?|"
    r"special[\s._-]?features?|dvd[\s._-]?menu|menus?|sample"
    r")(?:$|[\s._\-/)\]])",
    re.IGNORECASE,
)

PILOT_RE = re.compile(r"(?:^|[\s._\-(\[])(?:pilot|prologue)(?:$|[\s._\-)\]])", re.IGNORECASE)

# DVD menu VOBs like VIDEO_TS.VOB or VTS_01_0.VOB
DVD_MENU_VOB_RE = re.compile(r"^(?:video_ts|vts_\d+_0)$", re.IGNORECASE)

# Noise tokens removed prior to fallback numeric parsing
CRC_BRACKET_RE = re.compile(r"[\[\(][0-9a-fA-F]{6,10}[\]\)]")
RESOLUTION_RE = re.compile(r"\b(?:2160|1440|1080|720|576|480|360)[pi]\b|\b(?:4k|uhd|fhd)\b", re.IGNORECASE)
CODEC_RE = re.compile(
    r"\b(?:x264|x265|h\.?264|h\.?265|hevc|av1|xvid|divx|mpeg[24]?|"
    r"ac3|aac|dts(?:-hd)?|ddp?5\.?1|dd5\.?1|5\.1|7\.1|2\.0|10bit|8bit|"
    r"dvdrip|bdrip|brrip|bluray|blu-ray|webrip|web-dl|webdl|hdtv|remux|"
    r"proper|repack|ntsc|pal|uncut|extended|directors?[\s._-]?cut)\b",
    re.IGNORECASE,
)
YEAR_RE = re.compile(r"(?:^|[\s._\-(\[])(?:19[5-9]\d|20[0-3]\d)(?:$|[\s._\-)\]])")

# Folder context patterns
FOLDER_SEASON_RE = re.compile(
    r"(?:^|[\s._\-])(?:season|series|book|volume|vol\.?|year|s)[\s._\-]?(\d{1,3})(?:$|[\s._\-])",
    re.IGNORECASE,
)
FOLDER_DISC_RE = re.compile(
    r"(?:^|[\s._\-])(?:disc|disk|dvd|cd|d)[\s._\-]?(\d{1,3}|[a-d])(?:$|[\s._\-])",
    re.IGNORECASE,
)

# Ranked filename patterns
SXXEXX_RE = re.compile(
    r"(?:^|[\s._\-(\[])s(?:eason)?[\s._\-]?(\d{1,3})[\s._\-]*e(?:p(?:isode)?)?[\s._\-]?(\d{1,4})(?:[\s._\-]*-?e?(\d{1,4}))?(?:$|[\s._\-)\]])",
    re.IGNORECASE,
)
NXN_RE = re.compile(
    r"(?:^|[\s._\-(\[])(\d{1,2})x(\d{1,3})(?:$|[\s._\-)\]])",
    re.IGNORECASE,
)
VERBOSE_SE_RE = re.compile(
    r"season[\s._\-]+(\d{1,3})[\s._\-]+(?:ep(?:isode)?|part|pt)[\s._\-]+(\d{1,4})",
    re.IGNORECASE,
)
DISC_TITLE_RE = re.compile(
    r"(?:disc|disk|dvd|d)[\s._\-]?(\d{1,2})[\s._\-]*(?:title|track|tr|ep|t)[\s._\-]?(\d{1,3})",
    re.IGNORECASE,
)
MAKEMKV_TITLE_RE = re.compile(
    r"(?:^|[\s._\-])(?:title|track|chapter|clip|t)[\s._\-]?t?(\d{1,3})(?:$|[\s._\-])",
    re.IGNORECASE,
)
VTS_RE = re.compile(r"\bvts[\s._\-]?(\d{1,2})[\s._\-]?(\d{1,2})\b", re.IGNORECASE)
EPISODE_ONLY_RE = re.compile(
    r"(?:^|[\s._\-(\[])(?:episode|ep\.?|e|part|pt\.?|#|no\.?)[\s._\-]?(\d{1,4}|[ivxlc]{1,5})(?:$|[\s._\-)\]])",
    re.IGNORECASE,
)
LEADING_NUM_RE = re.compile(r"^[\s._\-]*(\d{1,4})(?:[\s._\-]+|$)")
TRAILING_OR_DELIM_NUM_RE = re.compile(r"(?:^|[\s._\-(\[])(\d{1,4})(?:$|[\s._\-)\]])")

ROMAN_MAP = {
    "i": 1,
    "ii": 2,
    "iii": 3,
    "iv": 4,
    "v": 5,
    "vi": 6,
    "vii": 7,
    "viii": 8,
    "ix": 9,
    "x": 10,
    "xi": 11,
    "xii": 12,
    "xiii": 13,
    "xiv": 14,
    "xv": 15,
    "xvi": 16,
    "xvii": 17,
    "xviii": 18,
    "xix": 19,
    "xx": 20,
}


@dataclass
class AnalyzedEpisode:
    """Intermediate representation produced by the intelligent file analyzer."""

    file_path: str
    relative_path: str
    filename: str
    parent_rel: str
    file_size: int
    title: str
    season: Optional[int] = None
    episode_number: Optional[int] = None
    disc: Optional[int] = None
    is_extra: bool = False
    is_pilot: bool = False
    confidence: float = 0.0
    sort_reason: str = ""
    raw_numeric_candidate: Optional[int] = None
    explicit_season_episode: bool = False


def natural_sort_key(text: str) -> list:
    """Split text into lowercase string and integer chunks for natural ordering."""
    return [int(tok) if tok.isdigit() else tok.lower() for tok in re.split(r"(\d+)", text)]


def _parse_int_or_roman(token: str) -> Optional[int]:
    t = token.strip().lower()
    if t.isdigit():
        return int(t)
    return ROMAN_MAP.get(t)


def _parse_disc_token(token: str) -> Optional[int]:
    t = token.strip().lower()
    if t.isdigit():
        return int(t)
    if len(t) == 1 and "a" <= t <= "z":
        return ord(t) - ord("a") + 1
    return None


def _strip_noise(stem: str) -> str:
    cleaned = CRC_BRACKET_RE.sub(" ", stem)
    cleaned = RESOLUTION_RE.sub(" ", cleaned)
    cleaned = CODEC_RE.sub(" ", cleaned)
    return cleaned


def _extract_folder_context(rel_parts: tuple[str, ...]) -> tuple[Optional[int], Optional[int], bool]:
    season: Optional[int] = None
    disc: Optional[int] = None
    is_extra = False

    # Inspect directory names only (excluding filename)
    for part in rel_parts[:-1]:
        if EXTRA_RE.search(part):
            is_extra = True
        m_season = FOLDER_SEASON_RE.search(part)
        if m_season:
            season = int(m_season.group(1))
        m_disc = FOLDER_DISC_RE.search(part)
        if m_disc:
            disc = _parse_disc_token(m_disc.group(1))
        # Special case: folder literally named "S1", "S02", or "1", "2" when nested
        if season is None and re.fullmatch(r"s0?(\d{1,2})", part.strip(), re.IGNORECASE):
            season = int(part.strip()[1:])

    return season, disc, is_extra


def _clean_display_title(
    stem: str,
    season: Optional[int],
    episode_number: Optional[int],
    disc: Optional[int],
    is_extra: bool,
) -> str:
    cleaned = _strip_noise(stem)
    # Remove explicit S01E02 / 1x02 / leading number prefixes if meaningful words follow
    stripped = SXXEXX_RE.sub(" ", cleaned)
    stripped = NXN_RE.sub(" ", stripped)
    stripped = VERBOSE_SE_RE.sub(" ", stripped)
    stripped = re.sub(r"[._]+", " ", stripped)
    stripped = re.sub(r"^\s*[-–—:]+\s*", "", stripped)
    stripped = re.sub(r"\s*[-–—:]+\s*$", "", stripped)
    stripped = re.sub(r"\s{2,}", " ", stripped).strip()

    # If the stripped title still has descriptive words (not just bare numbers), use it
    if stripped and re.search(r"[a-zA-Z]{2,}", stripped):
        # Remove leading bare episode number like "01 - Pilot" -> "Pilot"
        m = re.match(r"^\d{1,4}\s*[-–—:\s]\s*(.+)$", stripped)
        if m and re.search(r"[a-zA-Z]{2,}", m.group(1)):
            stripped = m.group(1).strip()
        return stripped

    # Otherwise synthesize a neat human-readable title
    if is_extra:
        return re.sub(r"[._]+", " ", stem).strip() or "Bonus Feature"
    if season is not None and episode_number is not None:
        return f"Season {season}, Episode {episode_number}"
    if disc is not None and episode_number is not None:
        return f"Disc {disc}, Title {episode_number:02d}"
    if episode_number is not None:
        return f"Episode {episode_number}"
    return re.sub(r"[._]+", " ", stem).strip() or stem


def _find_constant_sibling_numbers(stems: list[str]) -> set[str]:
    """Identify numeric tokens that appear identically across >=75% of siblings.

    Prevents show titles containing numbers (e.g. '24', 'The 100', 'Brooklyn 99',
    or a series year like '2004') from being mistaken for episode numbers.
    """
    if len(stems) < 2:
        return set()
    token_doc_freq: Counter[str] = Counter()
    for stem in stems:
        cleaned = _strip_noise(stem)
        unique_nums = set(re.findall(r"\d+", cleaned))
        for num in unique_nums:
            token_doc_freq[num] += 1

    threshold = max(2, int(len(stems) * 0.75 + 0.999))
    return {num for num, freq in token_doc_freq.items() if freq >= threshold}


def _extract_sibling_diff_numbers(items: list[AnalyzedEpisode]) -> None:
    """For unresolved siblings sharing a naming template, extract the varying number."""
    by_parent: dict[str, list[AnalyzedEpisode]] = defaultdict(list)
    for item in items:
        by_parent[item.parent_rel].append(item)

    for siblings in by_parent.values():
        if len(siblings) < 2:
            continue
        stems = [Path(s.filename).stem for s in siblings]
        constant_nums = _find_constant_sibling_numbers(stems)

        for item, stem in zip(siblings, stems):
            if item.explicit_season_episode or item.episode_number is not None:
                continue

            cleaned = YEAR_RE.sub(" ", _strip_noise(stem))
            candidates = [
                tok for tok in re.findall(r"\d{1,4}", cleaned) if tok not in constant_nums
            ]
            if candidates:
                # Prefer the last non-constant numeric token if multiple exist (e.g. "Show 2004 Ep 03")
                chosen = int(candidates[-1])
                item.episode_number = chosen
                item.raw_numeric_candidate = chosen
                item.confidence = max(item.confidence, 0.84)
                item.sort_reason = f"Sibling diff token #{chosen}"


def _apply_three_digit_see_consensus(items: list[AnalyzedEpisode]) -> None:
    """Detect whether 3-digit episode numbers (101, 102, 201, 202) encode Season+Episode."""
    candidates = [
        it
        for it in items
        if not it.explicit_season_episode
        and not it.is_extra
        and it.raw_numeric_candidate is not None
        and 101 <= it.raw_numeric_candidate <= 2999
    ]
    if not candidates:
        return

    hundreds_groups: dict[int, list[int]] = defaultdict(list)
    folder_matches = 0
    for it in candidates:
        assert it.raw_numeric_candidate is not None
        h = it.raw_numeric_candidate // 100
        ep = it.raw_numeric_candidate % 100
        if 1 <= ep <= 45:
            hundreds_groups[h].append(ep)
            if it.season is not None and it.season == h:
                folder_matches += 1

    # Valid SEE pattern if:
    # (a) multiple seasons are represented and episode offsets stay in a realistic season range, OR
    # (b) numbers consistently match their enclosing Season folder (e.g. Season 2/201.mp4).
    multi_season = (
        len(hundreds_groups) >= 2
        and sum(len(v) for v in hundreds_groups.values()) == len(candidates)
        and all(min(eps) <= 5 for eps in hundreds_groups.values())
    )
    folder_confirmed = folder_matches >= max(1, len(candidates) // 2) and folder_matches > 0

    if not (multi_season or folder_confirmed):
        return

    for it in candidates:
        assert it.raw_numeric_candidate is not None
        s = it.raw_numeric_candidate // 100
        e = it.raw_numeric_candidate % 100
        if 1 <= e <= 45:
            it.season = s
            it.episode_number = e
            it.confidence = max(it.confidence, 0.88)
            it.sort_reason = f"Inferred S{s:02d}E{e:02d} from {it.raw_numeric_candidate} numbering"


def _analyze_single_file(
    root: Path,
    file_path: Path,
    constant_sibling_nums: set[str],
) -> AnalyzedEpisode:
    rel = file_path.relative_to(root)
    rel_posix = rel.as_posix()
    parent_rel = rel.parent.as_posix()
    filename = file_path.name
    stem = file_path.stem
    try:
        file_size = file_path.stat().st_size
    except OSError:
        file_size = 0

    folder_season, folder_disc, folder_extra = _extract_folder_context(rel.parts)
    is_extra = folder_extra or bool(EXTRA_RE.search(stem)) or bool(DVD_MENU_VOB_RE.match(stem))
    is_pilot = bool(PILOT_RE.search(stem))

    season = folder_season
    disc = folder_disc
    episode_number: Optional[int] = None
    confidence = 0.45
    sort_reason = "Natural filename order"
    raw_numeric: Optional[int] = None
    explicit_se = False

    cleaned = _strip_noise(stem)

    # 1. Explicit S01E02 / Season 1 Episode 2 / 1x02
    m = SXXEXX_RE.search(cleaned)
    if m:
        season = int(m.group(1))
        episode_number = int(m.group(2))
        confidence = 0.98
        sort_reason = f"Matched S{season:02d}E{episode_number:02d}"
        explicit_se = True
    else:
        m = VERBOSE_SE_RE.search(cleaned)
        if m:
            season = int(m.group(1))
            episode_number = int(m.group(2))
            confidence = 0.97
            sort_reason = f"Matched Season {season} Episode {episode_number}"
            explicit_se = True
        else:
            m = NXN_RE.search(cleaned)
            if m:
                season = int(m.group(1))
                episode_number = int(m.group(2))
                confidence = 0.96
                sort_reason = f"Matched {season}x{episode_number:02d}"
                explicit_se = True

    # 2. DVD / MakeMKV Disc + Title / VTS patterns
    if episode_number is None:
        m = DISC_TITLE_RE.search(cleaned)
        if m:
            disc = int(m.group(1))
            episode_number = int(m.group(2))
            confidence = 0.94
            sort_reason = f"Matched Disc {disc}, Title {episode_number:02d}"
        else:
            m = VTS_RE.search(cleaned)
            if m:
                vts_title = int(m.group(1))
                vts_part = int(m.group(2))
                if vts_part == 0:
                    is_extra = True
                episode_number = vts_title * 100 + vts_part
                confidence = 0.92
                sort_reason = f"Matched DVD VTS_{vts_title:02d}_{vts_part}"
            else:
                m = MAKEMKV_TITLE_RE.search(cleaned)
                if m:
                    episode_number = int(m.group(1))
                    confidence = 0.91
                    if disc is not None:
                        sort_reason = f"Matched Disc {disc}, MakeMKV title {episode_number:02d}"
                    else:
                        sort_reason = f"Matched DVD title {episode_number:02d}"

    # 3. Explicit Episode / Part / # markers
    if episode_number is None:
        m = EPISODE_ONLY_RE.search(cleaned)
        if m:
            parsed = _parse_int_or_roman(m.group(1))
            if parsed is not None:
                episode_number = parsed
                confidence = 0.90
                if season is not None:
                    sort_reason = f"Folder Season {season} + Episode {episode_number}"
                else:
                    sort_reason = f"Matched Episode {episode_number}"

    # 4. Leading or delimited numeric token (filtering constant sibling numbers and years)
    if episode_number is None:
        no_year = YEAR_RE.sub(" ", cleaned)
        m_lead = LEADING_NUM_RE.match(no_year)
        if m_lead and m_lead.group(1) not in constant_sibling_nums:
            num = int(m_lead.group(1))
            episode_number = num
            raw_numeric = num
            confidence = 0.85
            sort_reason = f"Leading number #{num}"
        else:
            matches = [
                tok
                for tok in TRAILING_OR_DELIM_NUM_RE.findall(no_year)
                if tok not in constant_sibling_nums
            ]
            if matches:
                num = int(matches[-1])
                episode_number = num
                raw_numeric = num
                confidence = 0.80
                sort_reason = f"Extracted episode number #{num}"

    # 5. Pilot priority if unnumbered
    if episode_number is None and is_pilot and not is_extra:
        episode_number = 0
        confidence = 0.82
        sort_reason = "Detected Pilot episode"

    if is_extra and confidence < 0.70:
        sort_reason = "Detected Extra / Bonus feature"
        confidence = 0.85

    title = _clean_display_title(stem, season, episode_number, disc, is_extra)

    return AnalyzedEpisode(
        file_path=str(file_path.resolve()),
        relative_path=rel_posix,
        filename=filename,
        parent_rel=parent_rel,
        file_size=file_size,
        title=title,
        season=season,
        episode_number=episode_number,
        disc=disc,
        is_extra=is_extra,
        is_pilot=is_pilot,
        confidence=confidence,
        sort_reason=sort_reason,
        raw_numeric_candidate=raw_numeric,
        explicit_season_episode=explicit_se,
    )


def discover_video_files(folder_path: str | Path) -> list[Path]:
    """Recursively discover video files inside ``folder_path``."""
    root = Path(folder_path).expanduser().resolve()
    if not root.exists() or not root.is_dir():
        raise FileNotFoundError(f"Folder does not exist: {folder_path}")

    found: list[Path] = []
    for dirpath, dirnames, filenames in os.walk(root):
        # Prune hidden and system directories
        dirnames[:] = sorted(
            d for d in dirnames if not d.startswith(".") and d not in IGNORED_DIRS
        )
        for fname in filenames:
            if fname.startswith("."):
                continue
            p = Path(dirpath) / fname
            if p.suffix.lower() in VIDEO_EXTENSIONS:
                found.append(p)
    return found


def scan_and_order_folder(folder_path: str | Path) -> list[AnalyzedEpisode]:
    """Scan a TV show folder and return episodes intelligently ordered."""
    root = Path(folder_path).expanduser().resolve()
    video_files = discover_video_files(root)
    if not video_files:
        return []

    # Group files by parent folder to identify constant show-title numbers (e.g. "24", "100")
    by_parent: dict[Path, list[Path]] = defaultdict(list)
    for vp in video_files:
        by_parent[vp.parent].append(vp)

    constant_by_parent: dict[Path, set[str]] = {
        parent: _find_constant_sibling_numbers([p.stem for p in paths])
        for parent, paths in by_parent.items()
    }

    analyzed = [
        _analyze_single_file(root, vp, constant_by_parent[vp.parent])
        for vp in video_files
    ]

    # Folder-level consensus passes
    _extract_sibling_diff_numbers(analyzed)
    _apply_three_digit_see_consensus(analyzed)

    # Refresh synthesized titles if consensus upgraded season/episode info
    for item in analyzed:
        item.title = _clean_display_title(
            Path(item.filename).stem,
            item.season,
            item.episode_number,
            item.disc,
            item.is_extra,
        )

    # If a Pilot has no season but other episodes have seasons, align Pilot with the earliest season
    known_seasons = [it.season for it in analyzed if it.season is not None and not it.is_extra]
    if known_seasons:
        min_season = min(known_seasons)
        for it in analyzed:
            if it.is_pilot and it.season is None and not it.is_extra:
                it.season = min_season

    def sort_key(it: AnalyzedEpisode) -> tuple:
        extra_rank = 1 if it.is_extra else 0
        # Presence tuples ensure numbered seasons/discs/episodes sort cleanly before unknowns
        season_key = (0, it.season) if it.season is not None else (1, 0)
        disc_key = (0, it.disc) if it.disc is not None else (1, 0)
        ep_key = (0, it.episode_number) if it.episode_number is not None else (1, 0)
        return (
            extra_rank,
            season_key,
            disc_key,
            ep_key,
            natural_sort_key(it.relative_path),
        )

    analyzed.sort(key=sort_key)
    return analyzed
