//! Cross-episode diagonal alignment and consensus intro matching.

use std::collections::HashMap;

use super::{EpisodeFingerprint, IntroSpan, MAX_INTRO_SEC, MIN_INTRO_SEC};

const NEIGHBOR_DECAY: f64 = 0.88;
/// Sentinel strictly below any unit-vector dot product (even after `* NEIGHBOR_DECAY`).
const FLAT_SIM: f64 = -2.0;
/// Sentinel representing an inactive/flat pair cell in the flat similarity matrix.
const NO_SIM: f64 = f64::NEG_INFINITY;
/// Maximum contiguous mismatch duration (seconds) allowed inside a single intro sequence.
/// Any longer mismatch gap (e.g. real episode cold-open content between a studio logo and
/// the theme song) forces a hard split so real content is never bridged and skipped.
const MAX_INTERIOR_MISMATCH_SEC: f64 = 3.0;

#[inline]
fn max_gap_debt_for_step(step_sec: f64) -> usize {
    let max_mismatch_frames = ((MAX_INTERIOR_MISMATCH_SEC / step_sec).round() as usize).max(2);
    max_mismatch_frames * 2
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameMatchKind {
    Match,
    Flat,
    Mismatch,
}

/// Extract the strongest contiguous matching sub-run from `[0, len)` without ever bridging
/// an interior gap of real non-matching content exceeding `max_gap_debt`.
///
/// Uses a leaky gap counter (`+2` per mismatch, `+1` per flat/black frame, `-1` per match)
/// so even a real-content gap containing isolated accidental matches still triggers a split,
/// while 1–2s glitches or brief title cards inside a true intro remain intact.
fn best_contiguous_subrun<FKind, FWeight>(
    len: usize,
    max_gap_debt: usize,
    kind_at: FKind,
    weight_at: FWeight,
) -> Option<(usize, usize)>
where
    FKind: Fn(usize) -> FrameMatchKind,
    FWeight: Fn(usize) -> f64,
{
    let mut best: Option<(usize, usize, f64)> = None;
    let mut run_start: Option<usize> = None;
    let mut run_end = 0usize;
    let mut run_score = 0.0_f64;
    let mut gap_debt = 0usize;

    let mut commit_run = |start: Option<usize>, end: usize, score: f64| {
        if let Some(s) = start {
            if end > s && !best.is_some_and(|b| score <= b.2) {
                best = Some((s, end, score));
            }
        }
    };

    for k in 0..len {
        match kind_at(k) {
            FrameMatchKind::Match => {
                if run_start.is_none() {
                    run_start = Some(k);
                    run_score = 0.0;
                    gap_debt = 0;
                } else {
                    gap_debt = gap_debt.saturating_sub(1);
                }
                run_end = k + 1;
                run_score += weight_at(k);
            }
            FrameMatchKind::Flat => {
                if run_start.is_some() {
                    gap_debt += 1;
                    if gap_debt > max_gap_debt {
                        commit_run(run_start, run_end, run_score);
                        run_start = None;
                        run_score = 0.0;
                        gap_debt = 0;
                    }
                }
            }
            FrameMatchKind::Mismatch => {
                if run_start.is_some() {
                    gap_debt += 2;
                    if gap_debt > max_gap_debt {
                        commit_run(run_start, run_end, run_score);
                        run_start = None;
                        run_score = 0.0;
                        gap_debt = 0;
                    }
                }
            }
        }
    }
    commit_run(run_start, run_end, run_score);

    best.map(|(s, e, _)| (s, e))
}

#[inline]
fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b.iter()).map(|(&x, &y)| x * y).sum()
}

#[derive(Debug, Clone, Copy)]
struct PairMatch {
    start_a: usize,
    end_a: usize, // exclusive frame index
    start_b: usize,
    end_b: usize, // exclusive frame index
    score: f64,
}

