//! Stage 1: Pairwise 5-second block cross-episode intro discovery.

use super::{
    best_contiguous_subrun, block_frames_for_step, dot, max_gap_debt_for_step, percentile,
    trim_two_frame_edges, FrameMatchKind, Phase, WorkingFingerprint, HALF_STEP_PREFERENCE_MARGIN,
    MIN_BG_SAMPLES, NO_SIM, PAIR_MIN_MATCHED_RATIO, STEP_TOLERANCE_SEC,
};

#[derive(Debug, Clone, Copy)]
pub(super) struct PairMatch {
    pub(super) start_a: usize,
    pub(super) end_a: usize, // exclusive frame index
    pub(super) start_b: usize,
    pub(super) end_b: usize, // exclusive frame index
    pub(super) score: f64,
}

#[derive(Default)]
pub(super) struct DiagonalScratch {
    diag_sims: Vec<f64>,
    block_covered: Vec<bool>,
}

struct SimGridView<'a> {
    exact_grid: &'a [f64],
    inv_half_norm_b: &'a [f64],
    na: usize,
    nb: usize,
}

struct DiagonalEvalParams {
    step: f64,
    block_frames: usize,
    min_active_in_block: usize,
    min_frames: usize,
    max_frames: usize,
    max_gap_debt: usize,
    frame_thresh: f64,
    block_thresh: f64,
    min_mean_sim: f64,
}

/// Precompute the exact frame-to-frame cosine similarity matrix (`na * nb`, no ±1 halo)
/// into the reusable `grid` buffer.
fn build_exact_sim_grid(
    wf_a: &WorkingFingerprint<'_>,
    wf_b: &WorkingFingerprint<'_>,
    grid: &mut Vec<f64>,
) {
    let na = wf_a.vectors.len();
    let nb = wf_b.vectors.len();
    grid.clear();
    grid.resize(na * nb, NO_SIM);

    let active_b: Vec<(usize, &[f64])> = wf_b
        .vectors
        .iter()
        .zip(wf_b.flat_flags.iter())
        .enumerate()
        .filter_map(|(j, (v, &flat))| (!flat && !v.is_empty()).then_some((j, v.as_slice())))
        .collect();
    if active_b.is_empty() {
        return;
    }

    for (i, (va, &flat_a)) in wf_a.vectors.iter().zip(wf_a.flat_flags.iter()).enumerate() {
        if flat_a || va.is_empty() {
            continue;
        }
        let row_offset = i * nb;
        for &(j, vb) in &active_b {
            if va.len() == vb.len() {
                grid[row_offset + j] = dot(va, vb);
            }
        }
    }
}

/// Deterministically sample up to ~4096 cells of the exact similarity matrix to measure the
/// off-diagonal background distribution `(bg_mean, bg_p95)`.
fn estimate_pair_background(exact_grid: &[f64], na: usize, nb: usize) -> (f64, f64) {
    let step_i = (na / 64).max(1);
    let step_j = (nb / 64).max(1);
    let mut samples = Vec::with_capacity(4096);

    let mut i = 0usize;
    while i < na {
        let row = i * nb;
        let mut j = 0usize;
        while j < nb {
            let s = exact_grid[row + j];
            if s.is_finite() {
                samples.push(s);
            }
            j += step_j;
        }
        i += step_i;
    }

    if samples.len() < MIN_BG_SAMPLES {
        return (0.0, 0.0);
    }
    let bg_mean = samples.iter().sum::<f64>() / (samples.len() as f64);
    let bg_p95 = percentile(&mut samples, 0.95);
    (bg_mean, bg_p95)
}

fn fill_diagonal_sims(
    grid: &SimGridView<'_>,
    i_start: usize,
    i_end: usize,
    d: isize,
    phase: Phase,
    diag_sims: &mut Vec<f64>,
) {
    diag_sims.clear();
    diag_sims.reserve(i_end - i_start);
    match phase {
        Phase::Exact => {
            for i in i_start..i_end {
                let j = (i as isize + d) as usize;
                diag_sims.push(grid.exact_grid[i * grid.nb + j]);
            }
        }
        Phase::HalfStep => {
            for i in i_start..i_end {
                let j = (i as isize + d) as usize;
                let inv = grid.inv_half_norm_b[j];
                if inv > 0.0 {
                    let s0 = grid.exact_grid[i * grid.nb + j];
                    let s1 = grid.exact_grid[i * grid.nb + j + 1];
                    if s0.is_finite() && s1.is_finite() {
                        diag_sims.push((s0 + s1) * inv);
                    } else {
                        diag_sims.push(NO_SIM);
                    }
                } else {
                    diag_sims.push(NO_SIM);
                }
            }
        }
    }
}

