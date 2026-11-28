//! Cross-episode 5-second block discovery, multi-episode consensus boundary pinpointing,
//! and template-based intro/outro extrapolation.

mod consensus;
mod pairwise;
mod template;

use std::borrow::Cow;
use std::collections::HashMap;

use super::{EpisodeFingerprint, IntroSpan, MAX_INTRO_SEC, MIN_INTRO_SEC};

pub use consensus::{discover_intro_template, discover_intro_template_default};
pub use template::match_episodes_against_template;

/// Maximum difference in fingerprint step size (seconds) tolerated when comparing episodes.
const STEP_TOLERANCE_SEC: f64 = 0.05;
/// Default cosine similarity threshold for intro/outro matching.
const DEFAULT_SIM_THRESHOLD: f64 = 0.70;
/// Default maximum number of reference episodes used during consensus discovery.
const DEFAULT_MAX_REF_EPISODES: usize = 6;
/// Minimum duration (seconds) of a repeating outro/credits segment.
const OUTRO_MIN_SEC: f64 = 15.0;
/// Maximum duration (seconds) of a repeating outro/credits segment.
const OUTRO_MAX_SEC: f64 = 140.0;
/// Earliest episode fraction where an outro/credits match may begin.
const OUTRO_MIN_POSITION_FRACTION: f64 = 0.55;
/// Minimum matched-frame ratio required for a pairwise Stage 1 block segment.
const PAIR_MIN_MATCHED_RATIO: f64 = 0.72;
/// Minimum weighted matched-frame ratio required for a Stage 3 template match.
const TEMPLATE_MIN_MATCHED_RATIO: f64 = 0.62;
/// Minimum off-diagonal / off-peak background samples needed to estimate percentiles.
const MIN_BG_SAMPLES: usize = 32;
/// Duration (seconds) of the contiguous matching block required during Stage 1 discovery.
const BLOCK_DURATION_SEC: f64 = 5.0;
/// Sentinel strictly below any unit-vector dot product.
const FLAT_SIM: f64 = -2.0;
/// Sentinel representing an inactive/flat pair cell in the similarity matrix.
const NO_SIM: f64 = f64::NEG_INFINITY;
/// Maximum contiguous mismatch duration (seconds) allowed inside a single intro sequence.
/// Any longer mismatch gap (e.g. real episode cold-open content between a studio logo and
/// the theme song) forces a hard split so real content is never bridged and skipped.
const MAX_INTERIOR_MISMATCH_SEC: f64 = 3.0;
/// Minimum active frames required before applying temporal mean bias suppression.
const BIAS_MIN_ACTIVE_FRAMES: usize = 40;
/// Minimum L2 norm of the episode mean vector to trigger static spatial/spectral bias removal.
const BIAS_NORM_THRESHOLD: f64 = 0.15;
/// Residual L2 norm floor after subtracting the episode mean vector (below this is flat).
const RESIDUAL_NORM_FLOOR: f64 = 0.15;
/// Score multiplier required for half-step phase alignment to beat exact alignment.
const HALF_STEP_PREFERENCE_MARGIN: f64 = 1.02;

#[inline]
fn max_gap_debt_for_step(step_sec: f64) -> usize {
    let max_mismatch_frames = ((MAX_INTERIOR_MISMATCH_SEC / step_sec).round() as usize).max(2);
    max_mismatch_frames * 2
}

#[inline]
fn block_frames_for_step(step_sec: f64) -> usize {
    ((BLOCK_DURATION_SEC / step_sec).round() as usize).max(3)
}

#[inline]
fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b.iter()).map(|(&x, &y)| x * y).sum()
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