/// Trim leading/trailing weak frames and 1-frame ±1-neighbor jitter halos from a `[0, len)`
/// candidate span using a sustained 2-frame strong-match check so a single stray pre-intro
/// match or neighbor halo never shifts the intro boundaries.
fn trim_matching_edges<FStrong, FExact>(
    len: usize,
    sim_threshold: f64,
    is_strong: FStrong,
    exact_dot: FExact,
) -> (usize, usize)
where
    FStrong: Fn(usize) -> bool,
    FExact: Fn(usize) -> f64,
{
    let halo_floor = sim_threshold - 0.12;
    let mut start = 0usize;
    let mut end = len;

    while start + 1 < end {
        if is_strong(start) && is_strong(start + 1) {
            if exact_dot(start) < halo_floor && exact_dot(start + 1) >= sim_threshold {
                start += 1;
                continue;
            }
            break;
        }
        start += 1;
    }
    while end > start + 1 {
        let last = end - 1;
        let prev = end - 2;
        if is_strong(last) && is_strong(prev) {
            if exact_dot(last) < halo_floor && exact_dot(prev) >= sim_threshold {
                end -= 1;
                continue;
            }
            break;
        }
        end -= 1;
    }

    (start, end)
}

/// Precompute ±1-frame jitter-tolerant cosine similarity matrix in a flat `na * nb` buffer.
fn build_jitter_sim_grid(fp_a: &EpisodeFingerprint, fp_b: &EpisodeFingerprint) -> Vec<f64> {
    let na = fp_a.vectors.len();
    let nb = fp_b.vectors.len();
    let mut grid = vec![NO_SIM; na * nb];

    let active_b: Vec<(usize, &[f64])> = fp_b
        .vectors
        .iter()
        .zip(fp_b.flat_flags.iter())
        .enumerate()
        .filter_map(|(j, (v, &flat))| (!flat && !v.is_empty()).then_some((j, v.as_slice())))
        .collect();
    if active_b.is_empty() {
        return grid;
    }

    // Pad both ends with FLAT_SIM so ±1 neighbor lookup needs zero bounds checks
    let mut padded = vec![FLAT_SIM; nb + 2];
    for (i, (va, &flat_a)) in fp_a.vectors.iter().zip(fp_a.flat_flags.iter()).enumerate() {
        if flat_a || va.is_empty() {
            continue;
        }
        padded.fill(FLAT_SIM);
        for &(j, vb) in &active_b {
            padded[j + 1] = dot(va, vb);
        }

        let row_offset = i * nb;
        for &(j, _) in &active_b {
            let mut s = padded[j + 1];
            let left = padded[j] * NEIGHBOR_DECAY;
            if left > s {
                s = left;
            }
            let right = padded[j + 2] * NEIGHBOR_DECAY;
            if right > s {
                s = right;
            }
            grid[row_offset + j] = s;
        }
    }

    grid
}

