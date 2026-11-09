//! Signal helpers for the stem separators: STFT/ISTFT, resampling, onsets, envelopes.
#![allow(dead_code, unused_imports, unused_variables)]

pub const SEP_RATE: u32 = 44100;
pub const BP_RATE: u32 = 22050;
pub const N_FFT: usize = 4096;
pub const HOP: usize = 1024;

/// Periodic Hann window 0.5 - 0.5 cos(2 pi i / n).
pub fn hann(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / n as f64).cos()) as f32)
        .collect()
}

/// Index (f * 2 + ch) * 2049 + k.
pub struct Spec {
    pub frames: usize,
    pub re: Vec<f32>,
    pub im: Vec<f32>,
}

const BINS: usize = N_FFT / 2 + 1;

/// Symmetric reflect (numpy "reflect") index into `x`.
fn reflect(x: &[f32], i: isize) -> f32 {
    let n = x.len() as isize;
    if n == 0 {
        return 0.0;
    }
    if n == 1 {
        return x[0];
    }
    let period = 2 * (n - 1);
    let mut k = i.rem_euclid(period);
    if k >= n {
        k = period - k;
    }
    x[k as usize]
}

/// Interleaved stereo in; center reflect-pad N_FFT/2; frames = 1 + len/HOP (len in stereo frames).
pub fn stft(pcm: &[f32]) -> Spec {
    let len = pcm.len() / 2;
    let frames = 1 + len / HOP;
    let win = hann(N_FFT);
    let pad = (N_FFT / 2) as isize;
    let mut re = vec![0f32; frames * 2 * BINS];
    let mut im = vec![0f32; frames * 2 * BINS];
    for ch in 0..2 {
        let x: Vec<f32> = (0..len).map(|i| pcm[2 * i + ch]).collect();
        for f in 0..frames {
            let mut fr: Vec<f32> = (0..N_FFT)
                .map(|j| reflect(&x, (f * HOP + j) as isize - pad) * win[j])
                .collect();
            let mut fi = vec![0f32; N_FFT];
            crate::dash::fft(&mut fr, &mut fi);
            let base = (f * 2 + ch) * BINS;
            re[base..base + BINS].copy_from_slice(&fr[..BINS]);
            im[base..base + BINS].copy_from_slice(&fi[..BINS]);
        }
    }
    Spec { frames, re, im }
}

/// Inverse of `stft`. `len` is the length in stereo frames; the output is interleaved stereo (2 * len samples).
pub fn istft(s: &Spec, len: usize) -> Vec<f32> {
    let win = hann(N_FFT);
    let total = (s.frames.max(1) - 1) * HOP + N_FFT;
    let pad = N_FFT / 2;
    let mut ola = vec![vec![0f32; total]; 2];
    let mut wsum = vec![0f32; total];
    for f in 0..s.frames {
        for ch in 0..2 {
            let base = (f * 2 + ch) * BINS;
            // ifft(X) = conj(fft(conj X)) / N; only the real part is kept.
            let mut re = vec![0f32; N_FFT];
            let mut im = vec![0f32; N_FFT];
            for k in 0..BINS {
                re[k] = s.re[base + k];
                im[k] = -s.im[base + k];
                if k > 0 && k < N_FFT / 2 {
                    re[N_FFT - k] = s.re[base + k];
                    im[N_FFT - k] = s.im[base + k];
                }
            }
            crate::dash::fft(&mut re, &mut im);
            for j in 0..N_FFT {
                ola[ch][f * HOP + j] += re[j] / N_FFT as f32 * win[j];
            }
        }
        for j in 0..N_FFT {
            wsum[f * HOP + j] += win[j] * win[j];
        }
    }
    let mut out = vec![0f32; len * 2];
    for i in 0..len {
        let p = i + pad;
        let w = wsum.get(p).copied().unwrap_or(0.0).max(1e-8);
        for ch in 0..2 {
            out[2 * i + ch] = ola[ch].get(p).copied().unwrap_or(0.0) / w;
        }
    }
    out
}

