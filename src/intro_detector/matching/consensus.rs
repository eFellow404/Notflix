//! Stage 2: Multi-episode consensus core alignment, `P(k)` boundary pinpointing,
//! and [`IntroTemplate`] assembly.

use std::borrow::Cow;

use super::pairwise::{find_best_shared_segment, DiagonalScratch, PairMatch};
use super::{
    best_contiguous_subrun, block_frames_for_step, build_working_fingerprint, dot,
    max_gap_debt_for_step, percentile, pick_preferred_phase, sample_vec_at, trim_two_frame_edges,
    FrameMatchKind, Phase, PhaseAccumulators, WorkingFingerprint, DEFAULT_MAX_REF_EPISODES,
    DEFAULT_SIM_THRESHOLD, FLAT_SIM, MIN_BG_SAMPLES, STEP_TOLERANCE_SEC,
};
use crate::intro_detector::{EpisodeFingerprint, IntroTemplate, MAX_INTRO_SEC, MIN_INTRO_SEC};

struct AnchorCandidate {
    ref_idx: usize,
    start: usize,
    end: usize,
    total_score: f64,
    support: usize,
}

#[derive(Debug, Clone, Copy)]
struct AlignedRef {
    ref_idx: usize,
    offset: isize,
    phase: Phase,
}

struct CoreAlignment {
    offset: isize,
    phase: Phase,
    mean_sim: f64,
    matched_ratio: f64,
    matched_mean: f64,
}

struct ConsensusParams {
    min_intro_sec: f64,
    max_intro_sec: f64,
    sim_threshold: f64,
    step: f64,
    core_len: usize,
    pad: usize,
}

struct ConsensusProfile {
    p_vals: Vec<f64>,
    active_counts: Vec<usize>,
    agree_counts: Vec<usize>,
    agree_masks: Vec<Vec<bool>>,
}

fn best_phase_alignment(
    acc: &PhaseAccumulators,
    phase: Phase,
    offset_min: isize,
    num_active: usize,
    min_matched_frames: usize,
) -> Option<(f64, CoreAlignment)> {
    let mut best: Option<(f64, CoreAlignment)> = None;
    for oi in 0..acc.compared.len() {
        let comp = acc.compared[oi];
        if comp > 0 && acc.matched[oi] >= min_matched_frames {
            let ratio = (acc.matched[oi] as f64) / (num_active as f64);
            let mean_sim = acc.sim_sum[oi] / (comp as f64);
            let matched_mean = acc.matched_sum[oi] / (acc.matched[oi] as f64);
            let score = ratio * mean_sim;
            if !best.as_ref().is_some_and(|b| score <= b.0) {
                best = Some((
                    score,
                    CoreAlignment {
                        offset: offset_min + oi as isize,
                        phase,
                        mean_sim,
                        matched_ratio: ratio,
                        matched_mean,
                    },
                ));
            }
        }
    }
    best
}