/// Find the strongest contiguous matching segment between two episodes.
///
/// Scans every diagonal offset `d = j - i` using a Kadane-style maximum subarray
/// scan with ±1 frame neighbor tolerance to absorb sampling phase jitter.
fn find_best_shared_segment(
    fp_a: &EpisodeFingerprint,
    fp_b: &EpisodeFingerprint,
    min_intro_sec: f64,
    max_intro_sec: f64,
    sim_threshold: f64,
) -> Option<PairMatch> {
    let na = fp_a.vectors.len();
    let nb = fp_b.vectors.len();
    if na == 0 || nb == 0 || (fp_a.step_sec - fp_b.step_sec).abs() > 0.05 {
        return None;
    }

    let step = fp_a.step_sec;
    let min_frames = ((min_intro_sec / step).round() as usize).max(4);
    let max_frames = ((max_intro_sec / step).round() as usize).max(min_frames);
    if na < min_frames || nb < min_frames {
        return None;
    }

    let sim_grid = build_jitter_sim_grid(fp_a, fp_b);

    let mut best: Option<PairMatch> = None;
    let baseline = sim_threshold - 0.12;
    let mismatch_penalty = -1.25;
    let flat_penalty = -0.25;
    let min_mean_sim = sim_threshold.max(0.72);
    let max_gap_debt = max_gap_debt_for_step(step);

    let d_min = -((na - min_frames) as isize);
    let d_max = (nb - min_frames) as isize;

    for d in d_min..=d_max {
        let i_start = 0isize.max(-d) as usize;
        let i_end = (na as isize).min((nb as isize) - d) as usize;
        if i_end.saturating_sub(i_start) < min_frames {
            continue;
        }

        let mut cur_score = 0.0;
        let mut seg_start = i_start;
        let mut gap_debt = 0usize;
        let mut best_diag_score = 0.0;
        let mut best_diag_range: Option<(usize, usize)> = None;

        for i in i_start..i_end {
            let j = (i as isize + d) as usize;
            let s = sim_grid[i * nb + j];
            let weight = if !s.is_finite() {
                if cur_score > 0.0 {
                    gap_debt += 1;
                }
                flat_penalty
            } else if s >= sim_threshold {
                gap_debt = gap_debt.saturating_sub(1);
                s - baseline
            } else {
                if cur_score > 0.0 {
                    gap_debt += 2;
                }
                mismatch_penalty
            };

            // Hard-break across any multi-second non-matching gap so a repeating bumper/logo
            // at the start of an episode can never bridge across real cold-open content into
            // the intro sequence.
            if gap_debt > max_gap_debt || cur_score + weight <= 0.0 {
                cur_score = 0.0;
                seg_start = i + 1;
                gap_debt = 0;
            } else {
                cur_score += weight;
                let mut seg_len = (i + 1) - seg_start;
                if seg_len > max_frames {
                    // Cap runaway segments to max_frames by keeping the strongest tail
                    seg_start = (i + 1) - max_frames;
                    seg_len = max_frames;
                }
                if seg_len >= min_frames && cur_score > best_diag_score {
                    best_diag_score = cur_score;
                    best_diag_range = Some((seg_start, i + 1));
                }
            }
        }

        let Some((raw_a0, raw_a1)) = best_diag_range else {
            continue;
        };

        let (trim_start, trim_end) = trim_matching_edges(
            raw_a1 - raw_a0,
            sim_threshold,
            |rel| {
                let idx = raw_a0 + rel;
                let j = (idx as isize + d) as usize;
                let s = sim_grid[idx * nb + j];
                s.is_finite() && s >= sim_threshold
            },
            |rel| {
                let idx = raw_a0 + rel;
                let j = (idx as isize + d) as usize;
                if fp_a.flat_flags[idx]
                    || fp_b.flat_flags[j]
                    || fp_a.vectors[idx].is_empty()
                    || fp_b.vectors[j].is_empty()
                {
                    return FLAT_SIM;
                }
                dot(&fp_a.vectors[idx], &fp_b.vectors[j])
            },
        );
        let a0 = raw_a0 + trim_start;
        let a1 = raw_a0 + trim_end;

        let seg_len = a1 - a0;
        if seg_len < min_frames || seg_len > max_frames {
            continue;
        }

        let mut sim_sum = 0.0;
        let mut sim_count = 0usize;
        let mut matched = 0usize;
        for ii in a0..a1 {
            let j = (ii as isize + d) as usize;
            let s = sim_grid[ii * nb + j];
            if s.is_finite() {
                sim_sum += s;
                sim_count += 1;
                if s >= sim_threshold {
                    matched += 1;
                }
            }
        }

        if sim_count == 0 {
            continue;
        }
        let matched_ratio = (matched as f64) / (seg_len as f64);
        let mean_sim = sim_sum / (sim_count as f64);
        if matched_ratio < 0.72 || mean_sim < min_mean_sim {
            continue;
        }

        let quality_score = mean_sim * matched_ratio * ((seg_len as f64) * step);
        let candidate = PairMatch {
            start_a: a0,
            end_a: a1,
            start_b: (a0 as isize + d) as usize,
            end_b: (a1 as isize + d) as usize,
            score: quality_score,
        };
        if let Some(prev) = best {
            if candidate.score > prev.score {
                best = Some(candidate);
            }
        } else {
            best = Some(candidate);
        }
    }

    best
}