fn mark_covered_blocks(
    diag_sims: &[f64],
    params: &DiagonalEvalParams,
    block_covered: &mut Vec<bool>,
) -> bool {
    let diag_len = diag_sims.len();
    block_covered.clear();
    block_covered.resize(diag_len, false);

    let block_frames = params.block_frames;
    let mut active_in_block = 0usize;
    let mut matched_in_block = 0usize;
    let mut sim_sum = 0.0_f64;
    let mut any_covered = false;

    for t in 0..diag_len {
        let s_in = diag_sims[t];
        if s_in.is_finite() {
            active_in_block += 1;
            sim_sum += s_in;
            if s_in >= params.frame_thresh {
                matched_in_block += 1;
            }
        }
        if t >= block_frames {
            let s_out = diag_sims[t - block_frames];
            if s_out.is_finite() {
                active_in_block -= 1;
                sim_sum -= s_out;
                if s_out >= params.frame_thresh {
                    matched_in_block -= 1;
                }
            }
        }
        if t + 1 >= block_frames && active_in_block >= params.min_active_in_block {
            let block_mean = sim_sum / (active_in_block as f64);
            let enough_matched = matched_in_block * 4 >= active_in_block * 3
                || (block_frames == 3
                    && matched_in_block >= 2
                    && matched_in_block + 1 >= active_in_block);
            if block_mean >= params.block_thresh && enough_matched {
                let w_start = t + 1 - block_frames;
                for k in w_start..=t {
                    if diag_sims[k].is_finite() && diag_sims[k] >= params.frame_thresh {
                        block_covered[k] = true;
                        any_covered = true;
                    }
                }
            }
        }
    }

    any_covered
}

/// Cap runaway segments to `max_frames` by keeping the highest-scoring window inside the run.
fn cap_segment_to_max_frames(
    sub_start: usize,
    sub_end: usize,
    max_frames: usize,
    diag_sims: &[f64],
    block_covered: &[bool],
) -> (usize, usize) {
    if sub_end - sub_start <= max_frames {
        return (sub_start, sub_end);
    }
    let mut cur = 0.0_f64;
    let mut best_win_score = -1.0_f64;
    let mut best_win_start = sub_start;
    for t in sub_start..sub_end {
        if block_covered[t] {
            cur += diag_sims[t].max(0.0);
        }
        if t >= sub_start + max_frames {
            let out_idx = t - max_frames;
            if block_covered[out_idx] {
                cur -= diag_sims[out_idx].max(0.0);
            }
        }
        if t + 1 >= sub_start + max_frames && cur > best_win_score {
            best_win_score = cur;
            best_win_start = t + 1 - max_frames;
        }
    }
    (best_win_start, best_win_start + max_frames)
}

fn score_candidate_segment(
    i_start: usize,
    seg: (usize, usize),
    d: isize,
    phase: Phase,
    diag_sims: &[f64],
    params: &DiagonalEvalParams,
) -> Option<PairMatch> {
    let (sub_start, sub_end) = seg;
    let seg_len = sub_end.saturating_sub(sub_start);
    if seg_len < params.min_frames || seg_len > params.max_frames {
        return None;
    }

    let mut seg_sim_sum = 0.0_f64;
    let mut seg_sim_count = 0usize;
    let mut seg_matched = 0usize;
    for &s in &diag_sims[sub_start..sub_end] {
        if s.is_finite() {
            seg_sim_sum += s;
            seg_sim_count += 1;
            if s >= params.frame_thresh {
                seg_matched += 1;
            }
        }
    }

    if seg_sim_count == 0 {
        return None;
    }
    let matched_ratio = (seg_matched as f64) / (seg_len as f64);
    let mean_sim = seg_sim_sum / (seg_sim_count as f64);
    if matched_ratio < PAIR_MIN_MATCHED_RATIO || mean_sim < params.min_mean_sim {
        return None;
    }

    let a0 = i_start + sub_start;
    let a1 = i_start + sub_end;
    let raw_score = mean_sim * mean_sim * matched_ratio * ((seg_len as f64) * params.step);
    let score = match phase {
        Phase::Exact => raw_score,
        Phase::HalfStep => raw_score / HALF_STEP_PREFERENCE_MARGIN,
    };

    Some(PairMatch {
        start_a: a0,
        end_a: a1,
        start_b: (a0 as isize + d) as usize,
        end_b: (a1 as isize + d) as usize,
        score,
    })
}

