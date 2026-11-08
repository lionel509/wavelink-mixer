//! Signal helpers for the stem separators: STFT/ISTFT, resampling, onsets, envelopes.
//! Unit 1 skeleton: bodies are `todo!()` until Unit 2.
#![allow(dead_code, unused_imports, unused_variables)]

pub const SEP_RATE: u32 = 44100;
pub const BP_RATE: u32 = 22050;
pub const N_FFT: usize = 4096;
pub const HOP: usize = 1024;

/// Periodic Hann window 0.5 - 0.5 cos(2 pi i / n).
pub fn hann(n: usize) -> Vec<f32> {
    todo!()
}

/// Index (f * 2 + ch) * 2049 + k.
pub struct Spec {
    pub frames: usize,
    pub re: Vec<f32>,
    pub im: Vec<f32>,
}

/// Interleaved stereo in; center reflect-pad N_FFT/2; frames = 1 + len/HOP (len in stereo frames).
pub fn stft(pcm: &[f32]) -> Spec {
    todo!()
}

/// Inverse of `stft`. `len` is the length in stereo frames; the output is interleaved stereo (2 * len samples).
pub fn istft(s: &Spec, len: usize) -> Vec<f32> {
    todo!()
}

/// Interleaved stereo in and out; windowed sinc, zero delay.
pub fn resample(pcm: &[f32], from: u32, to: u32) -> Vec<f32> {
    todo!()
}

/// Spectral-flux onsets as (time in seconds, strength 0..1).
pub fn onsets(mono: &[f32], rate: usize) -> Vec<(f64, f32)> {
    todo!()
}

/// Mean of (|l| + |r|) / 2 per full block of stereo frames.
pub fn envelope(stereo: &[f32], block: usize) -> Vec<f32> {
    todo!()
}

/// Best Pearson r of a[i] against b[i + L] over L in 0..=max_lag.
pub fn xcorr_lag(a: &[f32], b: &[f32], max_lag: usize) -> (usize, f32) {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic uniform [0, 1) from an LCG (no `rand` crate).
    fn lcg(s: &mut u64) -> f32 {
        *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((*s >> 33) as f32) / (1u64 << 31) as f32
    }

    #[test]
    fn hann_is_periodic() {
        let w = hann(8);
        assert_eq!(w.len(), 8);
        assert!(w[0].abs() < 1e-6);
        assert!((w[4] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn stft_frame_count_and_bins() {
        let pcm = vec![0.0f32; 66150 * 2];
        let s = stft(&pcm);
        assert_eq!(s.frames, 65);
        assert_eq!(s.re.len(), 65 * 2 * 2049);
        assert_eq!(s.im.len(), s.re.len());
    }

    #[test]
    fn stft_istft_roundtrip() {
        let mut seed = 7u64;
        let x: Vec<f32> = (0..66150 * 2).map(|_| (lcg(&mut seed) * 2.0 - 1.0) * 0.5).collect();
        let y = istft(&stft(&x), 66150);
        assert_eq!(y.len(), x.len());
        let err = x.iter().zip(&y).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(err < 1e-3, "max err {err}");
    }

    #[test]
    fn stft_sine_peaks_in_right_bin() {
        let x: Vec<f32> = (0..44100)
            .flat_map(|i| {
                let v = (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / 44100.0).sin() as f32 * 0.5;
                [v, v]
            })
            .collect();
        let s = stft(&x);
        let f = s.frames / 2;
        let base = (f * 2) * 2049;
        let (mut best, mut bp) = (0usize, -1.0f32);
        for k in 0..2049 {
            let p = s.re[base + k].powi(2) + s.im[base + k].powi(2);
            if p > bp {
                bp = p;
                best = k;
            }
        }
        assert_eq!(best, 93);
    }

    #[test]
    fn resample_length_rule() {
        assert_eq!(resample(&vec![0.0f32; 88200 * 2], 44100, 22050).len() / 2, 44100);
        assert_eq!(resample(&vec![0.0f32; 72000 * 2], 48000, 44100).len() / 2, 66150);
        assert_eq!(resample(&vec![0.0f32; 66150 * 2], 44100, 48000).len() / 2, 72000);
    }

    #[test]
    fn resample_preserves_tone() {
        let x: Vec<f32> = (0..48000)
            .flat_map(|i| {
                let v = (2.0 * std::f64::consts::PI * 440.0 * i as f64 / 48000.0).sin() as f32 * 0.5;
                [v, v]
            })
            .collect();
        let y = resample(&x, 48000, 44100);
        let l: Vec<f32> = y.iter().step_by(2).cloned().collect();
        let zc = l.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count();
        assert!((878..=882).contains(&zc), "zero crossings {zc}");
        let rms = (l.iter().map(|v| v * v).sum::<f32>() / l.len() as f32).sqrt();
        let want = 0.5 / 2f32.sqrt();
        assert!(((rms - want) / want).abs() < 0.02, "rms {rms} want {want}");
    }

    #[test]
    fn onsets_find_click_train() {
        let n = 4 * 44100;
        let mut seed = 3u64;
        let mut m: Vec<f32> = (0..n).map(|_| (lcg(&mut seed) * 2.0 - 1.0) * 0.001).collect();
        for i in 0..8usize {
            let s0 = (i as f64 * 0.5 * 44100.0).round() as usize;
            for j in 0..44 {
                if s0 + j < n {
                    m[s0 + j] = 0.9 * if j % 2 == 0 { 1.0 } else { -1.0 };
                }
            }
        }
        let on = onsets(&m, 44100);
        assert_eq!(on.len(), 8, "onsets {on:?}");
        for (i, (t, s)) in on.iter().enumerate() {
            assert!((t - 0.5 * i as f64).abs() < 0.01, "onset {i} at {t}");
            assert!(*s > 0.5, "onset {i} strength {s}");
        }
        let sine: Vec<f32> = (0..n).map(|i| (2.0 * std::f64::consts::PI * 440.0 * i as f64 / 44100.0).sin() as f32 * 0.5).collect();
        assert!(onsets(&sine, 44100).is_empty());
    }

    #[test]
    fn envelope_blocks() {
        let x = vec![0.5f32; 2000];
        let e = envelope(&x, 44);
        assert_eq!(e.len(), 22);
        assert!(e.iter().all(|v| (v - 0.5).abs() < 1e-6));
    }

    #[test]
    fn xcorr_finds_shift() {
        let mut seed = 11u64;
        let a: Vec<f32> = (0..5000).map(|_| lcg(&mut seed)).collect();
        let mut b = vec![0.0f32; 37];
        b.extend_from_slice(&a);
        let (lag, r) = xcorr_lag(&a, &b, 200);
        assert_eq!(lag, 37);
        assert!(r > 0.99, "r {r}");
        assert_eq!(xcorr_lag(&a, &a, 200).0, 0);
    }
}