/// Best alignment offset of an episode against a template, plus exact and jitter-aware
/// per-frame similarities at that offset.
struct TemplateAlignment {
    best_offset: usize,
    best_mean: f64,
    best_ratio: f64,
    frame_sims: Vec<f64>,
    exact_frame_sims: Vec<f64>,
}

fn find_best_template_offset(
    ep_fp: &EpisodeFingerprint,
    template_vecs: &[Vec<f64>],
    template_flats: &[bool],
    sim_threshold: f64,
) -> Option<TemplateAlignment> {
    let n = ep_fp.vectors.len();
    let t_len = template_vecs.len();
    if n < t_len || t_len == 0 {
        return None;
    }

    let active_template: Vec<(usize, &[f64])> = template_vecs
        .iter()
        .zip(template_flats.iter())
        .enumerate()
        .filter_map(|(k, (tv, &flat))| (!flat && !tv.is_empty()).then_some((k, tv.as_slice())))
        .collect();
    let num_active = active_template.len();
    if num_active < (((t_len as f64) * 0.4) as usize).max(4) {
        return None;
    }

    let ep_vecs: Vec<Option<&[f64]>> = ep_fp
        .vectors
        .iter()
        .zip(ep_fp.flat_flags.iter())
        .map(|(v, &flat)| (!flat && !v.is_empty()).then_some(v.as_slice()))
        .collect();
    let active_ep: Vec<(usize, &[f64])> = ep_vecs
        .iter()
        .enumerate()
        .filter_map(|(j, opt)| opt.map(|v| (j, v)))
        .collect();
    if active_ep.is_empty() {
        return None;
    }

    let num_offsets = n - t_len + 1;
    let mut matched = vec![0usize; num_offsets];
    let mut sim_sum = vec![0.0_f64; num_offsets];
    let mut compared = vec![0usize; num_offsets];
    let mut padded = vec![FLAT_SIM; n + 2];

    // Compute each template frame's dot products once and accumulate across offsets
    for &(k, tv) in &active_template {
        padded.fill(FLAT_SIM);
        for &(j, ev) in &active_ep {
            padded[j + 1] = dot(tv, ev);
        }
        for offset in 0..num_offsets {
            let j = offset + k;
            let mut s = padded[j + 1];
            let left = padded[j] * NEIGHBOR_DECAY;
            if left > s {
                s = left;
            }
            let right = padded[j + 2] * NEIGHBOR_DECAY;
            if right > s {
                s = right;
            }
            if s >= -0.5 {
                compared[offset] += 1;
                sim_sum[offset] += s;
                if s >= sim_threshold {
                    matched[offset] += 1;
                }
            }
        }
    }

    let mut best_offset: Option<usize> = None;
    let mut best_score = -1.0_f64;
    let mut best_mean = 0.0_f64;
    let mut best_ratio = 0.0_f64;
    for offset in 0..num_offsets {
        let comp = compared[offset];
        if comp == 0 {
            continue;
        }
        let ratio = (matched[offset] as f64) / (num_active as f64);
        let mean_s = sim_sum[offset] / (comp as f64);
        let score = ratio * mean_s;
        if score > best_score {
            best_score = score;
            best_offset = Some(offset);
            best_mean = mean_s;
            best_ratio = ratio;
        }
    }

    let best_offset = best_offset?;

    // Compute both exact and jitter-aware per-frame similarities at best_offset once
    let mut frame_sims = vec![FLAT_SIM; t_len];
    let mut exact_frame_sims = vec![FLAT_SIM; t_len];
    for &(k, tv) in &active_template {
        let j = best_offset + k;
        let exact = ep_vecs[j].map_or(FLAT_SIM, |ev| dot(tv, ev));
        exact_frame_sims[k] = exact;
        let mut s = exact;
        if j > 0 {
            if let Some(ev) = ep_vecs[j - 1] {
                let left = dot(tv, ev) * NEIGHBOR_DECAY;
                if left > s {
                    s = left;
                }
            }
        }
        if j + 1 < n {
            if let Some(ev) = ep_vecs[j + 1] {
                let right = dot(tv, ev) * NEIGHBOR_DECAY;
                if right > s {
                    s = right;
                }
            }
        }
        frame_sims[k] = s;
    }

    Some(TemplateAlignment {
        best_offset,
        best_mean,
        best_ratio,
        frame_sims,
        exact_frame_sims,
    })
}

