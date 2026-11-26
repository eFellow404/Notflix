"""Cross-episode TV intro detection engine.

Combines three complementary strategies with zero mandatory external dependencies:
1. Container chapter markers (pure-Python MKV/WebM EBML and MP4 ``chpl`` parsing
   via ``notflix.chapters``).
2. Server-side acoustic fingerprinting + cross-episode diagonal alignment when
   ``ffmpeg`` is available on ``PATH``.
3. Shared fingerprint consensus matching that also accepts browser-extracted
   visual frame fingerprints when ``ffmpeg`` is not installed.
"""

from __future__ import annotations

import array
import math
import operator
import shutil
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Optional, Sequence

from notflix.chapters import (
    INTRO_CHAPTER_RE,
    ChapterMarker,
    detect_intro_from_chapters,
    extract_chapters,
    extract_mkv_chapters,
    extract_mp4_chapters,
)

# Intro bounds (seconds)
MIN_INTRO_SEC = 12.0
MAX_INTRO_SEC = 180.0
DEFAULT_AUDIO_SCAN_SEC = 480.0  # First 8 minutes covers cold open + intro


@dataclass(frozen=True)
class IntroSpan:
    """Detected or manually assigned intro timestamp range for an episode."""

    start_sec: float
    end_sec: float
    confidence: float
    source: str  # "chapters", "audio", "visual", "manual"

    @property
    def duration_sec(self) -> float:
        return max(0.0, self.end_sec - self.start_sec)

    def to_dict(self) -> dict:
        return {
            "intro_start_sec": round(self.start_sec, 2),
            "intro_end_sec": round(self.end_sec, 2),
            "intro_confidence": round(self.confidence, 2),
            "intro_source": self.source,
        }


@dataclass
class EpisodeFingerprint:
    """Time-series feature vectors for the opening portion of an episode."""

    episode_id: str
    step_sec: float
    vectors: list[tuple[float, ...]]
    flat_flags: list[bool]
    duration_sec: float = 0.0
    source: str = "visual"


def _normalize_vector(raw: Sequence[float]) -> tuple[tuple[float, ...], bool]:
    """Zero-mean, unit-L2 normalize a feature vector.

    Returns ``(normalized_tuple, is_flat)``. Near-constant vectors (black frames,
    solid fades, digital silence) are flagged as flat so they never produce false
    matches across episodes.
    """
    n = len(raw)
    if n == 0:
        return (), True
    mean = sum(raw) / n
    centered = [float(v) - mean for v in raw]
    norm_sq = sum(c * c for c in centered)
    if norm_sq <= 1e-8:
        return tuple(0.0 for _ in range(n)), True
    norm = math.sqrt(norm_sq)
    return tuple(c / norm for c in centered), False


def build_fingerprint(
    episode_id: str,
    step_sec: float,
    raw_vectors: Sequence[Sequence[float]],
    flat_flags: Optional[Sequence[bool]] = None,
    duration_sec: float = 0.0,
    source: str = "visual",
    min_std: float = 0.0,
) -> EpisodeFingerprint:
    """Normalize raw per-frame vectors into an ``EpisodeFingerprint``."""
    step = max(0.25, float(step_sec))
    norm_vecs: list[tuple[float, ...]] = []
    flats: list[bool] = []
    for idx, raw in enumerate(raw_vectors):
        user_flat = bool(flat_flags[idx]) if flat_flags is not None and idx < len(flat_flags) else False
        if not raw:
            norm_vecs.append(())
            flats.append(True)
            continue
        if min_std > 0.0 and not user_flat:
            mean = sum(raw) / len(raw)
            var = sum((v - mean) ** 2 for v in raw) / len(raw)
            if math.sqrt(var) < min_std:
                user_flat = True
        vec, vec_flat = _normalize_vector(raw)
        norm_vecs.append(vec)
        flats.append(user_flat or vec_flat)

    return EpisodeFingerprint(
        episode_id=episode_id,
        step_sec=step,
        vectors=norm_vecs,
        flat_flags=flats,
        duration_sec=max(0.0, float(duration_sec)),
        source=source,
    )