/// Compute the `q`-quantile in O(N) average time using `select_nth_unstable_by`
/// (bit-for-bit identical to sorting with `f64::total_cmp`).
fn percentile(samples: &mut [f64], q: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let clamped = q.clamp(0.0, 1.0);
    let idx = (((samples.len() - 1) as f64) * clamped).round() as usize;
    let idx = idx.min(samples.len() - 1);
    let (_, nth, _) = samples.select_nth_unstable_by(idx, f64::total_cmp);
    *nth
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameMatchKind {
    Match,
    Flat,
    Mismatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Exact frame-to-frame alignment: `a[i]` vs `b[j]`.
    Exact,
    /// Global half-step shifted alignment: `a[i]` vs `normalize(b[j] + b[j + 1])`.
    HalfStep,
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

/// Strictly trim leading and trailing frames until both outer 2-frame edges satisfy `is_match`.
fn trim_two_frame_edges(
    mut start: usize,
    mut end: usize,
    is_match: impl Fn(usize) -> bool,
) -> (usize, usize) {
    while start + 1 < end {
        if is_match(start) && is_match(start + 1) {
            break;
        }
        start += 1;
    }
    while end > start + 1 {
        if is_match(end - 1) && is_match(end - 2) {
            break;
        }
        end -= 1;
    }
    (start, end)
}

/// Per-offset correlation accumulators for a single alignment phase (`Exact` or `HalfStep`).
struct PhaseAccumulators {
    compared: Vec<usize>,
    sim_sum: Vec<f64>,
    matched: Vec<usize>,
    matched_sum: Vec<f64>,
}

impl PhaseAccumulators {
    fn new(len: usize) -> Self {
        Self {
            compared: vec![0; len],
            sim_sum: vec![0.0; len],
            matched: vec![0; len],
            matched_sum: vec![0.0; len],
        }
    }

    #[inline]
    fn record(&mut self, oi: usize, sim: f64, match_thresh: f64) {
        self.compared[oi] += 1;
        self.sim_sum[oi] += sim;
        if sim >= match_thresh {
            self.matched[oi] += 1;
            self.matched_sum[oi] += sim;
        }
    }
}

/// Select between the best `Exact` and `HalfStep` candidates, requiring half-step to exceed
/// exact by [`HALF_STEP_PREFERENCE_MARGIN`].
fn pick_preferred_phase<T>(exact: Option<(f64, T)>, half_step: Option<(f64, T)>) -> Option<T> {
    match (exact, half_step) {
        (Some((es, ea)), Some((hs, ha))) => {
            if hs > es * HALF_STEP_PREFERENCE_MARGIN {
                Some(ha)
            } else {
                Some(ea)
            }
        }
        (Some((_, ea)), None) => Some(ea),
        (None, Some((_, ha))) => Some(ha),
        (None, None) => None,
    }
}

/// Precompute `1.0 / ||v[j] + v[j + 1]||_2` for adjacent active unit vectors into `out` so
/// global half-step similarities can be derived from exact dot products with zero extra inner products.
fn compute_inv_half_norms_into(vectors: &[Vec<f64>], flat_flags: &[bool], out: &mut Vec<f64>) {
    out.clear();
    let n = vectors.len();
    if n < 2 {
        return;
    }
    out.resize(n - 1, 0.0_f64);
    for j in 0..(n - 1) {
        if flat_flags[j]
            || flat_flags[j + 1]
            || vectors[j].is_empty()
            || vectors[j].len() != vectors[j + 1].len()
        {
            continue;
        }
        let norm_sq = 2.0 + 2.0 * dot(&vectors[j], &vectors[j + 1]);
        if norm_sq > 1e-6 {
            out[j] = 1.0 / norm_sq.sqrt();
        }
    }
}

fn compute_inv_half_norms(vectors: &[Vec<f64>], flat_flags: &[bool]) -> Vec<f64> {
    let mut out = Vec::new();
    compute_inv_half_norms_into(vectors, flat_flags, &mut out);
    out
}

/// Sample a unit vector at signed index `idx` under `phase`, borrowing directly for `Phase::Exact`
/// and allocating a normalized blend only for `Phase::HalfStep`.
fn sample_vec_at<'a>(
    vectors: &'a [Vec<f64>],
    flat_flags: &[bool],
    inv_half_norms: &[f64],
    idx: isize,
    phase: Phase,
) -> Option<Cow<'a, [f64]>> {
    if idx < 0 {
        return None;
    }
    let j = idx as usize;
    match phase {
        Phase::Exact => {
            if j < vectors.len() && !flat_flags[j] && !vectors[j].is_empty() {
                Some(Cow::Borrowed(&vectors[j]))
            } else {
                None
            }
        }
        Phase::HalfStep => {
            if j + 1 < vectors.len()
                && j < inv_half_norms.len()
                && inv_half_norms[j] > 0.0
                && !flat_flags[j]
                && !flat_flags[j + 1]
                && !vectors[j].is_empty()
                && vectors[j].len() == vectors[j + 1].len()
            {
                let inv = inv_half_norms[j];
                let blended: Vec<f64> = vectors[j]
                    .iter()
                    .zip(vectors[j + 1].iter())
                    .map(|(&a, &b)| (a + b) * inv)
                    .collect();
                Some(Cow::Owned(blended))
            } else {
                None
            }
        }
    }
}