/// Refine a candidate template's boundaries across multiple reference episodes so any
/// pre-intro cold open, recap, or bumper shared by only a subset of episodes is stripped.
fn refine_template_with_consensus(
    anchor_fp: &EpisodeFingerprint,
    tmpl_start: usize,
    tmpl_end: usize,
    refs: &[&EpisodeFingerprint],
    min_frames: usize,
    sim_threshold: f64,
) -> (usize, usize) {
    let t_len = tmpl_end.saturating_sub(tmpl_start);
    if t_len <= min_frames || refs.len() < 3 {
        return (tmpl_start, tmpl_end);
    }

    let template_vecs = &anchor_fp.vectors[tmpl_start..tmpl_end];
    let template_flats = &anchor_fp.flat_flags[tmpl_start..tmpl_end];

    let mut frame_votes = vec![0usize; t_len];
    let mut matching_others = 0usize;

    for &other in refs {
        if other.episode_id == anchor_fp.episode_id {
            continue;
        }
        let Some(aln) =
            find_best_template_offset(other, template_vecs, template_flats, sim_threshold - 0.04)
        else {
            continue;
        };
        if aln.best_ratio < 0.45 || aln.best_mean < (sim_threshold - 0.04) {
            continue;
        }
        matching_others += 1;
        for (k, &s) in aln.frame_sims.iter().enumerate() {
            if s >= sim_threshold - 0.02 {
                frame_votes[k] += 1;
            }
        }
    }

    if matching_others < 2 {
        return (tmpl_start, tmpl_end);
    }

    // Require at least 60% of the other matching episodes (and >= 2) to agree on template frames,
    // and extract the strongest contiguous core so any interior low-consensus valley (real
    // content between an opening bumper and the intro) is cleanly split out.
    let min_votes = ((matching_others * 6).div_ceil(10)).max(2);
    let max_gap_debt = max_gap_debt_for_step(anchor_fp.step_sec);

    let Some((core_start, core_end)) = best_contiguous_subrun(
        t_len,
        max_gap_debt,
        |k| {
            if frame_votes[k] >= min_votes {
                FrameMatchKind::Match
            } else if template_flats[k] || template_vecs[k].is_empty() {
                FrameMatchKind::Flat
            } else {
                FrameMatchKind::Mismatch
            }
        },
        |k| frame_votes[k] as f64,
    ) else {
        return (tmpl_start, tmpl_end);
    };

    let mut trim_start = core_start;
    let mut trim_end = core_end;
    while trim_start + 1 < trim_end {
        if frame_votes[trim_start] >= min_votes && frame_votes[trim_start + 1] >= min_votes {
            break;
        }
        trim_start += 1;
    }
    while trim_end > trim_start + 1 {
        if frame_votes[trim_end - 1] >= min_votes && frame_votes[trim_end - 2] >= min_votes {
            break;
        }
        trim_end -= 1;
    }

    if trim_end.saturating_sub(trim_start) >= min_frames {
        (tmpl_start + trim_start, tmpl_start + trim_end)
    } else {
        (tmpl_start, tmpl_end)
    }
}

