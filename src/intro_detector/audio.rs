//! Optional server-side FFmpeg acoustic fingerprinting (24-band Goertzel + CMN).

use std::path::Path;
use std::process::Command;
use std::sync::LazyLock;
use std::time::Duration;

use super::{build_fingerprint_owned, EpisodeFingerprint};
use crate::ffmpeg_bin::resolve_ffmpeg;
use crate::util::run_command_with_timeout;

/// Default scan window from the start of each episode (8 minutes covers cold open + intro).
pub const DEFAULT_AUDIO_SCAN_SEC: f64 = 480.0;

const AUDIO_SAMPLE_RATE: usize = 4000;
const MIN_AUDIO_PCM_SEC: usize = 10;
const SUB_WINDOW_SAMPLES: usize = 512;
const SILENCE_RMS_THRESHOLD: f64 = 45.0;
const CMN_MIN_ACTIVE_FRAMES: usize = 30;
const GOERTZEL_FREQS: [f64; 24] = [
    70.0, 81.0, 94.0, 108.0, 125.0, 145.0, 168.0, 194.0, 224.0, 260.0, 300.0, 348.0, 402.0, 466.0,
    539.0, 624.0, 722.0, 836.0, 968.0, 1120.0, 1296.0, 1500.0, 1700.0, 1920.0,
];
const FFMPEG_TIMEOUT: Duration = Duration::from_secs(45);
const TAIL_FFMPEG_TIMEOUT: Duration = Duration::from_secs(15);
const TAIL_FFMPEG_SAMPLE_RATE: usize = 8000;

static GOERTZEL_COEFFS: LazyLock<[f64; 24]> = LazyLock::new(|| {
    let mut coeffs = [0.0; 24];
    for (i, &freq) in GOERTZEL_FREQS.iter().enumerate() {
        coeffs[i] = 2.0 * ((2.0 * std::f64::consts::PI * freq) / (AUDIO_SAMPLE_RATE as f64)).cos();
    }
    coeffs
});

/// Convert an `i16` sub-window into `f64` samples once while accumulating squared energy.
#[inline]
fn convert_window_and_energy(samples: &[i16], dst: &mut [f64], energy_sum: &mut f64) {
    for (out, &s) in dst.iter_mut().zip(samples.iter()) {
        let v = f64::from(s);
        *out = v;
        *energy_sum += v * v;
    }
}

/// Compute log-scaled spectral band energies using Goertzel filters on pre-converted `f64` samples.
fn goertzel_band_powers(samples: &[f64]) -> [f64; 24] {
    let n = samples.len();
    if n == 0 {
        return [0.0; 24];
    }
    let inv_n = 1.0 / (n as f64);
    let coeffs = &*GOERTZEL_COEFFS;
    let mut powers = [0.0; 24];

    for (idx, &coeff) in coeffs.iter().enumerate() {
        let mut s1 = 0.0_f64;
        let mut s2 = 0.0_f64;
        for &sample in samples {
            let next = sample + coeff * s1 - s2;
            s2 = s1;
            s1 = next;
        }
        let power = (s1 * s1 + s2 * s2 - coeff * s1 * s2).max(0.0);
        powers[idx] = (power * inv_n).ln_1p();
    }
    powers
}

/// Build an audio [`EpisodeFingerprint`] directly from 4 kHz mono signed 16-bit PCM samples.
///
/// Uses 24 log-spaced spectral bands on a uniform log-power scale and applies per-episode
/// temporal mean subtraction (Cepstral Mean Normalization) so constant `1/f` speech/room
/// spectral tilt never causes unrelated cold-open audio to match across episodes.
pub fn extract_audio_fingerprint_from_pcm(
    pcm: &[i16],
    episode_id: &str,
    step_sec: f64,
) -> Option<EpisodeFingerprint> {
    if pcm.len() < AUDIO_SAMPLE_RATE * MIN_AUDIO_PCM_SEC {
        return None;
    }

    let samples_per_step = ((AUDIO_SAMPLE_RATE as f64 * step_sec) as usize).max(256);
    let sub_window = samples_per_step.min(SUB_WINDOW_SAMPLES);
    let step_count = pcm.len() / samples_per_step;
    let mut raw_vectors = Vec::with_capacity(step_count);
    let mut flat_flags = Vec::with_capacity(step_count);

    let mut buf1 = [0.0_f64; SUB_WINDOW_SAMPLES];
    let mut buf2 = [0.0_f64; SUB_WINDOW_SAMPLES];
    let mut buf3 = [0.0_f64; SUB_WINDOW_SAMPLES];

    let mut offset = 0;
    while offset + samples_per_step <= pcm.len() {
        // Sample 3 evenly spaced sub-windows inside each step for crisp spectral stability
        let slack = samples_per_step.saturating_sub(sub_window);
        let c1 = &pcm[offset..offset + sub_window];
        let c2 = &pcm[offset + slack / 2..offset + slack / 2 + sub_window];
        let c3 = &pcm[offset + slack..offset + slack + sub_window];

        let mut energy_sum = 0.0_f64;
        convert_window_and_energy(c1, &mut buf1[..sub_window], &mut energy_sum);
        convert_window_and_energy(c2, &mut buf2[..sub_window], &mut energy_sum);
        convert_window_and_energy(c3, &mut buf3[..sub_window], &mut energy_sum);

        let total_samples = (c1.len() + c2.len() + c3.len()) as f64;
        let rms = (energy_sum / total_samples).sqrt();
        if rms < SILENCE_RMS_THRESHOLD {
            raw_vectors.push(vec![0.0; GOERTZEL_FREQS.len()]);
            flat_flags.push(true);
            offset += samples_per_step;
            continue;
        }

        let b1 = goertzel_band_powers(&buf1[..sub_window]);
        let b2 = goertzel_band_powers(&buf2[..sub_window]);
        let b3 = goertzel_band_powers(&buf3[..sub_window]);

        let mut vec = Vec::with_capacity(GOERTZEL_FREQS.len());
        for i in 0..GOERTZEL_FREQS.len() {
            vec.push((b1[i] + b2[i] + b3[i]) / 3.0);
        }

        raw_vectors.push(vec);
        flat_flags.push(false);
        offset += samples_per_step;
    }

    // Subtract per-band episode average across active frames (CMN) to remove constant 1/f tilt
    let active_count = flat_flags.iter().filter(|&&f| !f).count();
    if active_count >= CMN_MIN_ACTIVE_FRAMES {
        let mut band_means = [0.0_f64; GOERTZEL_FREQS.len()];
        for (vec, &flat) in raw_vectors.iter().zip(flat_flags.iter()) {
            if !flat {
                for (m, &v) in band_means.iter_mut().zip(vec.iter()) {
                    *m += v;
                }
            }
        }
        let inv_active = 1.0 / (active_count as f64);
        for m in &mut band_means {
            *m *= inv_active;
        }
        for (vec, &flat) in raw_vectors.iter_mut().zip(flat_flags.iter()) {
            if !flat {
                for (v, &m) in vec.iter_mut().zip(band_means.iter()) {
                    *v -= m;
                }
            }
        }
    }

    Some(build_fingerprint_owned(
        episode_id,
        step_sec,
        raw_vectors,
        flat_flags,
        (pcm.len() as f64) / (AUDIO_SAMPLE_RATE as f64),
        "audio",
        0.0,
    ))
}

