//! Stage 3: Template matching / extrapolation to all episodes.

use std::collections::HashMap;

use super::{
    best_contiguous_subrun, compute_inv_half_norms_into, dot, max_gap_debt_for_step, percentile,
    pick_preferred_phase, round2, trim_two_frame_edges, FrameMatchKind, Phase,
    DEFAULT_SIM_THRESHOLD, FLAT_SIM, MIN_BG_SAMPLES, STEP_TOLERANCE_SEC,
    TEMPLATE_MIN_MATCHED_RATIO,
};
use crate::intro_detector::{EpisodeFingerprint, IntroSpan, IntroTemplate, MIN_INTRO_SEC};

#[derive(Default)]
struct WeightedPhaseAccumulators {
    sim_sum: Vec<f64>,
    w_compared: Vec<f64>,
    w_matched: Vec<f64>,
}

impl WeightedPhaseAccumulators {
    fn reset(&mut self, len: usize) {
        self.sim_sum.clear();
        self.sim_sum.resize(len, 0.0_f64);
        self.w_compared.clear();
        self.w_compared.resize(len, 0.0_f64);
        self.w_matched.clear();
        self.w_matched.resize(len, 0.0_f64);
    }

    #[inline]
    fn record(&mut self, offset: usize, sim: f64, weight: f64, sim_threshold: f64) {
        self.w_compared[offset] += weight;
        self.sim_sum[offset] += sim * weight;
        if sim >= sim_threshold {
            self.w_matched[offset] += weight;
        }
    }

    fn best_offset(
        &self,
        total_active_weight: f64,
        phase: Phase,
    ) -> Option<(f64, BestTemplateOffset)> {
        let mut best: Option<(f64, BestTemplateOffset)> = None;
        for offset in 0..self.w_compared.len() {
            let comp = self.w_compared[offset];
            if comp <= 0.0 {
                continue;
            }
            let ratio = self.w_matched[offset] / total_active_weight;
            let mean_s = self.sim_sum[offset] / comp;
            let score = ratio * mean_s;
            if !best.as_ref().is_some_and(|b| score <= b.0) {
                best = Some((
                    score,
                    BestTemplateOffset {
                        offset,
                        phase,
                        mean_sim: mean_s,
                        ratio,
                    },
                ));
            }
        }
        best
    }
}

#[derive(Default)]
struct TemplateMatchScratch {
    inv_half_norms: Vec<f64>,
    ex: WeightedPhaseAccumulators,
    hs: WeightedPhaseAccumulators,
    dots: Vec<f64>,
    off_peak_means: Vec<f64>,
    frame_bg_samples: Vec<f64>,
    exact_sims: Vec<f64>,
}

struct BestTemplateOffset {
    offset: usize,
    phase: Phase,
    mean_sim: f64,
    ratio: f64,
}

struct EpisodeTemplateMatch {
    offset: usize,
    phase: Phase,
    best_mean: f64,
    best_ratio: f64,
    offset_bg_p95: Option<f64>,
    frame_bg_p95: f64,
}

fn accumulate_template_offset_scores(
    active_template: &[(usize, &[f64], f64)],
    active_ep: &[(usize, &[f64])],
    n: usize,
    t_len: usize,
    sim_threshold: f64,
    scratch: &mut TemplateMatchScratch,
) {
    let num_offsets = n - t_len + 1;
    let num_half_offsets = n.saturating_sub(t_len);

    scratch.ex.reset(num_offsets);
    scratch.hs.reset(num_half_offsets);
    scratch.dots.clear();
    scratch.dots.resize(n, FLAT_SIM);

    for &(k, tv, w) in active_template {
        scratch.dots.fill(FLAT_SIM);
        for &(j, ev) in active_ep {
            if tv.len() == ev.len() {
                scratch.dots[j] = dot(tv, ev);
            }
        }
        for offset in 0..num_offsets {
            let s = scratch.dots[offset + k];
            if s > FLAT_SIM {
                scratch.ex.record(offset, s, w, sim_threshold);
            }
        }
        for offset in 0..num_half_offsets {
            let j = offset + k;
            let inv = scratch.inv_half_norms[j];
            if inv > 0.0 {
                let s0 = scratch.dots[j];
                let s1 = scratch.dots[j + 1];
                if s0 > FLAT_SIM && s1 > FLAT_SIM {
                    let sh = (s0 + s1) * inv;
                    scratch.hs.record(offset, sh, w, sim_threshold);
                }
            }
        }
    }
}