/// Slide a consensus intro template across one episode's fingerprint.
fn align_episode_to_template(
    ep_fp: &EpisodeFingerprint,
    template_vecs: &[Vec<f64>],
    template_flats: &[bool],
    sim_threshold: f64,
) -> Option<IntroSpan> {
    let aln = find_best_template_offset(ep_fp, template_vecs, template_flats, sim_threshold)?;
    if aln.best_ratio < 0.62 || aln.best_mean < sim_threshold.max(0.70) {
        return None;
    }

    let best_offset = aln.best_offset;
    let t_len = template_vecs.len();
    let max_gap_debt = max_gap_debt_for_step(ep_fp.step_sec);

    // Extract the strongest contiguous matching sub-run so an interior mismatch gap in this
    // episode is never bridged into the intro span.
    let (sub_start, sub_end) = best_contiguous_subrun(
        t_len,
        max_gap_debt,
        |k| {
            let s = aln.frame_sims[k];
            if s >= sim_threshold {
                FrameMatchKind::Match
            } else if template_flats[k]
                || template_vecs[k].is_empty()
                || ep_fp.flat_flags[best_offset + k]
                || ep_fp.vectors[best_offset + k].is_empty()
            {
                FrameMatchKind::Flat
            } else {
                FrameMatchKind::Mismatch
            }
        },
        |k| aln.frame_sims[k].max(0.0),
    )?;

    let (trim_start, trim_end) = trim_matching_edges(
        sub_end - sub_start,
        sim_threshold,
        |rel| aln.frame_sims[sub_start + rel] >= sim_threshold,
        |rel| aln.exact_frame_sims[sub_start + rel],
    );

    let local_start = sub_start + trim_start;
    let local_end = sub_start + trim_end;
    if local_end <= local_start {
        return None;
    }

    let start_idx = best_offset + local_start;
    let end_idx = best_offset + local_end;
    let span_frames = local_end - local_start;
    let matched_in_span = (local_start..local_end)
        .filter(|&k| aln.frame_sims[k] >= sim_threshold)
        .count();
    if (matched_in_span as f64) / (span_frames as f64) < 0.70 {
        return None;
    }

    let start_sec = (start_idx as f64) * ep_fp.step_sec;
    let end_sec = (end_idx as f64) * ep_fp.step_sec;
    if end_sec - start_sec < MIN_INTRO_SEC * 0.8 {
        return None;
    }

    let confidence = ((aln.best_mean * 0.65) + (aln.best_ratio * 0.35)).clamp(0.50, 0.99);
    Some(IntroSpan {
        start_sec,
        end_sec,
        confidence,
        source: ep_fp.source.clone(),
    })
}

/// Find the repeating intro sequence across a show's episodes.
///
/// 1. Compares pairs among up to `max_reference_episodes` episodes to locate the
///    consensus intro template (robust to pilot episodes with no intro).
/// 2. Aligns every episode in `fingerprints` against that consensus template to
///    compute per-episode `(intro_start_sec, intro_end_sec)` spans.
pub fn find_shared_intros(
    fingerprints: &[EpisodeFingerprint],
    min_intro_sec: f64,
    max_intro_sec: f64,
    sim_threshold: f64,
    max_reference_episodes: usize,
) -> HashMap<String, IntroSpan> {
    let valid: Vec<&EpisodeFingerprint> = fingerprints
        .iter()
        .filter(|fp| fp.vectors.len() >= 4)
        .collect();
    if valid.len() < 2 {
        return HashMap::new();
    }

    let ref_count = valid.len().min(max_reference_episodes).max(2);
    let refs = &valid[..ref_count];

    let mut pair_matches: Vec<((usize, usize), PairMatch)> = Vec::new();
    for i in 0..refs.len() {
        for j in (i + 1)..refs.len() {
            if let Some(m) = find_best_shared_segment(
                refs[i],
                refs[j],
                min_intro_sec,
                max_intro_sec,
                sim_threshold,
            ) {
                pair_matches.push(((i, j), m));
            }
        }
    }

    if pair_matches.is_empty() {
        return HashMap::new();
    }

    // Score each matched segment by how many other reference episodes agree with it
    let mut candidates: Vec<(&EpisodeFingerprint, usize, usize, f64, usize)> =
        Vec::with_capacity(pair_matches.len());

    for &((i, j), m) in &pair_matches {
        let mut support = 1usize;
        let mut total_score = m.score;
        for &((u, v), other) in &pair_matches {
            if (u, v) == (i, j) {
                continue;
            }
            let other_span = if u == i {
                Some((other.start_a, other.end_a))
            } else if v == i {
                Some((other.start_b, other.end_b))
            } else {
                None
            };
            if let Some((o_start, o_end)) = other_span {
                let ov = m.end_a.min(o_end).saturating_sub(m.start_a.max(o_start));
                let union = m.end_a.max(o_end) - m.start_a.min(o_start);
                if union > 0 && (ov as f64) / (union as f64) >= 0.5 {
                    support += 1;
                    total_score += other.score;
                }
            }
        }
        candidates.push((refs[i], m.start_a, m.end_a, total_score, support));
    }

    // Choose the anchor template with highest multi-episode support, then total score
    candidates.sort_by(|a, b| b.4.cmp(&a.4).then_with(|| b.3.total_cmp(&a.3)));
    let (anchor_fp, raw_start, raw_end, _, _) = candidates[0];

    let min_frames = ((min_intro_sec / anchor_fp.step_sec).round() as usize).max(4);
    let (tmpl_start, tmpl_end) = refine_template_with_consensus(
        anchor_fp,
        raw_start,
        raw_end,
        refs,
        min_frames,
        sim_threshold,
    );

    let template_vecs = &anchor_fp.vectors[tmpl_start..tmpl_end];
    let template_flats = &anchor_fp.flat_flags[tmpl_start..tmpl_end];

    let mut results = HashMap::new();
    for &fp in &valid {
        if let Some(span) =
            align_episode_to_template(fp, template_vecs, template_flats, sim_threshold - 0.02)
        {
            results.insert(fp.episode_id.clone(), span);
        }
    }

    results
}

