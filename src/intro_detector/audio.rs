//! Optional server-side FFmpeg acoustic fingerprinting (16-band Goertzel + CMN).

use std::path::Path;
use std::process::Command;
use std::sync::LazyLock;
use std::time::Duration;

use super::{build_fingerprint, EpisodeFingerprint};
use crate::util::run_command_with_timeout;

/// Default scan window from the start of each episode (8 minutes covers cold open + intro).
pub const DEFAULT_AUDIO_SCAN_SEC: f64 = 480.0;

const AUDIO_SAMPLE_RATE: usize = 4000;
const GOERTZEL_FREQS: [f64; 16] = [
    80.0, 100.0, 125.0, 155.0, 195.0, 245.0, 305.0, 380.0, 475.0, 595.0, 745.0, 930.0, 1160.0,
    1450.0, 1700.0, 1900.0,
];
const FFMPEG_TIMEOUT: Duration = Duration::from_secs(45);

static GOERTZEL_COEFFS: LazyLock<[f64; 16]> = LazyLock::new(|| {
    let mut coeffs = [0.0; 16];
    for (i, &freq) in GOERTZEL_FREQS.iter().enumerate() {
        coeffs[i] = 2.0 * ((2.0 * std::f64::consts::PI * freq) / (AUDIO_SAMPLE_RATE as f64)).cos();
    }
    coeffs
});

/// Compute log-scaled spectral band energies using Goertzel filters.
fn goertzel_band_powers(samples: &[i16]) -> [f64; 16] {
    let n = samples.len();
    if n == 0 {
        return [0.0; 16];
    }
    let inv_n = 1.0 / (n as f64);
    let coeffs = &*GOERTZEL_COEFFS;
    let mut powers = [0.0; 16];

    for (idx, &coeff) in coeffs.iter().enumerate() {
        let mut s1 = 0.0_f64;
        let mut s2 = 0.0_f64;
        for &sample in samples {
            let next = f64::from(sample) + coeff * s1 - s2;
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
/// Uses 16 log-spaced spectral bands on a uniform log-power scale and applies per-episode
/// temporal mean subtraction (Cepstral Mean Normalization) so constant `1/f` speech/room
/// spectral tilt never causes unrelated cold-open audio to match across episodes.
pub fn extract_audio_fingerprint_from_pcm(
    pcm: &[i16],
    episode_id: &str,
    step_sec: f64,
) -> Option<EpisodeFingerprint> {
    if pcm.len() < AUDIO_SAMPLE_RATE * 10 {
        return None;
    }

    let samples_per_step = ((AUDIO_SAMPLE_RATE as f64 * step_sec) as usize).max(256);
    let sub_window = samples_per_step.min(512);
    let step_count = pcm.len() / samples_per_step;
    let mut raw_vectors = Vec::with_capacity(step_count);
    let mut flat_flags = Vec::with_capacity(step_count);

    let mut offset = 0;
    while offset + samples_per_step <= pcm.len() {
        // Sample 3 evenly spaced sub-windows inside each step for crisp spectral stability
        let slack = samples_per_step.saturating_sub(sub_window);
        let c1 = &pcm[offset..offset + sub_window];
        let c2 = &pcm[offset + slack / 2..offset + slack / 2 + sub_window];
        let c3 = &pcm[offset + slack..offset + slack + sub_window];

        let energy_sum: f64 = c1
            .iter()
            .chain(c2.iter())
            .chain(c3.iter())
            .map(|&s| {
                let v = f64::from(s);
                v * v
            })
            .sum();
        let total_samples = (c1.len() + c2.len() + c3.len()) as f64;
        let rms = (energy_sum / total_samples).sqrt();
        if rms < 45.0 {
            raw_vectors.push(vec![0.0; GOERTZEL_FREQS.len()]);
            flat_flags.push(true);
            offset += samples_per_step;
            continue;
        }

        let b1 = goertzel_band_powers(c1);
        let b2 = goertzel_band_powers(c2);
        let b3 = goertzel_band_powers(c3);

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
    if active_count >= 30 {
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

    Some(build_fingerprint(
        episode_id,
        step_sec,
        &raw_vectors,
        Some(&flat_flags),
        (pcm.len() as f64) / (AUDIO_SAMPLE_RATE as f64),
        "audio",
        0.0,
    ))
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
    let ffmpeg_bin = which::which("ffmpeg").ok()?;
    if !file_path.exists() {
        return None;
    }

    let max_sec_str = (max_sec.max(0.0) as u64).to_string();
    let sample_rate_str = AUDIO_SAMPLE_RATE.to_string();

    let mut cmd = Command::new(ffmpeg_bin);
    cmd.args(["-v", "error", "-t", &max_sec_str, "-i"])
        .arg(file_path)
        .args([
            "-vn",
            "-ac",
            "1",
            "-ar",
            &sample_rate_str,
            "-f",
            "s16le",
            "pipe:1",
        ]);

    let outcome = run_command_with_timeout(&mut cmd, FFMPEG_TIMEOUT, false);
    if !outcome.success() || outcome.stdout.is_empty() {
        return None;
    }

    let usable = outcome.stdout.len() - (outcome.stdout.len() % 2);
    let pcm: Vec<i16> = outcome.stdout[..usable]
        .chunks_exact(2)
        .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]))
        .collect();

    extract_audio_fingerprint_from_pcm(&pcm, episode_id, step_sec)
}