fn estimate_template_background(
    active_template: &[(usize, &[f64], f64)],
    active_ep: &[(usize, &[f64])],
    t_len: usize,
    best_offset: usize,
    best_phase: Phase,
    scratch: &mut TemplateMatchScratch,
) -> (Option<f64>, f64) {
    // Estimate offset-level off-peak background similarity (when enough non-overlapping offsets exist)
    let min_sep = (t_len / 2).max(1);
    scratch.off_peak_means.clear();
    for offset in 0..scratch.ex.w_compared.len() {
        if offset.abs_diff(best_offset) >= min_sep && scratch.ex.w_compared[offset] > 0.0 {
            scratch
                .off_peak_means
                .push(scratch.ex.sim_sum[offset] / scratch.ex.w_compared[offset]);
        }
    }
    let offset_bg_p95 = if scratch.off_peak_means.len() >= 10 {
        Some(percentile(&mut scratch.off_peak_means, 0.95))
    } else {
        None
    };

    // Estimate frame-level off-peak background similarity from episode frames strictly outside
    // the matched span (works for both full-length and windowed scans).
    let match_lo = best_offset;
    let match_hi = best_offset + t_len + usize::from(best_phase == Phase::HalfStep);
    let outside_ep: Vec<&[f64]> = active_ep
        .iter()
        .filter_map(|&(j, ev)| (j < match_lo || j >= match_hi).then_some(ev))
        .collect();
    let frame_bg_p95 = if !outside_ep.is_empty() {
        let tmpl_stride = (active_template.len() / 16).max(1);
        let ep_stride = (outside_ep.len() / 64).max(1);
        scratch.frame_bg_samples.clear();
        scratch.frame_bg_samples.reserve(1024);
        for &(_, tv, _) in active_template.iter().step_by(tmpl_stride).take(16) {
            for &ev in outside_ep.iter().step_by(ep_stride).take(64) {
                if tv.len() == ev.len() {
                    scratch.frame_bg_samples.push(dot(tv, ev));
                }
            }
        }
        if scratch.frame_bg_samples.len() >= MIN_BG_SAMPLES {
            percentile(&mut scratch.frame_bg_samples, 0.95)
        } else {
            0.0
        }
    } else {
        0.0
    };

    (offset_bg_p95, frame_bg_p95)
}

/// Compute exact per-frame similarities at the chosen `(best_offset, best_phase)` with ZERO ±1 halo.
fn compute_exact_frame_sims(
    active_template: &[(usize, &[f64], f64)],
    ep_vecs: &[Option<&[f64]>],
    n: usize,
    t_len: usize,
    best_offset: usize,
    best_phase: Phase,
    scratch: &mut TemplateMatchScratch,
) {
    scratch.exact_sims.clear();
    scratch.exact_sims.resize(t_len, FLAT_SIM);
    for &(k, tv, _) in active_template {
        let j = best_offset + k;
        scratch.exact_sims[k] = match best_phase {
            Phase::Exact => ep_vecs[j].map_or(FLAT_SIM, |ev| {
                if tv.len() == ev.len() {
                    dot(tv, ev)
                } else {
                    FLAT_SIM
                }
            }),
            Phase::HalfStep => {
                if j + 1 < n && scratch.inv_half_norms[j] > 0.0 {
                    match (ep_vecs[j], ep_vecs[j + 1]) {
                        (Some(e0), Some(e1)) if tv.len() == e0.len() && tv.len() == e1.len() => {
                            (dot(tv, e0) + dot(tv, e1)) * scratch.inv_half_norms[j]
                        }
                        _ => FLAT_SIM,
                    }
                } else {
                    FLAT_SIM
                }
            }
        };
    }
}

fn align_episode_to_intro_template(
    ep: &EpisodeFingerprint,
    template: &IntroTemplate,
    active_template: &[(usize, &[f64], f64)],
    total_active_weight: f64,
    scratch: &mut TemplateMatchScratch,
) -> Option<EpisodeTemplateMatch> {
    let n = ep.vectors.len();
    let t_len = template.vectors.len();
    if n < t_len || (ep.step_sec - template.step_sec).abs() > STEP_TOLERANCE_SEC {
        return None;
    }

    let ep_vecs: Vec<Option<&[f64]>> = ep
        .vectors
        .iter()
        .zip(ep.flat_flags.iter())
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

    compute_inv_half_norms_into(&ep.vectors, &ep.flat_flags, &mut scratch.inv_half_norms);
    accumulate_template_offset_scores(
        active_template,
        &active_ep,
        n,
        t_len,
        template.sim_threshold,
        scratch,
    );

    let best_ex = scratch.ex.best_offset(total_active_weight, Phase::Exact);
    let best_hs = scratch.hs.best_offset(total_active_weight, Phase::HalfStep);
    let best = pick_preferred_phase(best_ex, best_hs)?;

    let (offset_bg_p95, frame_bg_p95) = estimate_template_background(
        active_template,
        &active_ep,
        t_len,
        best.offset,
        best.phase,
        scratch,
    );

    compute_exact_frame_sims(
        active_template,
        &ep_vecs,
        n,
        t_len,
        best.offset,
        best.phase,
        scratch,
    );

    Some(EpisodeTemplateMatch {
        offset: best.offset,
        phase: best.phase,
        best_mean: best.mean_sim,
        best_ratio: best.ratio,
        offset_bg_p95,
        frame_bg_p95,
    })
}