/// Convenience wrapper calling [`find_shared_intros`] with default thresholds.
pub fn find_shared_intros_default(
    fingerprints: &[EpisodeFingerprint],
) -> HashMap<String, IntroSpan> {
    find_shared_intros(fingerprints, MIN_INTRO_SEC, MAX_INTRO_SEC, 0.70, 6)
}

#[inline]
fn round2(value: f64) -> f64 {
    let rounded = (value * 100.0).round() / 100.0;
    if rounded == 0.0 {
        0.0
    } else {
        rounded
    }
}

/// Find the repeating outro/credits sequence across episodes from tail fingerprints.
///
/// Each fingerprint in `tail_fingerprints` must store the full episode duration in
/// `duration_sec` and tail feature vectors covering the final
/// `vectors.len() as f64 * step_sec` seconds of the episode. Matched tail offsets are
/// converted back into absolute episode timestamps, rejecting any match starting in the
/// first 55% of the episode.
pub fn find_shared_outros(tail_fingerprints: &[EpisodeFingerprint]) -> HashMap<String, IntroSpan> {
    let raw_matches = find_shared_intros(tail_fingerprints, 15.0, 140.0, 0.70, 6);
    if raw_matches.is_empty() {
        return HashMap::new();
    }

    let fp_by_id: HashMap<&str, &EpisodeFingerprint> = tail_fingerprints
        .iter()
        .map(|fp| (fp.episode_id.as_str(), fp))
        .collect();

    let mut results = HashMap::with_capacity(raw_matches.len());
    for (ep_id, span) in raw_matches {
        let Some(&fp) = fp_by_id.get(ep_id.as_str()) else {
            continue;
        };
        let window_len = (fp.vectors.len() as f64) * fp.step_sec;
        let window_start = (fp.duration_sec - window_len).max(0.0);
        let abs_start = window_start + span.start_sec;
        let raw_end = window_start + span.end_sec;
        let abs_end = if fp.duration_sec > 0.0 {
            raw_end.min(fp.duration_sec).max(abs_start)
        } else {
            raw_end
        };

        if fp.duration_sec > 0.0 && abs_start < fp.duration_sec * 0.55 {
            continue;
        }

        results.insert(
            ep_id,
            IntroSpan {
                start_sec: round2(abs_start),
                end_sec: round2(abs_end),
                confidence: span.confidence,
                source: span.source,
            },
        );
    }

    results
}