/// Bias-suppressed working view of an [`EpisodeFingerprint`] used for Stage 1 discovery
/// and Stage 2 consensus boundary pinpointing without mutating caller fingerprints.
///
/// Borrows `vectors`, `flat_flags`, and `inv_half_norms` without deep cloning when the
/// episode does not require temporal mean bias suppression.
struct WorkingFingerprint<'a> {
    orig: &'a EpisodeFingerprint,
    vectors: Cow<'a, [Vec<f64>]>,
    flat_flags: Cow<'a, [bool]>,
    orig_inv_half_norms: Vec<f64>,
    centered_inv_half_norms: Option<Vec<f64>>,
}

impl<'a> WorkingFingerprint<'a> {
    #[inline]
    fn inv_half_norms(&self) -> &[f64] {
        self.centered_inv_half_norms
            .as_deref()
            .unwrap_or(&self.orig_inv_half_norms)
    }
}

/// Construct a temporal-centered working copy when the episode has enough active frames
/// (`>= 40`) and exhibits a genuine static spatial/spectral bias (`||mean||_2 > 0.15`).
fn build_working_fingerprint(fp: &EpisodeFingerprint) -> WorkingFingerprint<'_> {
    let orig_inv_half_norms = compute_inv_half_norms(&fp.vectors, &fp.flat_flags);
    let mut dim = 0usize;
    let mut active_count = 0usize;

    for (v, &flat) in fp.vectors.iter().zip(fp.flat_flags.iter()) {
        if !flat && !v.is_empty() {
            if dim == 0 {
                dim = v.len();
            }
            if v.len() == dim {
                active_count += 1;
            }
        }
    }

    if active_count >= BIAS_MIN_ACTIVE_FRAMES && dim > 0 {
        let mut mean_vec = vec![0.0_f64; dim];
        for (v, &flat) in fp.vectors.iter().zip(fp.flat_flags.iter()) {
            if !flat && v.len() == dim {
                for (m, &x) in mean_vec.iter_mut().zip(v.iter()) {
                    *m += x;
                }
            }
        }
        let inv_active = 1.0 / (active_count as f64);
        for m in &mut mean_vec {
            *m *= inv_active;
        }
        let mean_norm = mean_vec.iter().map(|&x| x * x).sum::<f64>().sqrt();

        if mean_norm > BIAS_NORM_THRESHOLD {
            let mut centered_vecs = Vec::with_capacity(fp.vectors.len());
            let mut centered_flats = Vec::with_capacity(fp.flat_flags.len());

            for (v, &flat) in fp.vectors.iter().zip(fp.flat_flags.iter()) {
                if flat || v.len() != dim {
                    centered_vecs.push(vec![0.0; dim]);
                    centered_flats.push(true);
                    continue;
                }
                let mut res = Vec::with_capacity(dim);
                let mut norm_sq = 0.0_f64;
                for (&x, &m) in v.iter().zip(mean_vec.iter()) {
                    let d = x - m;
                    norm_sq += d * d;
                    res.push(d);
                }
                let res_norm = norm_sq.sqrt();
                if res_norm < RESIDUAL_NORM_FLOOR {
                    res.fill(0.0);
                    centered_vecs.push(res);
                    centered_flats.push(true);
                } else {
                    let inv = 1.0 / res_norm;
                    for d in &mut res {
                        *d *= inv;
                    }
                    centered_vecs.push(res);
                    centered_flats.push(false);
                }
            }

            let centered_inv_half_norms = compute_inv_half_norms(&centered_vecs, &centered_flats);
            return WorkingFingerprint {
                orig: fp,
                vectors: Cow::Owned(centered_vecs),
                flat_flags: Cow::Owned(centered_flats),
                orig_inv_half_norms,
                centered_inv_half_norms: Some(centered_inv_half_norms),
            };
        }
    }

    WorkingFingerprint {
        orig: fp,
        vectors: Cow::Borrowed(&fp.vectors),
        flat_flags: Cow::Borrowed(&fp.flat_flags),
        orig_inv_half_norms,
        centered_inv_half_norms: None,
    }
}