fn refine_template_match_span(
    ep: &EpisodeFingerprint,
    template: &IntroTemplate,
    aln: &EpisodeTemplateMatch,
    exact_sims: &[f64],
) -> Option<IntroSpan> {
    let t_len = template.vectors.len();
    let min_mean_floor = template.sim_threshold.max(DEFAULT_SIM_THRESHOLD);

    // Reject episodes whose peak does not clear basic quality or their own background
    if aln.best_ratio < TEMPLATE_MIN_MATCHED_RATIO || aln.best_mean < min_mean_floor {
        return None;
    }
    if let Some(bg_p95) = aln.offset_bg_p95 {
        if aln.best_mean < bg_p95 + 0.12 {
            return None;
        }
    }
    if aln.frame_bg_p95 > 0.0 && aln.best_mean < aln.frame_bg_p95 + 0.06 {
        return None;
    }

    let edge_thresh = template
        .sim_threshold
        .max(aln.frame_bg_p95 + 0.08)
        .min((aln.best_mean - 0.06).max(template.sim_threshold));
    let max_gap_debt = max_gap_debt_for_step(ep.step_sec);

    let (sub_start, sub_end) = best_contiguous_subrun(
        t_len,
        max_gap_debt,
        |k| {
            let s = exact_sims[k];
            if s >= edge_thresh {
                FrameMatchKind::Match
            } else {
                let tmpl_flat = template.flat_flags.get(k).copied().unwrap_or(false)
                    || template.vectors[k].is_empty();
                let j = aln.offset + k;
                let ep_flat = match aln.phase {
                    Phase::Exact => ep.flat_flags[j] || ep.vectors[j].is_empty(),
                    Phase::HalfStep => {
                        j + 1 >= ep.vectors.len()
                            || ep.flat_flags[j]
                            || ep.flat_flags[j + 1]
                            || ep.vectors[j].is_empty()
                            || ep.vectors[j + 1].is_empty()
                    }
                };
                if tmpl_flat || ep_flat {
                    FrameMatchKind::Flat
                } else {
                    FrameMatchKind::Mismatch
                }
            }
        },
        |k| {
            let w = template
                .weights
                .get(k)
                .copied()
                .unwrap_or(1.0)
                .clamp(0.1, 1.0);
            exact_sims[k].max(0.0) * w
        },
    )?;

    // Strict inward-only 2-frame exact boundary trim (zero halo, never extends outside template)
    let (local_start, local_end) =
        trim_two_frame_edges(sub_start, sub_end, |k| exact_sims[k] >= edge_thresh);
    if local_end <= local_start {
        return None;
    }

    let span_frames = local_end - local_start;
    let matched_in_span = (local_start..local_end)
        .filter(|&k| exact_sims[k] >= edge_thresh)
        .count();
    if (matched_in_span as f64) / (span_frames as f64) < DEFAULT_SIM_THRESHOLD {
        return None;
    }

    let phase_shift = match aln.phase {
        Phase::Exact => 0.0,
        Phase::HalfStep => 0.5 * ep.step_sec,
    };
    let start_idx = aln.offset + local_start;
    let end_idx = aln.offset + local_end;
    let start_sec = round2(ep.offset_sec + (start_idx as f64) * ep.step_sec + phase_shift);
    let end_sec = round2(ep.offset_sec + (end_idx as f64) * ep.step_sec + phase_shift);
    if end_sec - start_sec < MIN_INTRO_SEC * 0.8 {
        return None;
    }

    let confidence = ((aln.best_mean * 0.65) + (aln.best_ratio * 0.35)).clamp(0.50, 0.99);
    Some(IntroSpan {
        start_sec,
        end_sec,
        confidence,
        source: ep.source.clone(),
    })
}

/// Match a slice of episode fingerprints (full-length or windowed via `offset_sec`) against a
/// previously discovered [`IntroTemplate`].
pub fn match_episodes_against_template(
    fingerprints: &[EpisodeFingerprint],
    template: &IntroTemplate,
) -> HashMap<String, IntroSpan> {
    let t_len = template.vectors.len();
    if t_len == 0 || !template.step_sec.is_finite() || template.step_sec <= 0.0 {
        return HashMap::new();
    }

    let active_template: Vec<(usize, &[f64], f64)> = template
        .vectors
        .iter()
        .enumerate()
        .filter_map(|(k, tv)| {
            let flat = template.flat_flags.get(k).copied().unwrap_or(false);
            if flat || tv.is_empty() {
                return None;
            }
            let w = template
                .weights
                .get(k)
                .copied()
                .unwrap_or(1.0)
                .clamp(0.1, 1.0);
            Some((k, tv.as_slice(), w))
        })
        .collect();
    let min_active = (((t_len as f64) * 0.4) as usize).max(3).min(t_len);
    if active_template.len() < min_active {
        return HashMap::new();
    }
    let total_active_weight: f64 = active_template.iter().map(|&(_, _, w)| w).sum();
    if total_active_weight <= 0.0 {
        return HashMap::new();
    }

    let mut results = HashMap::new();
    let mut scratch = TemplateMatchScratch::default();

    for ep in fingerprints {
        let Some(aln) = align_episode_to_intro_template(
            ep,
            template,
            &active_template,
            total_active_weight,
            &mut scratch,
        ) else {
            continue;
        };

        if let Some(span) = refine_template_match_span(ep, template, &aln, &scratch.exact_sims) {
            results.insert(ep.episode_id.clone(), span);
        }
    }

    results
}