/// Decode mono s16le PCM bytes from `file_path` via `ffmpeg` with caller-supplied pre-input args.
fn run_ffmpeg_pcm(
    file_path: &Path,
    pre_input_args: &[&str],
    sample_rate: usize,
    timeout: Duration,
) -> Option<Vec<u8>> {
    let ffmpeg_bin = resolve_ffmpeg()?;
    if !file_path.exists() {
        return None;
    }

    let sample_rate_str = sample_rate.to_string();
    let mut cmd = Command::new(ffmpeg_bin);
    cmd.args(pre_input_args).arg("-i").arg(file_path).args([
        "-vn",
        "-ac",
        "1",
        "-ar",
        &sample_rate_str,
        "-f",
        "s16le",
        "pipe:1",
    ]);

    let outcome = run_command_with_timeout(&mut cmd, timeout, false);
    if !outcome.success() || outcome.stdout.is_empty() {
        return None;
    }
    Some(outcome.stdout)
}

/// Extract compact per-second spectral audio fingerprints via `ffmpeg`.
///
/// Returns `None` if `ffmpeg` is unavailable, times out, or decoding fails.
pub fn extract_audio_fingerprint(
    file_path: &Path,
    episode_id: &str,
    max_sec: f64,
    step_sec: f64,
) -> Option<EpisodeFingerprint> {
    let max_sec_str = (max_sec.max(0.0) as u64).to_string();
    let stdout = run_ffmpeg_pcm(
        file_path,
        &["-v", "error", "-t", &max_sec_str],
        AUDIO_SAMPLE_RATE,
        FFMPEG_TIMEOUT,
    )?;

    let usable = stdout.len() - (stdout.len() % 2);
    let pcm: Vec<i16> = stdout[..usable]
        .chunks_exact(2)
        .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]))
        .collect();

    extract_audio_fingerprint_from_pcm(&pcm, episode_id, step_sec)
}

/// Extract compact spectral audio fingerprints from the trailing `tail_sec` seconds of an episode.
///
/// Decodes mono 8 kHz s16le PCM via `ffmpeg -sseof`, downsamples 2:1 to the 4 kHz Goertzel
/// analysis rate, and records the full episode `duration_sec` on the resulting fingerprint so
/// tail offsets can be mapped back to absolute timestamps.
pub fn extract_tail_audio_fingerprint(
    file_path: &Path,
    episode_id: &str,
    duration_sec: f64,
    tail_sec: f64,
    step_sec: f64,
) -> Option<EpisodeFingerprint> {
    if !tail_sec.is_finite() || tail_sec <= 0.0 {
        return None;
    }

    let sseof_arg = format!("-{tail_sec:.1}");
    let stdout = run_ffmpeg_pcm(
        file_path,
        &["-hide_banner", "-loglevel", "error", "-sseof", &sseof_arg],
        TAIL_FFMPEG_SAMPLE_RATE,
        TAIL_FFMPEG_TIMEOUT,
    )?;

    // Downsample 8 kHz s16le PCM -> 4 kHz mono i16 (2-sample boxcar average) so
    // Goertzel filter coefficients and step sizes align with `AUDIO_SAMPLE_RATE`.
    let usable = stdout.len() - (stdout.len() % 4);
    let pcm: Vec<i16> = stdout[..usable]
        .chunks_exact(4)
        .map(|chunk| {
            let s0 = i32::from(i16::from_le_bytes([chunk[0], chunk[1]]));
            let s1 = i32::from(i16::from_le_bytes([chunk[2], chunk[3]]));
            ((s0 + s1) / 2) as i16
        })
        .collect();

    let mut fp = extract_audio_fingerprint_from_pcm(&pcm, episode_id, step_sec)?;
    if duration_sec.is_finite() && duration_sec > 0.0 {
        fp.duration_sec = duration_sec;
    }
    Some(fp)
}