# ---------------------------------------------------------------------------
# Strategy 2: Optional Server-Side FFmpeg Audio Fingerprinting
# ---------------------------------------------------------------------------

_AUDIO_SAMPLE_RATE = 4000
_GOERTZEL_FREQS = (95.0, 145.0, 220.0, 330.0, 480.0, 700.0, 1000.0, 1400.0, 1800.0)
_GOERTZEL_COEFFS: tuple[tuple[float, float], ...] = tuple(
    (freq, 2.0 * math.cos((2.0 * math.pi * freq) / _AUDIO_SAMPLE_RATE))
    for freq in _GOERTZEL_FREQS
)


def _goertzel_band_powers(
    samples: Sequence[int],
    sample_rate: int = _AUDIO_SAMPLE_RATE,
) -> list[float]:
    """Compute log-scaled spectral band energies using Goertzel filters (pure Python)."""
    n = len(samples)
    if n == 0:
        return [0.0] * len(_GOERTZEL_FREQS)

    coeffs = (
        _GOERTZEL_COEFFS
        if sample_rate == _AUDIO_SAMPLE_RATE
        else tuple((f, 2.0 * math.cos((2.0 * math.pi * f) / sample_rate)) for f in _GOERTZEL_FREQS)
    )
    powers: list[float] = []
    for _freq, coeff in coeffs:
        s1 = 0.0
        s2 = 0.0
        for x in samples:
            s1, s2 = x + coeff * s1 - s2, s1
        power = max(0.0, s1 * s1 + s2 * s2 - coeff * s1 * s2)
        powers.append(math.log1p(power / n))
    return powers