/// Align one reference episode's working vectors against the anchor core `[core_start..core_end)`
/// using exact and global half-step correlation (zero ±1 halo, supporting partial/negative offsets
/// so episodes without a recap still align cleanly to a recap+intro anchor core).
fn align_ref_to_core(
    ep_wf: &WorkingFingerprint<'_>,
    core_vecs: &[Vec<f64>],
    core_flats: &[bool],
    min_frames: usize,
    sim_threshold: f64,
) -> Option<CoreAlignment> {
    let n = ep_wf.vectors.len();
    let core_len = core_vecs.len();
    let min_overlap = min_frames.min(core_len);
    if n < min_overlap || core_len == 0 {
        return None;
    }

    let active_core: Vec<(usize, &[f64])> = core_vecs
        .iter()
        .zip(core_flats.iter())
        .enumerate()
        .filter_map(|(k, (v, &flat))| (!flat && !v.is_empty()).then_some((k, v.as_slice())))
        .collect();
    let num_active = active_core.len();
    if num_active < (((core_len as f64) * 0.4) as usize).max(3) {
        return None;
    }

    let active_ep: Vec<(usize, &[f64])> = ep_wf
        .vectors
        .iter()
        .zip(ep_wf.flat_flags.iter())
        .enumerate()
        .filter_map(|(j, (v, &flat))| (!flat && !v.is_empty()).then_some((j, v.as_slice())))
        .collect();
    if active_ep.is_empty() {
        return None;
    }

    let offset_min = -((core_len - min_overlap) as isize);
    let offset_max = (n - min_overlap) as isize;
    let num_offsets = (offset_max - offset_min + 1) as usize;

    let mut ex = PhaseAccumulators::new(num_offsets);
    let mut hs = PhaseAccumulators::new(num_offsets);
    let mut dots = vec![FLAT_SIM; n];
    let inv_half_norms = ep_wf.inv_half_norms();
    let match_thresh = sim_threshold - 0.04;

    for &(k, cv) in &active_core {
        dots.fill(FLAT_SIM);
        for &(j, ev) in &active_ep {
            if cv.len() == ev.len() {
                dots[j] = dot(cv, ev);
            }
        }

        let k_isize = k as isize;
        let oi_start = (0isize - k_isize - offset_min).max(0) as usize;
        let oi_end_ex = ((n as isize) - k_isize - offset_min).min(num_offsets as isize);
        if oi_end_ex <= oi_start as isize {
            continue;
        }
        let oi_end = oi_end_ex as usize;

        for oi in oi_start..oi_end {
            let j = (offset_min + oi as isize + k_isize) as usize;
            let s = dots[j];
            if s > FLAT_SIM {
                ex.record(oi, s, match_thresh);
            }
            if j + 1 < n {
                let inv = inv_half_norms[j];
                if inv > 0.0 {
                    let s1 = dots[j + 1];
                    if s > FLAT_SIM && s1 > FLAT_SIM {
                        let sh = (s + s1) * inv;
                        hs.record(oi, sh, match_thresh);
                    }
                }
            }
        }
    }

    let min_matched_frames = ((min_overlap as f64) * 0.60).ceil() as usize;
    let best_ex = best_phase_alignment(
        &ex,
        Phase::Exact,
        offset_min,
        num_active,
        min_matched_frames,
    );
    let best_hs = best_phase_alignment(
        &hs,
        Phase::HalfStep,
        offset_min,
        num_active,
        min_matched_frames,
    );

    pick_preferred_phase(best_ex, best_hs)
}

/// Sample off-intro pairwise similarities across the aligned reference episodes to establish
/// the background distribution `(bg_mean, bg_p95)` for adaptive boundary thresholding.
fn estimate_consensus_background(
    working_refs: &[WorkingFingerprint<'_>],
    aligned_refs: &[AlignedRef],
    core_len: usize,
) -> (f64, f64) {
    let mut off_indices: Vec<Vec<usize>> = Vec::with_capacity(aligned_refs.len());
    for ar in aligned_refs {
        let wf = &working_refs[ar.ref_idx];
        let intro_lo = ar.offset.max(0) as usize;
        let intro_hi = (ar.offset + core_len as isize).max(0) as usize;
        let all_off: Vec<usize> = wf
            .vectors
            .iter()
            .zip(wf.flat_flags.iter())
            .enumerate()
            .filter_map(|(j, (v, &flat))| {
                (!flat && !v.is_empty() && (j < intro_lo || j >= intro_hi)).then_some(j)
            })
            .collect();
        let stride = (all_off.len() / 32).max(1);
        let sampled: Vec<usize> = all_off.into_iter().step_by(stride).take(32).collect();
        off_indices.push(sampled);
    }

    let mut samples = Vec::with_capacity(2048);
    'outer: for a in 0..aligned_refs.len() {
        let wf_a = &working_refs[aligned_refs[a].ref_idx];
        for b in (a + 1)..aligned_refs.len() {
            let wf_b = &working_refs[aligned_refs[b].ref_idx];
            for &ia in &off_indices[a] {
                let va = &wf_a.vectors[ia];
                for &ib in &off_indices[b] {
                    let vb = &wf_b.vectors[ib];
                    if va.len() == vb.len() {
                        samples.push(dot(va, vb));
                        if samples.len() >= 2048 {
                            break 'outer;
                        }
                    }
                }
            }
        }
    }

    if samples.len() < MIN_BG_SAMPLES {
        return (0.0, 0.0);
    }
    let bg_mean = samples.iter().sum::<f64>() / (samples.len() as f64);
    let bg_p95 = percentile(&mut samples, 0.95);
    (bg_mean, bg_p95)
}