/// Evaluate a single diagonal `d` under `phase` using 5-second contiguous block matching
/// and strict gap-debt splitting (zero ±1 frame dilation).
fn eval_diagonal_blocks(
    grid: &SimGridView<'_>,
    d: isize,
    phase: Phase,
    params: &DiagonalEvalParams,
    scratch: &mut DiagonalScratch,
) -> Option<PairMatch> {
    let nb_eff = match phase {
        Phase::Exact => grid.nb,
        Phase::HalfStep => grid.nb.saturating_sub(1),
    };
    let i_start = 0isize.max(-d) as usize;
    let i_end = (grid.na as isize).min((nb_eff as isize) - d) as usize;
    let diag_len = i_end.saturating_sub(i_start);
    if diag_len < params.min_frames {
        return None;
    }

    fill_diagonal_sims(grid, i_start, i_end, d, phase, &mut scratch.diag_sims);
    if !mark_covered_blocks(&scratch.diag_sims, params, &mut scratch.block_covered) {
        return None;
    }

    let (sub_start, sub_end) = best_contiguous_subrun(
        diag_len,
        params.max_gap_debt,
        |t| {
            if scratch.block_covered[t] {
                FrameMatchKind::Match
            } else if !scratch.diag_sims[t].is_finite() {
                FrameMatchKind::Flat
            } else {
                FrameMatchKind::Mismatch
            }
        },
        |t| scratch.diag_sims[t].max(0.0),
    )?;

    let (capped_start, capped_end) = cap_segment_to_max_frames(
        sub_start,
        sub_end,
        params.max_frames,
        &scratch.diag_sims,
        &scratch.block_covered,
    );

    // Strict 2-frame exact boundary trim (no halo, no flat edges)
    let trimmed = trim_two_frame_edges(capped_start, capped_end, |t| scratch.block_covered[t]);

    score_candidate_segment(i_start, trimmed, d, phase, &scratch.diag_sims, params)
}

/// Find the strongest 5-second-block-supported shared segment between two episodes,
/// reusing caller-provided scratch buffers across reference pairs.
pub(super) fn find_best_shared_segment(
    wf_a: &WorkingFingerprint<'_>,
    wf_b: &WorkingFingerprint<'_>,
    min_intro_sec: f64,
    max_intro_sec: f64,
    sim_threshold: f64,
    grid_scratch: &mut Vec<f64>,
    diag_scratch: &mut DiagonalScratch,
) -> Option<PairMatch> {
    let na = wf_a.vectors.len();
    let nb = wf_b.vectors.len();
    if na == 0 || nb == 0 || (wf_a.orig.step_sec - wf_b.orig.step_sec).abs() > STEP_TOLERANCE_SEC {
        return None;
    }

    let step = wf_a.orig.step_sec;
    let block_frames = block_frames_for_step(step);
    let min_frames = ((min_intro_sec / step).round() as usize).max(block_frames);
    let max_frames = ((max_intro_sec / step).round() as usize).max(min_frames);
    if na < min_frames || nb < min_frames {
        return None;
    }

    build_exact_sim_grid(wf_a, wf_b, grid_scratch);
    let (bg_mean, bg_p95) = estimate_pair_background(grid_scratch, na, nb);
    let bg_block_floor = (bg_mean + 0.25).max(bg_p95 + 0.04).min(0.84);
    let frame_thresh = sim_threshold.max(bg_block_floor);
    let block_thresh = sim_threshold.max(bg_block_floor);

    let params = DiagonalEvalParams {
        step,
        block_frames,
        min_active_in_block: (block_frames * 3).div_ceil(5),
        min_frames,
        max_frames,
        max_gap_debt: max_gap_debt_for_step(step),
        frame_thresh,
        block_thresh,
        min_mean_sim: frame_thresh.max(PAIR_MIN_MATCHED_RATIO),
    };

    let grid = SimGridView {
        exact_grid: grid_scratch,
        inv_half_norm_b: wf_b.inv_half_norms(),
        na,
        nb,
    };

    let d_min = -((na - min_frames) as isize);
    let d_max = (nb - min_frames) as isize;
    let mut best: Option<PairMatch> = None;

    for d in d_min..=d_max {
        if let Some(cand) = eval_diagonal_blocks(&grid, d, Phase::Exact, &params, diag_scratch) {
            if !best.is_some_and(|prev| cand.score <= prev.score) {
                best = Some(cand);
            }
        }
        if nb > min_frames {
            if let Some(cand) =
                eval_diagonal_blocks(&grid, d, Phase::HalfStep, &params, diag_scratch)
            {
                if !best.is_some_and(|prev| cand.score <= prev.score) {
                    best = Some(cand);
                }
            }
        }
    }

    best
}