/// Interleaved stereo in and out; windowed sinc, zero delay.
pub fn resample(pcm: &[f32], from: u32, to: u32) -> Vec<f32> {
    use std::f64::consts::PI;
    let n = pcm.len() / 2;
    let out_n = (n as f64 * to as f64 / from as f64).round() as usize;
    let lo = from.min(to) as f64;
    // Cutoff at 0.9 of the lower rate's Nyquist, in cycles per input sample.
    let fc = 0.9 * lo / 2.0 / from as f64;
    // 16 taps per side at the lower rate, expressed in input samples.
    let half = 16.0 * from as f64 / lo;
    let mut out = vec![0f32; out_n * 2];
    for ch in 0..2 {
        for j in 0..out_n {
            let t = j as f64 * from as f64 / to as f64;
            let (m0, m1) = ((t - half).ceil() as i64, (t + half).floor() as i64);
            let (mut acc, mut norm) = (0f64, 0f64);
            for m in m0..=m1 {
                let d = t - m as f64;
                let w = 0.5 + 0.5 * (PI * d / half).cos();
                let arg = 2.0 * fc * d;
                let sinc = if arg.abs() < 1e-12 { 1.0 } else { (PI * arg).sin() / (PI * arg) };
                let h = 2.0 * fc * sinc * w;
                norm += h;
                if m >= 0 && (m as usize) < n {
                    acc += h * pcm[2 * m as usize + ch] as f64;
                }
            }
            out[2 * j + ch] = (acc / norm) as f32;
        }
    }
    out
}

/// Absolute spectral-flux floor, in amplitude units (see `onsets`). Below it nothing is an onset.
pub const ONSET_FLOOR: f32 = 0.01;

/// Spectral-flux onsets as (time in seconds, strength 0..1).
/// Frames are unpadded, `N` long with hop `H`. Magnitudes are in amplitude units, so a steady sine of
/// amplitude A reads A on a bin centre. `ONSET_FLOOR` makes the peak rules absolute: relative rules
/// alone turn float jitter on a steady tone into onsets.
pub fn onsets(mono: &[f32], rate: usize) -> Vec<(f64, f32)> {
    const N: usize = 1024;
    const H: usize = 256;
    const MIN_GAP_S: f64 = 0.05;
    let win = hann(N);
    let win_sum: f32 = win.iter().sum();
    let bins = N / 2 + 1;
    let nfr = if mono.len() < N { 0 } else { 1 + (mono.len() - N) / H };
    let mut prev = vec![0f32; bins];
    let mut sf = vec![0f32; nfr];
    for i in 0..nfr {
        let mut re: Vec<f32> = (0..N).map(|j| mono[i * H + j] * win[j]).collect();
        let mut im = vec![0f32; N];
        crate::dash::fft(&mut re, &mut im);
        let mag: Vec<f32> = (0..bins)
            .map(|k| 2.0 * (re[k] * re[k] + im[k] * im[k]).sqrt() / win_sum)
            .collect();
        if i > 0 {
            sf[i] = mag.iter().zip(&prev).map(|(a, b)| (a - b).max(0.0)).sum();
        }
        prev = mag;
    }
    let max = sf.iter().cloned().fold(0f32, f32::max);
    if max <= 0.0 {
        return Vec::new();
    }
    let mut out: Vec<(f64, f32)> = Vec::new();
    // Frame 0 has no previous frame and is never an onset; the last frame has no successor.
    for i in 1..nfr.saturating_sub(1) {
        let lo = i.saturating_sub(8);
        let hi = (i + 8).min(nfr - 1);
        let mean = sf[lo..=hi].iter().sum::<f32>() / (hi - lo + 1) as f32;
        let is_peak = sf[i] > sf[i - 1]
            && sf[i] >= sf[i + 1]
            && sf[i] >= 1.5 * mean
            && sf[i] > 0.05 * max
            && sf[i] >= ONSET_FLOOR;
        if !is_peak {
            continue;
        }
        // Time of the frame centre: the window's energy is centred there, not at its start.
        let t = (i * H + N / 2) as f64 / rate as f64;
        let strength = sf[i] / max;
        match out.last_mut() {
            Some(last) if t - last.0 < MIN_GAP_S => {
                if strength > last.1 {
                    *last = (t, strength);
                }
            }
            _ => out.push((t, strength)),
        }
    }
    out
}