// ---------------------------------------------------------------------------
// Backwards-Compatible High-Level Wrappers
// ---------------------------------------------------------------------------

/// Find the repeating intro sequence across a show's episodes.
///
/// 1. Discovers a tight consensus [`crate::intro_detector::IntroTemplate`] across up to
///    `max_reference_episodes` reference episodes via 5-second block discovery and `P(k)`
///    boundary pinpointing.
/// 2. Extrapolates that template to every episode in `fingerprints` to compute per-episode
///    `(start_sec, end_sec)` spans.
pub fn find_shared_intros(
    fingerprints: &[EpisodeFingerprint],
    min_intro_sec: f64,
    max_intro_sec: f64,
    sim_threshold: f64,
    max_reference_episodes: usize,
) -> HashMap<String, IntroSpan> {
    let Some(template) = discover_intro_template(
        fingerprints,
        min_intro_sec,
        max_intro_sec,
        sim_threshold,
        max_reference_episodes,
    ) else {
        return HashMap::new();
    };
    match_episodes_against_template(fingerprints, &template)
}

/// Convenience wrapper calling [`find_shared_intros`] with default thresholds.
pub fn find_shared_intros_default(
    fingerprints: &[EpisodeFingerprint],
) -> HashMap<String, IntroSpan> {
    find_shared_intros(
        fingerprints,
        MIN_INTRO_SEC,
        MAX_INTRO_SEC,
        DEFAULT_SIM_THRESHOLD,
        DEFAULT_MAX_REF_EPISODES,
    )
}

/// Find the repeating outro/credits sequence across episodes from tail fingerprints.
///
/// Each fingerprint in `tail_fingerprints` must store the full episode duration in
/// `duration_sec` and tail feature vectors covering the final
/// `vectors.len() as f64 * step_sec` seconds of the episode. Matched tail offsets are
/// converted back into absolute episode timestamps, rejecting any match starting in the
/// first 55% of the episode.
pub fn find_shared_outros(tail_fingerprints: &[EpisodeFingerprint]) -> HashMap<String, IntroSpan> {
    let raw_matches = find_shared_intros(
        tail_fingerprints,
        OUTRO_MIN_SEC,
        OUTRO_MAX_SEC,
        DEFAULT_SIM_THRESHOLD,
        DEFAULT_MAX_REF_EPISODES,
    );
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

        if fp.duration_sec > 0.0 && abs_start < fp.duration_sec * OUTRO_MIN_POSITION_FRACTION {
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