def extract_audio_fingerprint(
    file_path: Path,
    episode_id: str,
    max_sec: float = DEFAULT_AUDIO_SCAN_SEC,
    step_sec: float = 1.0,
) -> Optional[EpisodeFingerprint]:
    """Extract compact per-second spectral audio fingerprints via ``ffmpeg``.

    Returns ``None`` if ``ffmpeg`` is unavailable or decoding fails.
    """
    ffmpeg_bin = shutil.which("ffmpeg")
    if not ffmpeg_bin or not file_path.exists():
        return None

    cmd = [
        ffmpeg_bin,
        "-v",
        "error",
        "-t",
        str(int(max_sec)),
        "-i",
        str(file_path),
        "-vn",
        "-ac",
        "1",
        "-ar",
        str(_AUDIO_SAMPLE_RATE),
        "-f",
        "s16le",
        "pipe:1",
    ]
    try:
        proc = subprocess.run(cmd, capture_output=True, timeout=45, check=False)
    except (OSError, subprocess.TimeoutExpired):
        return None

    if proc.returncode != 0 or not proc.stdout:
        return None

    pcm = array.array("h")
    usable = len(proc.stdout) - (len(proc.stdout) % 2)
    pcm.frombytes(proc.stdout[:usable])
    if len(pcm) < _AUDIO_SAMPLE_RATE * 10:
        return None

    samples_per_step = max(256, int(_AUDIO_SAMPLE_RATE * step_sec))
    sub_window = min(512, samples_per_step)
    raw_vectors: list[list[float]] = []
    flat_flags: list[bool] = []

    for offset in range(0, len(pcm) - samples_per_step + 1, samples_per_step):
        # Sample two evenly spaced 512-sample chunks inside each step for speed + stability
        chunk1 = pcm[offset : offset + sub_window]
        mid = offset + max(0, (samples_per_step - sub_window) // 2)
        chunk2 = pcm[mid : mid + sub_window]

        rms = math.sqrt(sum(s * s for s in chunk1) / len(chunk1))
        if rms < 45.0:
            raw_vectors.append([0.0] * (len(_GOERTZEL_FREQS) + 2))
            flat_flags.append(True)
            continue

        b1 = _goertzel_band_powers(chunk1)
        b2 = _goertzel_band_powers(chunk2)
        bands = [(x + y) * 0.5 for x, y in zip(b1, b2)]

        # Zero-crossing rate adds percussion/speech discrimination
        zc = 0
        prev_s = chunk1[0]
        for s in chunk1[1:]:
            if (s >= 0) != (prev_s >= 0):
                zc += 1
            prev_s = s
        zcr = (zc / len(chunk1)) * 10.0

        raw_vectors.append(bands + [math.log1p(rms), zcr])
        flat_flags.append(False)

    return build_fingerprint(
        episode_id=episode_id,
        step_sec=step_sec,
        raw_vectors=raw_vectors,
        flat_flags=flat_flags,
        duration_sec=len(pcm) / float(_AUDIO_SAMPLE_RATE),
        source="audio",
    )


# ---------------------------------------------------------------------------
# Strategy 3: Cross-Episode Diagonal Alignment & Consensus Intro Matching
# ---------------------------------------------------------------------------

_NEIGHBOR_DECAY = 0.96
# Sentinel strictly below any unit-vector dot product (even after *_NEIGHBOR_DECAY)
_FLAT_SIM = -2.0


def _dot(a: tuple[float, ...], b: tuple[float, ...]) -> float:
    return sum(map(operator.mul, a, b))


@dataclass(frozen=True)
class _PairMatch:
    start_a: int
    end_a: int  # exclusive frame index
    start_b: int
    end_b: int  # exclusive frame index
    mean_sim: float
    matched_ratio: float
    score: float

    @property
    def length_frames(self) -> int:
        return self.end_a - self.start_a


def _build_jitter_sim_grid(
    fp_a: EpisodeFingerprint,
    fp_b: EpisodeFingerprint,
) -> list[Optional[list[Optional[float]]]]:
    """Precompute ±1-frame jitter-tolerant cosine similarity rows, skipping flat frames."""
    nb = len(fp_b.vectors)
    active_b = [
        (j, v)
        for j, (v, flat) in enumerate(zip(fp_b.vectors, fp_b.flat_flags))
        if not flat and v
    ]
    if not active_b:
        return [None] * len(fp_a.vectors)

    grid: list[Optional[list[Optional[float]]]] = []
    for va, flat_a in zip(fp_a.vectors, fp_a.flat_flags):
        if flat_a or not va:
            grid.append(None)
            continue

        # Pad both ends with _FLAT_SIM so ±1 neighbor lookup needs zero bounds checks
        padded = [_FLAT_SIM] * (nb + 2)
        for j, vb in active_b:
            padded[j + 1] = _dot(va, vb)

        row: list[Optional[float]] = [None] * nb
        for j, _ in active_b:
            s = padded[j + 1]
            left = padded[j] * _NEIGHBOR_DECAY
            if left > s:
                s = left
            right = padded[j + 2] * _NEIGHBOR_DECAY
            if right > s:
                s = right
            row[j] = s
        grid.append(row)

    return grid


def find_best_shared_segment(
    fp_a: EpisodeFingerprint,
    fp_b: EpisodeFingerprint,
    min_intro_sec: float = MIN_INTRO_SEC,
    max_intro_sec: float = MAX_INTRO_SEC,
    sim_threshold: float = 0.68,
) -> Optional[_PairMatch]:
    """Find the strongest contiguous matching segment between two episodes.

    Scans every diagonal offset ``d = j - i`` using a Kadane-style maximum
    subarray scan with ±1 frame neighbor tolerance to absorb sampling phase
    jitter when cold opens do not align to exact ``step_sec`` multiples.
    """
    na = len(fp_a.vectors)
    nb = len(fp_b.vectors)
    if na == 0 or nb == 0 or abs(fp_a.step_sec - fp_b.step_sec) > 0.05:
        return None

    step = fp_a.step_sec
    min_frames = max(4, int(round(min_intro_sec / step)))
    max_frames = max(min_frames, int(round(max_intro_sec / step)))
    if na < min_frames or nb < min_frames:
        return None

    sim_grid = _build_jitter_sim_grid(fp_a, fp_b)

    best: Optional[_PairMatch] = None
    baseline = sim_threshold - 0.12
    mismatch_penalty = -0.70
    flat_penalty = -0.10
    min_mean_sim = max(sim_threshold, 0.72)

    # Iterate over all diagonal offsets d = j - i with enough overlap
    for d in range(-(na - min_frames), nb - min_frames + 1):
        i_start = max(0, -d)
        i_end = min(na, nb - d)
        if i_end - i_start < min_frames:
            continue

        cur_score = 0.0
        seg_start = i_start
        best_diag_score = 0.0
        best_diag_range: Optional[tuple[int, int]] = None

        for i in range(i_start, i_end):
            row = sim_grid[i]
            s = None if row is None else row[i + d]
            if s is None:
                weight = flat_penalty
            elif s >= sim_threshold:
                weight = s - baseline
            else:
                weight = mismatch_penalty

            if cur_score + weight <= 0.0:
                cur_score = 0.0
                seg_start = i + 1
            else:
                cur_score += weight
                seg_len = (i + 1) - seg_start
                if seg_len > max_frames:
                    # Cap runaway segments to max_frames by keeping the strongest tail
                    seg_start = (i + 1) - max_frames
                if seg_len >= min_frames and cur_score > best_diag_score:
                    best_diag_score = cur_score
                    best_diag_range = (seg_start, i + 1)

        if best_diag_range is None:
            continue

        a0, a1 = best_diag_range

        # Trim leading/trailing flat or sub-threshold frames for crisp edges
        while a0 < a1:
            row = sim_grid[a0]
            if row is not None and (s := row[a0 + d]) is not None and s >= sim_threshold:
                break
            a0 += 1
        while a1 > a0:
            row = sim_grid[a1 - 1]
            if row is not None and (s := row[a1 - 1 + d]) is not None and s >= sim_threshold:
                break
            a1 -= 1

        seg_len = a1 - a0
        if seg_len < min_frames or seg_len > max_frames:
            continue

        sim_sum = 0.0
        sim_count = 0
        matched = 0
        for ii in range(a0, a1):
            row = sim_grid[ii]
            if row is None:
                continue
            s = row[ii + d]
            if s is not None:
                sim_sum += s
                sim_count += 1
                if s >= sim_threshold:
                    matched += 1

        if sim_count == 0:
            continue
        matched_ratio = matched / seg_len
        mean_sim = sim_sum / sim_count
        if matched_ratio < 0.65 or mean_sim < min_mean_sim:
            continue

        quality_score = mean_sim * matched_ratio * (seg_len * step)
        candidate = _PairMatch(
            start_a=a0,
            end_a=a1,
            start_b=a0 + d,
            end_b=a1 + d,
            mean_sim=mean_sim,
            matched_ratio=matched_ratio,
            score=quality_score,
        )
        if best is None or candidate.score > best.score:
            best = candidate

    return best


def _align_episode_to_template(
    ep_fp: EpisodeFingerprint,
    template_vecs: Sequence[tuple[float, ...]],
    template_flats: Sequence[bool],
    sim_threshold: float = 0.66,
) -> Optional[IntroSpan]:
    """Slide a consensus intro template across one episode's fingerprint."""
    n = len(ep_fp.vectors)
    t_len = len(template_vecs)
    if n < t_len or t_len == 0:
        return None

    active_template = [
        (k, tv)
        for k, (tv, flat) in enumerate(zip(template_vecs, template_flats))
        if not flat and tv
    ]
    num_active = len(active_template)
    if num_active < max(4, int(t_len * 0.4)):
        return None

    ep_vecs: list[Optional[tuple[float, ...]]] = [
        v if (not flat and v) else None
        for v, flat in zip(ep_fp.vectors, ep_fp.flat_flags)
    ]
    active_ep = [(j, v) for j, v in enumerate(ep_vecs) if v is not None]
    if not active_ep:
        return None

    num_offsets = n - t_len + 1
    matched = [0] * num_offsets
    sim_sum = [0.0] * num_offsets
    compared = [0] * num_offsets

    # Compute each template frame's dot products once and accumulate across offsets
    for k, tv in active_template:
        padded = [_FLAT_SIM] * (n + 2)
        for j, ev in active_ep:
            padded[j + 1] = _dot(tv, ev)
        for offset in range(num_offsets):
            j = offset + k
            s = padded[j + 1]
            left = padded[j] * _NEIGHBOR_DECAY
            if left > s:
                s = left
            right = padded[j + 2] * _NEIGHBOR_DECAY
            if right > s:
                s = right
            if s >= -0.5:
                compared[offset] += 1
                sim_sum[offset] += s
                if s >= sim_threshold:
                    matched[offset] += 1

    best_offset = -1
    best_score = -1.0
    best_mean = 0.0
    best_ratio = 0.0
    for offset in range(num_offsets):
        comp = compared[offset]
        if comp == 0:
            continue
        ratio = matched[offset] / num_active
        mean_s = sim_sum[offset] / comp
        score = ratio * mean_s
        if score > best_score:
            best_score = score
            best_offset = offset
            best_mean = mean_s
            best_ratio = ratio

    if best_offset < 0 or best_ratio < 0.58 or best_mean < max(sim_threshold, 0.70):
        return None

    # Trim unmatched leading/trailing frames cleanly
    trim_threshold = sim_threshold - 0.04
    trim_start = 0
    trim_end = t_len
    while trim_start < trim_end:
        ev = ep_vecs[best_offset + trim_start]
        if (
            not template_flats[trim_start]
            and ev is not None
            and template_vecs[trim_start]
            and _dot(template_vecs[trim_start], ev) >= trim_threshold
        ):
            break
        trim_start += 1
    while trim_end > trim_start:
        k = trim_end - 1
        ev = ep_vecs[best_offset + k]
        if (
            not template_flats[k]
            and ev is not None
            and template_vecs[k]
            and _dot(template_vecs[k], ev) >= trim_threshold
        ):
            break
        trim_end -= 1

    start_idx = best_offset + trim_start
    end_idx = best_offset + trim_end
    if end_idx <= start_idx:
        start_idx = best_offset
        end_idx = best_offset + t_len

    start_sec = start_idx * ep_fp.step_sec
    end_sec = end_idx * ep_fp.step_sec
    if end_sec - start_sec < MIN_INTRO_SEC * 0.8:
        return None

    confidence = min(0.99, max(0.50, (best_mean * 0.65) + (best_ratio * 0.35)))
    return IntroSpan(
        start_sec=start_sec,
        end_sec=end_sec,
        confidence=confidence,
        source=ep_fp.source,
    )


def find_shared_intros(
    fingerprints: Sequence[EpisodeFingerprint],
    min_intro_sec: float = MIN_INTRO_SEC,
    max_intro_sec: float = MAX_INTRO_SEC,
    sim_threshold: float = 0.68,
    max_reference_episodes: int = 6,
) -> dict[str, IntroSpan]:
    """Find the repeating intro sequence across a show's episodes.

    1. Compares pairs among up to ``max_reference_episodes`` episodes to locate
       the consensus intro template (robust to Pilot episodes with no intro).
    2. Aligns every episode in ``fingerprints`` against that consensus template
       to compute per-episode ``(intro_start_sec, intro_end_sec)`` spans.
    """
    valid = [fp for fp in fingerprints if len(fp.vectors) >= 4]
    if len(valid) < 2:
        return {}

    refs = valid[: max(2, min(len(valid), max_reference_episodes))]

    # Collect pairwise matches keyed by candidate (anchor_id, start, end)
    candidates: list[tuple[EpisodeFingerprint, int, int, float, int]] = []
    pair_matches: dict[tuple[int, int], _PairMatch] = {}

    for i in range(len(refs)):
        for j in range(i + 1, len(refs)):
            match = find_best_shared_segment(
                refs[i],
                refs[j],
                min_intro_sec=min_intro_sec,
                max_intro_sec=max_intro_sec,
                sim_threshold=sim_threshold,
            )
            if match is not None:
                pair_matches[(i, j)] = match

    if not pair_matches:
        return {}

    # Score each matched segment by how many other reference episodes agree with it
    for (i, j), m in pair_matches.items():
        # Count supporting matches overlapping [m.start_a, m.end_a] on refs[i]
        support = 1
        total_score = m.score
        for (u, v), other in pair_matches.items():
            if (u, v) == (i, j):
                continue
            if u == i:
                ov = min(m.end_a, other.end_a) - max(m.start_a, other.start_a)
                union = max(m.end_a, other.end_a) - min(m.start_a, other.start_a)
                if union > 0 and (ov / union) >= 0.5:
                    support += 1
                    total_score += other.score
            elif v == i:
                ov = min(m.end_a, other.end_b) - max(m.start_a, other.start_b)
                union = max(m.end_a, other.end_b) - min(m.start_a, other.start_b)
                if union > 0 and (ov / union) >= 0.5:
                    support += 1
                    total_score += other.score
        candidates.append((refs[i], m.start_a, m.end_a, total_score, support))

    # Choose the anchor template with highest multi-episode support, then total score
    candidates.sort(key=lambda item: (item[4], item[3]), reverse=True)
    anchor_fp, tmpl_start, tmpl_end, _, _ = candidates[0]

    template_vecs = anchor_fp.vectors[tmpl_start:tmpl_end]
    template_flats = anchor_fp.flat_flags[tmpl_start:tmpl_end]

    results: dict[str, IntroSpan] = {}
    for fp in valid:
        span = _align_episode_to_template(
            fp,
            template_vecs=template_vecs,
            template_flats=template_flats,
            sim_threshold=sim_threshold - 0.02,
        )
        if span is not None:
            results[fp.episode_id] = span

    return results


def detect_show_intros_server(
    episodes: Sequence[tuple[str, Path, float]],
    use_ffmpeg: bool = True,
) -> tuple[dict[str, IntroSpan], str, bool]:
    """Run server-side intro detection for a show's episodes.

    Args:
        episodes: Sequence of ``(episode_id, file_path, duration_sec)`` tuples.
        use_ffmpeg: Whether to attempt ``ffmpeg`` audio fingerprinting if chapters
            do not cover the show.

    Returns:
        ``(detected_map, method_used, needs_client_visual_scan)``
    """
    detected: dict[str, IntroSpan] = {}

    # Layer 1: Fast container chapter markers (works with zero external dependencies)
    for ep_id, path, dur in episodes:
        if not path.exists():
            continue
        chapters = extract_chapters(path)
        span = detect_intro_from_chapters(chapters, duration_sec=dur)
        if span is not None:
            detected[ep_id] = span

    if len(detected) >= max(1, len(episodes) // 2) and detected:
        return detected, "chapters", False

    if len(episodes) < 2:
        return detected, ("chapters" if detected else "insufficient_episodes"), False

    # Layer 2: FFmpeg acoustic fingerprinting when installed
    if use_ffmpeg and shutil.which("ffmpeg") is not None:
        fps: list[EpisodeFingerprint] = []
        for ep_id, path, _ in episodes:
            fp = extract_audio_fingerprint(path, episode_id=ep_id)
            if fp is not None:
                fps.append(fp)
        if len(fps) >= 2:
            audio_matches = find_shared_intros(fps)
            for ep_id, span in audio_matches.items():
                # Keep explicit named chapter matches if already found
                detected.setdefault(ep_id, span)
            if audio_matches:
                return detected, "audio", False

    # Layer 3: Signal browser to run zero-dependency visual frame fingerprinting
    return detected, ("chapters" if detected else "visual_needed"), True