/// Run Stage 1 5-second block discovery across every reference pair, reusing scratch buffers.
fn collect_pair_matches(
    working_refs: &[WorkingFingerprint<'_>],
    min_intro_sec: f64,
    max_intro_sec: f64,
    sim_threshold: f64,
) -> Vec<((usize, usize), PairMatch)> {
    let mut pair_matches = Vec::new();
    let mut grid_scratch = Vec::new();
    let mut diag_scratch = DiagonalScratch::default();

    for i in 0..working_refs.len() {
        for j in (i + 1)..working_refs.len() {
            if let Some(m) = find_best_shared_segment(
                &working_refs[i],
                &working_refs[j],
                min_intro_sec,
                max_intro_sec,
                sim_threshold,
                &mut grid_scratch,
                &mut diag_scratch,
            ) {
                pair_matches.push(((i, j), m));
            }
        }
    }

    pair_matches
}

/// Cluster overlapping pairwise spans on each reference episode to pick the anchor core
/// with highest multi-episode support and cumulative quality score.
fn select_anchor_core(pair_matches: &[((usize, usize), PairMatch)]) -> Option<AnchorCandidate> {
    if pair_matches.is_empty() {
        return None;
    }

    let mut candidates: Vec<AnchorCandidate> = Vec::with_capacity(pair_matches.len() * 2);
    for &((i, j), m) in pair_matches {
        for (ep_idx, s, e) in [(i, m.start_a, m.end_a), (j, m.start_b, m.end_b)] {
            let mut support = 1usize;
            let mut total_score = m.score;
            for &((u, v), other) in pair_matches {
                if (u, v) == (i, j) {
                    continue;
                }
                let other_span = if u == ep_idx {
                    Some((other.start_a, other.end_a))
                } else if v == ep_idx {
                    Some((other.start_b, other.end_b))
                } else {
                    None
                };
                if let Some((o_start, o_end)) = other_span {
                    let ov = e.min(o_end).saturating_sub(s.max(o_start));
                    let union = e.max(o_end) - s.min(o_start);
                    if union > 0 && (ov as f64) / (union as f64) >= 0.5 {
                        support += 1;
                        total_score += other.score;
                    }
                }
            }
            candidates.push(AnchorCandidate {
                ref_idx: ep_idx,
                start: s,
                end: e,
                total_score,
                support,
            });
        }
    }

    candidates.sort_by(|a, b| {
        b.support
            .cmp(&a.support)
            .then_with(|| b.total_score.total_cmp(&a.total_score))
    });
    candidates.into_iter().next()
}

/// Align all reference episodes against the winning anchor core.
fn align_references_to_core(
    working_refs: &[WorkingFingerprint<'_>],
    anchor: &AnchorCandidate,
    step: f64,
    min_frames: usize,
    sim_threshold: f64,
) -> Vec<AlignedRef> {
    let anchor_wf = &working_refs[anchor.ref_idx];
    let core_vecs = &anchor_wf.vectors[anchor.start..anchor.end];
    let core_flats = &anchor_wf.flat_flags[anchor.start..anchor.end];

    let mut aligned_refs = Vec::with_capacity(working_refs.len());
    aligned_refs.push(AlignedRef {
        ref_idx: anchor.ref_idx,
        offset: anchor.start as isize,
        phase: Phase::Exact,
    });

    for (r_idx, wf) in working_refs.iter().enumerate() {
        if r_idx == anchor.ref_idx {
            continue;
        }
        if (wf.orig.step_sec - step).abs() > STEP_TOLERANCE_SEC {
            continue;
        }
        let Some(aln) = align_ref_to_core(wf, core_vecs, core_flats, min_frames, sim_threshold)
        else {
            continue;
        };
        let accepted = (aln.matched_ratio >= 0.55 && aln.mean_sim >= sim_threshold - 0.04)
            || (aln.matched_ratio >= 0.45 && aln.matched_mean >= sim_threshold);
        if accepted {
            aligned_refs.push(AlignedRef {
                ref_idx: r_idx,
                offset: aln.offset,
                phase: aln.phase,
            });
        }
    }

    aligned_refs
}

/// Probe `[-pad .. core_len + pad)` around the core using exact Mean Pairwise Cosine
/// Similarity `P(k)` and per-frame consensus agreement.
fn compute_consensus_profile(
    working_refs: &[WorkingFingerprint<'_>],
    aligned_refs: &[AlignedRef],
    params: &ConsensusParams,
) -> ConsensusProfile {
    let win_len = params.core_len + 2 * params.pad;
    let mut p_vals = vec![FLAT_SIM; win_len];
    let mut active_counts = vec![0usize; win_len];
    let mut agree_counts = vec![0usize; win_len];
    let mut agree_masks: Vec<Vec<bool>> = vec![vec![false; aligned_refs.len()]; win_len];

    let frame_agree_thresh = params.sim_threshold.max(DEFAULT_SIM_THRESHOLD);
    let mut sampled_vecs: Vec<(usize, Cow<'_, [f64]>)> = Vec::with_capacity(aligned_refs.len());
    let mut sum_vec: Vec<f64> = Vec::new();

    for w in 0..win_len {
        let rel_k = (w as isize) - (params.pad as isize);
        sampled_vecs.clear();
        let mut dim = 0usize;

        for (slot, ar) in aligned_refs.iter().enumerate() {
            let wf = &working_refs[ar.ref_idx];
            if let Some(sampled) = sample_vec_at(
                &wf.vectors,
                &wf.flat_flags,
                wf.inv_half_norms(),
                ar.offset + rel_k,
                ar.phase,
            ) {
                if dim == 0 {
                    dim = sampled.len();
                }
                if sampled.len() == dim {
                    sampled_vecs.push((slot, sampled));
                }
            }
        }

        let active_m = sampled_vecs.len();
        active_counts[w] = active_m;
        if active_m < 2 || dim == 0 {
            continue;
        }

        sum_vec.clear();
        sum_vec.resize(dim, 0.0_f64);
        for (_, v) in &sampled_vecs {
            for (acc, &x) in sum_vec.iter_mut().zip(v.iter()) {
                *acc += x;
            }
        }
        let sum_norm_sq: f64 = sum_vec.iter().map(|&x| x * x).sum();
        let p_k = (sum_norm_sq - (active_m as f64)) / ((active_m * (active_m - 1)) as f64);
        p_vals[w] = p_k;

        if sum_norm_sq > 1e-8 {
            let inv_norm = 1.0 / sum_norm_sq.sqrt();
            for x in &mut sum_vec {
                *x *= inv_norm;
            }
            let mut agreed = 0usize;
            for (slot, v) in &sampled_vecs {
                if dot(v, &sum_vec) >= frame_agree_thresh {
                    agreed += 1;
                    agree_masks[w][*slot] = true;
                }
            }
            agree_counts[w] = agreed;
        }
    }

    ConsensusProfile {
        p_vals,
        active_counts,
        agree_counts,
        agree_masks,
    }
}

/// Refine consensus start/end window indices via adaptive background thresholding,
/// bounded outward extension, gap-debt splitting, and strict 2-frame edge trimming.
fn refine_consensus_bounds(
    profile: &ConsensusProfile,
    working_refs: &[WorkingFingerprint<'_>],
    aligned_refs: &[AlignedRef],
    params: &ConsensusParams,
) -> Option<(usize, usize)> {
    let win_len = params.core_len + 2 * params.pad;
    let mut interior_p: Vec<f64> = profile.p_vals[params.pad..(params.pad + params.core_len)]
        .iter()
        .copied()
        .filter(|&p| p > FLAT_SIM)
        .collect();
    let intro_p50 = if interior_p.is_empty() {
        params.sim_threshold
    } else {
        percentile(&mut interior_p, 0.50)
    };
    let (bg_mean, bg_p95) =
        estimate_consensus_background(working_refs, aligned_refs, params.core_len);
    let adaptive_mid = bg_mean + 0.45 * (intro_p50 - bg_mean);
    let boundary_thresh = params
        .sim_threshold
        .max(bg_p95 + 0.08)
        .max(adaptive_mid)
        .min((intro_p50 - 0.08).max(params.sim_threshold));

    let m_total = aligned_refs.len();
    let min_agree = if m_total >= 3 {
        (((m_total as f64) * 0.65).ceil() as usize).max(2)
    } else {
        2
    };

    let is_consensus_match =
        |w: usize| profile.agree_counts[w] >= min_agree && profile.p_vals[w] >= boundary_thresh;

    // Extend outward from the Stage 1 core ONLY across contiguous matching frames (never
    // bridging multi-frame fades/gaps outside the core into stray pre/post-intro matches).
    let mut ext_start = params.pad;
    while ext_start > 0 {
        let can_extend = is_consensus_match(ext_start - 1)
            || (ext_start >= 3
                && is_consensus_match(ext_start - 2)
                && is_consensus_match(ext_start - 3));
        if can_extend {
            ext_start -= 1;
        } else {
            break;
        }
    }

    let mut ext_end = params.pad + params.core_len;
    while ext_end < win_len {
        let can_extend = is_consensus_match(ext_end)
            || (ext_end + 2 < win_len
                && is_consensus_match(ext_end + 1)
                && is_consensus_match(ext_end + 2));
        if can_extend {
            ext_end += 1;
        } else {
            break;
        }
    }

    let ext_len = ext_end.saturating_sub(ext_start);
    let max_gap_debt = max_gap_debt_for_step(params.step);
    let (rel_sub_start, rel_sub_end) = best_contiguous_subrun(
        ext_len,
        max_gap_debt,
        |rel| {
            let w = ext_start + rel;
            if is_consensus_match(w) {
                FrameMatchKind::Match
            } else if profile.active_counts[w] < 2 {
                FrameMatchKind::Flat
            } else {
                FrameMatchKind::Mismatch
            }
        },
        |rel| {
            let w = ext_start + rel;
            profile.p_vals[w].max(0.0) * (profile.agree_counts[w] as f64)
        },
    )?;

    // Strictly trim leading and trailing frames until both outer 2-frame edges satisfy consensus
    let (trim_start, trim_end) = trim_two_frame_edges(
        ext_start + rel_sub_start,
        ext_start + rel_sub_end,
        is_consensus_match,
    );

    let final_len = trim_end.saturating_sub(trim_start);
    let final_sec = (final_len as f64) * params.step;
    if final_len < 3
        || final_sec < params.min_intro_sec * 0.9
        || final_sec > params.max_intro_sec + params.step
    {
        return None;
    }

    Some((trim_start, trim_end))
}

fn accumulate_orig_frame(
    wf: &WorkingFingerprint<'_>,
    idx: isize,
    phase: Phase,
    sum_orig: &mut Vec<f64>,
    count_orig: &mut usize,
) {
    if let Some(sampled) = sample_vec_at(
        &wf.orig.vectors,
        &wf.orig.flat_flags,
        &wf.orig_inv_half_norms,
        idx,
        phase,
    ) {
        if sum_orig.is_empty() {
            sum_orig.resize(sampled.len(), 0.0);
        }
        if sampled.len() == sum_orig.len() {
            for (acc, &x) in sum_orig.iter_mut().zip(sampled.iter()) {
                *acc += x;
            }
            *count_orig += 1;
        }
    }
}

/// Build an [`IntroTemplate`] from the ORIGINAL normalized vectors of the agreeing reference
/// episodes so both full-length and short windowed Phase 2 scans match it seamlessly.
fn assemble_intro_template(
    working_refs: &[WorkingFingerprint<'_>],
    aligned_refs: &[AlignedRef],
    profile: &ConsensusProfile,
    trim_range: (usize, usize),
    params: &ConsensusParams,
    source: String,
) -> IntroTemplate {
    let (trim_start, trim_end) = trim_range;
    let final_len = trim_end.saturating_sub(trim_start);
    let mut tmpl_vectors = Vec::with_capacity(final_len);
    let mut tmpl_flats = Vec::with_capacity(final_len);
    let mut tmpl_weights = Vec::with_capacity(final_len);

    for w in trim_start..trim_end {
        let rel_k = (w as isize) - (params.pad as isize);
        let prefer_agreed = profile.agree_counts[w] >= 2;
        let mut sum_orig: Vec<f64> = Vec::new();
        let mut count_orig = 0usize;

        for (slot, ar) in aligned_refs.iter().enumerate() {
            if prefer_agreed && !profile.agree_masks[w][slot] {
                continue;
            }
            accumulate_orig_frame(
                &working_refs[ar.ref_idx],
                ar.offset + rel_k,
                ar.phase,
                &mut sum_orig,
                &mut count_orig,
            );
        }

        // Fallback to all active aligned refs if no agreed original vectors were sampled
        if count_orig == 0 && prefer_agreed {
            for ar in aligned_refs {
                accumulate_orig_frame(
                    &working_refs[ar.ref_idx],
                    ar.offset + rel_k,
                    ar.phase,
                    &mut sum_orig,
                    &mut count_orig,
                );
            }
        }

        let norm = sum_orig.iter().map(|&x| x * x).sum::<f64>().sqrt();
        if count_orig == 0 || norm <= 1e-6 {
            tmpl_vectors.push(Vec::new());
            tmpl_flats.push(true);
            tmpl_weights.push(0.1);
        } else {
            let inv = 1.0 / norm;
            for x in &mut sum_orig {
                *x *= inv;
            }
            tmpl_vectors.push(sum_orig);
            tmpl_flats.push(false);
            let weight = if profile.p_vals[w] > FLAT_SIM {
                profile.p_vals[w].clamp(0.1, 1.0)
            } else {
                0.1
            };
            tmpl_weights.push(weight);
        }
    }

    IntroTemplate {
        step_sec: params.step,
        vectors: tmpl_vectors,
        flat_flags: tmpl_flats,
        weights: tmpl_weights,
        sim_threshold: params.sim_threshold,
        source,
    }
}

/// Discover a consensus [`IntroTemplate`] across up to `max_reference_episodes` reference
/// episodes using:
/// 1. Static spatial/spectral bias suppression (temporal centering).
/// 2. 5-second contiguous block discovery across reference pairs.
/// 3. Multi-episode mean pairwise cosine similarity `P(k)` boundary pinpointing.
pub fn discover_intro_template(
    fingerprints: &[EpisodeFingerprint],
    min_intro_sec: f64,
    max_intro_sec: f64,
    sim_threshold: f64,
    max_reference_episodes: usize,
) -> Option<IntroTemplate> {
    let valid: Vec<&EpisodeFingerprint> = fingerprints
        .iter()
        .filter(|fp| fp.vectors.len() >= 4)
        .collect();
    if valid.len() < 2 {
        return None;
    }

    let ref_count = valid.len().min(max_reference_episodes).max(2);
    let refs = &valid[..ref_count];
    let working_refs: Vec<WorkingFingerprint<'_>> = refs
        .iter()
        .map(|&fp| build_working_fingerprint(fp))
        .collect();

    // Stage 1: 5-second block discovery on every reference pair
    let pair_matches =
        collect_pair_matches(&working_refs, min_intro_sec, max_intro_sec, sim_threshold);
    let anchor = select_anchor_core(&pair_matches)?;

    let anchor_wf = &working_refs[anchor.ref_idx];
    let step = anchor_wf.orig.step_sec;
    let block_frames = block_frames_for_step(step);
    let min_frames = ((min_intro_sec / step).round() as usize).max(block_frames);
    let core_len = anchor.end.saturating_sub(anchor.start);
    if core_len < 3 {
        return None;
    }

    // Stage 2: Align all reference episodes to the winning anchor core
    let aligned_refs =
        align_references_to_core(&working_refs, &anchor, step, min_frames, sim_threshold);
    if aligned_refs.len() < 2 {
        return None;
    }

    let params = ConsensusParams {
        min_intro_sec,
        max_intro_sec,
        sim_threshold,
        step,
        core_len,
        pad: block_frames + 4,
    };

    let profile = compute_consensus_profile(&working_refs, &aligned_refs, &params);
    let trim_range = refine_consensus_bounds(&profile, &working_refs, &aligned_refs, &params)?;

    Some(assemble_intro_template(
        &working_refs,
        &aligned_refs,
        &profile,
        trim_range,
        &params,
        anchor_wf.orig.source.clone(),
    ))
}

/// Convenience wrapper calling [`discover_intro_template`] with default thresholds.
pub fn discover_intro_template_default(
    fingerprints: &[EpisodeFingerprint],
) -> Option<IntroTemplate> {
    discover_intro_template(
        fingerprints,
        MIN_INTRO_SEC,
        MAX_INTRO_SEC,
        DEFAULT_SIM_THRESHOLD,
        DEFAULT_MAX_REF_EPISODES,
    )
}