/// Mean of (|l| + |r|) / 2 per full block of stereo frames.
pub fn envelope(stereo: &[f32], block: usize) -> Vec<f32> {
    if block == 0 {
        return Vec::new();
    }
    let frames = stereo.len() / 2;
    (0..frames / block)
        .map(|b| {
            let sum: f32 = (b * block..(b + 1) * block)
                .map(|f| (stereo[2 * f].abs() + stereo[2 * f + 1].abs()) / 2.0)
                .sum();
            sum / block as f32
        })
        .collect()
}

fn pearson(x: &[f32], y: &[f32]) -> f32 {
    let n = x.len() as f64;
    let mx = x.iter().map(|&v| v as f64).sum::<f64>() / n;
    let my = y.iter().map(|&v| v as f64).sum::<f64>() / n;
    let (mut cov, mut vx, mut vy) = (0f64, 0f64, 0f64);
    for (&a, &b) in x.iter().zip(y) {
        let (da, db) = (a as f64 - mx, b as f64 - my);
        cov += da * db;
        vx += da * da;
        vy += db * db;
    }
    if vx <= 0.0 || vy <= 0.0 {
        return 0.0;
    }
    (cov / (vx * vy).sqrt()) as f32
}

/// Best Pearson r of a[i] against b[i + L] over L in 0..=max_lag.
pub fn xcorr_lag(a: &[f32], b: &[f32], max_lag: usize) -> (usize, f32) {
    let n = a.len().min(b.len());
    if n <= max_lag {
        return (0, 0.0);
    }
    let span = n - max_lag;
    let mut best = (0usize, f32::MIN);
    for lag in 0..=max_lag {
        let r = pearson(&a[..span], &b[lag..lag + span]);
        if r > best.1 {
            best = (lag, r);
        }
    }
    best
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
        let noise: Vec<f32> = (0..n).map(|_| (lcg(&mut seed) * 2.0 - 1.0) * 0.001).collect();
        let sine: Vec<f32> = (0..n).map(|i| (2.0 * std::f64::consts::PI * 440.0 * i as f64 / 44100.0).sin() as f32 * 0.5).collect();
        // 1 ms clicks (0.9, alternating sign) at 0.25 + 0.5*i s, added on top of `base`.
        let clicks = |base: &[f32]| -> Vec<f32> {
            let mut m = base.to_vec();
            for i in 0..8usize {
                let s0 = ((0.25 + 0.5 * i as f64) * 44100.0).round() as usize;
                for j in 0..44 {
                    m[s0 + j] += 0.9 * if j % 2 == 0 { 1.0 } else { -1.0 };
                }
            }
            m
        };
        for (name, base) in [("noise", noise.as_slice()), ("sine", sine.as_slice())] {
            let on = onsets(&clicks(base), 44100);
            assert_eq!(on.len(), 8, "{name}: onsets {on:?}");
            for (i, (t, s)) in on.iter().enumerate() {
                assert!((t - (0.25 + 0.5 * i as f64)).abs() < 0.01, "{name}: onset {i} at {t}");
                assert!(*s > 0.5, "{name}: onset {i} strength {s}");
            }
        }
        // Sustained tone, noise floor, silence, too short: no fake drum hits, frame 0 included.
        let on = onsets(&sine, 44100);
        assert!(on.is_empty(), "sine: {on:?}");
        let on = onsets(&noise, 44100);
        assert!(on.is_empty(), "noise: {on:?}");
        assert!(onsets(&vec![0.0f32; n], 44100).is_empty());
        assert!(onsets(&[0.1f32; 500], 44100).is_empty());
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
