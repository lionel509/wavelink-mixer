//! Note detection by iterative harmonic subtraction in the complex STFT domain (#23), the
//! "find a note, subtract its wave, repeat" core:
//! per frame (4096-point Hann, zero-phase, hop 256 at 22.05 kHz) on a spectrally whitened complex
//! spectrum:
//!   1. score every key by a weighted harmonic sum (Klapuri weights) on the residual magnitude;
//!   2. take the best, refine f0 from its interpolated partial peaks (search window wide enough for
//!      piano inharmonicity), and estimate each partial's complex amplitude by least squares against
//!      the window's transform, capped by the spectral-smoothness envelope so a partial shared with
//!      another note is not taken whole;
//!   3. subtract those partials (window-shaped complex peaks) from the residual;
//!   4. repeat until the best score falls under a fraction of the frame's first score (max 14 notes).
//!
//! The result per frame is an 88-key activation (the fitted amplitude) plus the fitted partials.
//! Notes come from tracking each key's activation over frames; the vocal line is the
//! centre-weighted, continuity-tracked harmonic series in the voice range, resynthesized from its
//! own partials (inverse STFT) as a vocal stem.
use crate::notes::Note;
use crate::piano::{KEYS, LOW_KEY, RATE};

pub const N: usize = 4096;
pub const HOP: usize = 256;
/// The long window for bass pitch decisions (< 110 Hz): it ends where the short one ends, so it
/// adds no latency.
pub const LONG: usize = 8192;
/// The short window for per-key attack evidence.
pub const SHORT: usize = 1024;
const KMAX: usize = 1000; // ~5.4 kHz
const MAX_NOTES: usize = 14;

/// The precision defaults (pursuit stops at 30 % of the first score or when a note would explain
/// < 10 % of the residual, 6 voices in the pianist); MIXER_PIANO_PRECISION=0 restores 2e48eb7's.
pub fn precision() -> bool {
    env_on("MIXER_PIANO_PRECISION", true)
}

/// Longer holds: decoding lag 465 ms (was 300) and 8 pianist voices (was 6);
/// MIXER_PIANO_LEGATO=0 restores e9cac14's.
pub fn legato() -> bool {
    env_on("MIXER_PIANO_LEGATO", true)
}

/// Runtime A/B switches for the live path: "0"/"1" (unset: the default).
pub fn env_on(name: &str, def: bool) -> bool {
    match std::env::var(name).as_deref() {
        Ok("0") | Ok("off") | Ok("false") => false,
        Ok("1") | Ok("on") | Ok("true") => true,
        _ => def,
    }
}

fn tun(name: &str, def: f32) -> f32 {
    std::env::var("SUB_TUNE")
        .ok()
        .and_then(|s| s.split(',').find_map(|kv| kv.split_once('=').filter(|(k, _)| k.trim() == name).and_then(|(_, v)| v.trim().parse().ok())))
        .unwrap_or(def)
}

/// In-place radix-2 complex FFT (forward when `inv` is false).
pub struct Fft {
    n: usize,
    rev: Vec<usize>,
    tw: Vec<(f32, f32)>,
}

impl Fft {
    pub fn new(n: usize) -> Fft {
        let bits = n.trailing_zeros();
        let rev = (0..n).map(|i| i.reverse_bits() >> (usize::BITS - bits)).collect();
        let tw = (0..n / 2).map(|k| {
            let a = -2.0 * std::f64::consts::PI * k as f64 / n as f64;
            (a.cos() as f32, a.sin() as f32)
        });
        Fft { n, rev, tw: tw.collect() }
    }
    pub fn run(&self, re: &mut [f32], im: &mut [f32], inv: bool) {
        let n = self.n;
        for i in 0..n {
            let j = self.rev[i];
            if j > i {
                re.swap(i, j);
                im.swap(i, j);
            }
        }
        let mut len = 2;
        while len <= n {
            let step = n / len;
            for s in (0..n).step_by(len) {
                for k in 0..len / 2 {
                    let (c, mut d) = self.tw[k * step];
                    if inv {
                        d = -d;
                    }
                    let (a, b) = (s + k, s + k + len / 2);
                    let (xr, xi) = (re[b] * c - im[b] * d, re[b] * d + im[b] * c);
                    re[b] = re[a] - xr;
                    im[b] = im[a] - xi;
                    re[a] += xr;
                    im[a] += xi;
                }
            }
            len <<= 1;
        }
    }
}

/// Hann window transform (zero-phase, real) at an offset of `d` bins, normalised to 1 at d = 0.
fn wk(d: f32) -> f32 {
    let sinc = |x: f32| if x.abs() < 1e-6 { 1.0 } else { (std::f32::consts::PI * x).sin() / (std::f32::consts::PI * x) };
    sinc(d) + 0.5 * (sinc(d - 1.0) + sinc(d + 1.0))
}

#[derive(Clone, Copy, Default)]
pub struct Partial {
    /// Fractional bin.
    pub pos: f32,
    pub re: f32,
    pub im: f32,
}

#[derive(Clone)]
pub struct Found {
    pub key: u8,
    pub f0: f32,
    pub amp: f32,
    pub partials: Vec<Partial>,
}

pub struct Frames {
    pub frame_s: f64,
    /// Seconds of frame 0's centre.
    pub t0: f64,
    pub act: Vec<[f32; KEYS]>,
    pub found: Vec<Vec<Found>>,
    /// Per frame, per bin up to KMAX: stereo centre similarity 0..1.
    pub centre: Vec<Vec<f32>>,
    /// Per frame, per bin: the whitening gain applied (to undo it for resynthesis).
    pub gain: Vec<Vec<f32>>,
}

fn hann(n: usize) -> Vec<f32> {
    (0..n).map(|i| (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / n as f64).cos()) as f32).collect()
}

/// Whitening gains per bin from the magnitude (smoothed over ±30 bins, gain = smooth^(nu-1)).
fn whiten(mag: &[f32], out: &mut [f32]) {
    let nu = tun("NU", 0.33);
    let w = 30usize;
    let n = mag.len();
    let mut pre = vec![0f64; n + 1];
    for i in 0..n {
        pre[i + 1] = pre[i] + (mag[i] * mag[i]) as f64;
    }
    for i in 0..n {
        let (a, b) = (i.saturating_sub(w), (i + w + 1).min(n));
        let s = ((pre[b] - pre[a]) / (b - a) as f64).sqrt() as f32 + 1e-7;
        out[i] = s.powf(nu - 1.0);
    }
}

fn key_hz(k: usize) -> f32 {
    440.0 * 2f32.powf((LOW_KEY as f32 + k as f32 - 69.0) / 12.0)
}

/// Peak near fractional bin `c` within ±`w` bins on `mag`: (interpolated pos, magnitude).
fn peak(mag: &[f32], c: f32, w: f32) -> Option<(f32, f32)> {
    let (a, b) = (((c - w).floor().max(1.0)) as usize, ((c + w).ceil() as usize).min(mag.len() - 2));
    if a >= b {
        return None;
    }
    let i = (a..=b).max_by(|&x, &y| mag[x].total_cmp(&mag[y]))?;
    let (l, m, r) = (mag[i - 1].max(1e-12).ln(), mag[i].max(1e-12).ln(), mag[i + 1].max(1e-12).ln());
    let den = l - 2.0 * m + r;
    let d = if den.abs() > 1e-9 { (0.5 * (l - r) / den).clamp(-0.5, 0.5) } else { 0.0 };
    Some((i as f32 + d, mag[i]))
}

fn n_harm(f0: f32, df: f32) -> usize {
    ((KMAX as f32 * df * 0.95 / f0) as usize).clamp(1, 16)
}

/// Harmonic-sum score of key `k` on `mag`.
fn score(mag: &[f32], k: usize, df: f32) -> f32 {
    let f0 = key_hz(k);
    let mut s = 0.0;
    for h in 1..=n_harm(f0, df) {
        let c = h as f32 * f0 * (1.0 + 0.0004 * (h * h) as f32).sqrt() / df;
        let w = (0.03 * c).max(1.0);
        if let Some((_, a)) = peak(mag, c, w) {
            s += (f0 + 27.0) / (h as f32 * f0 + 320.0) * a;
        }
    }
    s
}

/// Analyses stereo PCM at RATE.
/// One frame of the subtraction: FFT, whitening, pursuit.
pub struct Analyzer {
    fft: Fft,
    win: Vec<f32>,
    lr: Vec<f32>,
    li: Vec<f32>,
    rr: Vec<f32>,
    ri: Vec<f32>,
    mag: Vec<f32>,
    g: Vec<f32>,
    df: f32,
    stop_rel: f32,
    fft2: Fft,
    win2: Vec<f32>,
    re2: Vec<f32>,
    im2: Vec<f32>,
    mag2: Vec<f32>,
    long_ok: bool,
    drums: Option<crate::drums::Drums>,
    attack: bool,
    sfft: Fft,
    swin: Vec<f32>,
    sre: Vec<f32>,
    sim: Vec<f32>,
    bass_long: bool,
    cluster: bool,
    /// The last 16 short frames' per-band (drum energy, total energy), newest first.
    dring: Vec<[[f32; 48]; 2]>,
}

pub struct FrameOut {
    pub act: [f32; KEYS],
    /// The harmonic score of every key before any subtraction (the raw per-key spectrum).
    pub raw: [f32; KEYS],
    pub found: Vec<Found>,
    pub cen: Vec<f32>,
    pub gain: Vec<f32>,
    /// Drum hits found in the short frame that ends with this window: (GM note, velocity); its
    /// centre is DRUM_LEAD_S after this frame's centre.
    pub hits: Vec<(u8, u8)>,
    /// Audio chroma of this frame (60 Hz-4 kHz, before whitening and subtraction).
    pub chroma: [f32; 12],
    /// Per key, short-window (1024) energy of partials 1-4, dB: attack evidence.
    pub short: [f32; KEYS],
}

/// The drum frame's centre relative to the analysis frame's centre.
pub const DRUM_LEAD_S: f64 = (N / 2 - crate::drums::SN / 2) as f64 / RATE as f64;

impl Default for Analyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl Analyzer {
    pub fn new() -> Analyzer {
        Analyzer {
            fft: Fft::new(N),
            win: hann(N),
            lr: vec![0.0; N],
            li: vec![0.0; N],
            rr: vec![0.0; N],
            ri: vec![0.0; N],
            mag: vec![0.0; KMAX],
            g: vec![0.0; KMAX],
            df: RATE as f32 / N as f32,
            stop_rel: tun("STOP_REL", if precision() { 0.3 } else { 0.22 }),
            fft2: Fft::new(LONG),
            win2: hann(LONG),
            re2: vec![0.0; LONG],
            im2: vec![0.0; LONG],
            mag2: vec![0.0; LONG / 8],
            long_ok: false,
            drums: if env_on("MIXER_PIANO_DRUMS", false) { Some(crate::drums::Drums::new()) } else { None },
            attack: env_on("MIXER_PIANO_ATTACK", true),
            sfft: Fft::new(SHORT),
            swin: hann(SHORT),
            sre: vec![0.0; SHORT],
            sim: vec![0.0; SHORT],
            bass_long: env_on("MIXER_PIANO_BASSWIN", true),
            cluster: env_on("MIXER_PIANO_CLUSTER", true),
            dring: vec![[[0.0; 48]; 2]; 16],
        }
    }

    /// `get(i)` gives the (left, right) sample i of the N-sample window (i = 0 is N/2 before the
    /// frame centre; down to N - LONG for the bass window); with `stereo` false only the left is read and the centre is all ones.
    pub fn analyse(&mut self, get: impl Fn(isize) -> (f32, f32), stereo: bool) -> FrameOut {
        self.long_ok = false;
        for i in 0..N {
            let (a, b) = get(i as isize);
            // zero-phase: rotate by N/2 so phases refer to the frame centre and W is real
            let j = (i + N / 2) % N;
            self.lr[j] = a * self.win[i];
            self.li[j] = 0.0;
            if stereo {
                self.rr[j] = b * self.win[i];
                self.ri[j] = 0.0;
            }
        }
        self.fft.run(&mut self.lr, &mut self.li, false);
        if stereo {
            self.fft.run(&mut self.rr, &mut self.ri, false);
        } else {
            self.rr.copy_from_slice(&self.lr);
            self.ri.copy_from_slice(&self.li);
        }
        // attack evidence: a 1024-point window centred on the frame, per key the strongest bins near
        // partials 1-4 (dB); its rise over a few hops is a key's own attack (the long window smears it)
        let mut short = [-120f32; KEYS];
        if self.attack {
            let sn = SHORT;
            let off = (N / 2 - sn / 2) as isize;
            for i in 0..sn {
                let (a, b) = get(off + i as isize);
                self.sre[i] = if stereo { 0.5 * (a + b) } else { a } * self.swin[i];
                self.sim[i] = 0.0;
            }
            self.sfft.run(&mut self.sre, &mut self.sim, false);
            let sdf = RATE as f32 / sn as f32;
            let sm: Vec<f32> = (0..sn / 2).map(|k| (self.sre[k] * self.sre[k] + self.sim[k] * self.sim[k]).sqrt()).collect();
            for k in 0..KEYS {
                let f0 = key_hz(k);
                let mut e = 0f32;
                for h in 1..=4 {
                    let c = h as f32 * f0 / sdf;
                    if c + 1.0 >= (sn / 2) as f32 {
                        break;
                    }
                    let b0 = c.round() as usize;
                    e += sm[b0.saturating_sub(1)..=(b0 + 1).min(sn / 2 - 1)].iter().fold(0f32, |m, &v| m.max(v)).powi(2);
                }
                short[k] = 10.0 * e.max(1e-14).log10();
            }
        }
        let mut cen = vec![0f32; KMAX];
        let (mut xr, mut xi) = (vec![0f32; KMAX], vec![0f32; KMAX]);
        for k in 0..KMAX {
            let (ar, ai, br, bi) = (self.lr[k], self.li[k], self.rr[k], self.ri[k]);
            let cross = ar * br + ai * bi; // Re(L conj R)
            let pw = ar * ar + ai * ai + br * br + bi * bi;
            cen[k] = (2.0 * cross / pw.max(1e-20)).max(0.0);
            xr[k] = 0.5 * (ar + br);
            xi[k] = 0.5 * (ai + bi);
            self.mag[k] = (xr[k] * xr[k] + xi[k] * xi[k]).sqrt();
        }
        // drums: a short frame ending where this window ends, fitted with the drum templates; the
        // drum share of each band over this window (Hann-weighted over the last 16 short frames)
        // is taken out of every bin that is not a spectral peak, before whitening and the pursuit
        let mut hits = Vec::new();
        if let Some(d) = self.drums.as_mut() {
            let off = (N - crate::drums::SN) as isize;
            let df = d.frame(
                |i| {
                    let (a, b) = get(off + i as isize);
                    if stereo { 0.5 * (a + b) } else { a }
                },
                None,
            );
            hits = df.hits.clone();
            self.dring.rotate_right(1);
            let mut e = [[0f32; 48]; 2];
            for b in 0..48 {
                let dm = df.band_frac[b] * df.band_mag[b];
                e[0][b] = dm * dm;
                e[1][b] = df.band_mag[b] * df.band_mag[b];
            }
            self.dring[0] = e;
            let lead = ((N / 2 - crate::drums::SN / 2) / HOP) as isize;
            let mut phi = [0f32; 48];
            let (mut ed, mut et) = ([0f32; 48], [0f32; 48]);
            for (age, e) in self.dring.iter().enumerate() {
                let pos = N as isize / 2 + (lead - age as isize) * HOP as isize;
                if pos <= 0 || pos >= N as isize {
                    continue;
                }
                let w = self.win[pos as usize];
                let w2 = w * w;
                for b in 0..48 {
                    ed[b] += w2 * e[0][b];
                    et[b] += w2 * e[1][b];
                }
            }
            for b in 0..48 {
                phi[b] = if et[b] > 0.0 { (ed[b] / et[b]).sqrt().clamp(0.0, 1.0) } else { 0.0 };
            }
            let mut pre = vec![0f32; KMAX + 1];
            for k in 0..KMAX {
                pre[k + 1] = pre[k] + self.mag[k];
            }
            let lnr = (11000.0f32 / 30.0).ln();
            for k in 1..KMAX {
                let f = k as f32 * self.df;
                if f < 30.0 {
                    continue;
                }
                let b = ((48.0 * (f / 30.0).ln() / lnr) as usize).min(47);
                if phi[b] <= 0.0 {
                    continue;
                }
                let (lo, hi) = (k.saturating_sub(8), (k + 9).min(KMAX));
                let local = (pre[hi] - pre[lo]) / (hi - lo) as f32;
                let peak = ((self.mag[k] / local.max(1e-12) - 1.0) / 3.0).clamp(0.0, 1.0);
                let m = (1.0 - phi[b] * (1.0 - peak)).max(0.05);
                xr[k] *= m;
                xi[k] *= m;
                self.mag[k] *= m;
            }
        }
        let mut chroma = [0f32; 12];
        for k in ((60.0 / self.df) as usize).max(1)..((4000.0 / self.df) as usize).min(KMAX) {
            let pc = ((69.0 + 12.0 * ((k as f32 * self.df) / 440.0).log2()).round() as i32).rem_euclid(12) as usize;
            chroma[pc] += self.mag[k].sqrt();
        }
        whiten(&self.mag, &mut self.g);
        for k in 0..KMAX {
            xr[k] *= self.g[k];
            xi[k] *= self.g[k];
            self.mag[k] = (xr[k] * xr[k] + xi[k] * xi[k]).sqrt();
        }
        let mut act = [0f32; KEYS];
        let mut raw = [0f32; KEYS];
        let mut found: Vec<Found> = Vec::new();
        let mut first = 0f32;
        let mut taken = [false; KEYS];
        let mut got = [false; KEYS];
        // precision: at most (distinct peaks below 1 kHz) + 1 notes (PEAKCAP=1), and stop when the
        // next note would explain less than EXPL of the residual energy (EXPL > 0)
        let mut cap = MAX_NOTES;
        if tun("PEAKCAP", 0.0) > 0.0 {
            let k1 = (1000.0 / self.df) as usize;
            let mx = self.mag[2..k1].iter().fold(0f32, |m, &v| m.max(v));
            let mut n = 0;
            for k in 3..k1 {
                let m = self.mag[k];
                if m > 0.03 * mx && m >= self.mag[k - 1] && m >= self.mag[k + 1] && m >= self.mag[k - 2] && m >= self.mag[k + 2] {
                    let (lo, hi) = (k.saturating_sub(12), (k + 13).min(KMAX));
                    let mean = self.mag[lo..hi].iter().sum::<f32>() / (hi - lo) as f32;
                    if m > 2.0 * mean {
                        n += 1;
                    }
                }
            }
            cap = (n + 1).min(MAX_NOTES);
        }
        let expl = tun("EXPL", if precision() { 0.1 } else { 0.0 });
        for _ in 0..cap {
            let mut best = (0f32, usize::MAX);
            for k in 0..KEYS {
                if taken[k] || key_hz(k) > 2100.0 {
                    continue;
                }
                let mut s = score(&self.mag, k, self.df);
                if first == 0.0 {
                    raw[k] = s;
                }
                // an octave (or twelfth) over a note already found needs more evidence: its partials
                // were that note's even ones
                if (k >= 12 && got[k - 12]) || (k >= 19 && got[k - 19]) {
                    s *= tun("OCT_PEN", 0.6);
                }
                if s > best.0 {
                    best = (s, k);
                }
            }
            if best.1 == usize::MAX || best.0 <= 0.0 {
                break;
            }
            // against noise: the same weights on the residual's mean level (a flat spectrum scores this x ~2)
            let lvl = self.mag[10..KMAX].iter().sum::<f32>() / (KMAX - 10) as f32;
            let f0b = key_hz(best.1);
            let wsum: f32 = (1..=n_harm(f0b, self.df)).map(|h| (f0b + 27.0) / (h as f32 * f0b + 320.0)).sum();
            if best.0 < tun("SNR", 4.0) * lvl * wsum {
                break;
            }
            if first == 0.0 {
                first = best.0;
            } else if best.0 < self.stop_rel * first {
                break;
            }
            let mut k = best.1;
            // refine f0 from the partial peaks
            let f0n = key_hz(k);
            let nh = n_harm(f0n, self.df);
            let mut pk: Vec<Option<(f32, f32)>> = Vec::with_capacity(nh);
            let (mut num, mut den) = (0f32, 0f32);
            for h in 1..=nh {
                let c = h as f32 * f0n * (1.0 + 0.0004 * (h * h) as f32).sqrt() / self.df;
                let p = peak(&self.mag, c, (0.03 * c).max(1.0));
                if let Some((pos, a)) = p {
                    if h <= 10 {
                        // higher partials resolve f0 finer: weight by h (a * h * f_h / h), stretch removed
                        num += a * pos * self.df / (1.0 + 0.0002 * (h * h) as f32).sqrt();
                        den += a * h as f32;
                    }
                }
                pk.push(p);
            }
            let f0 = if den > 0.0 { num / den } else { f0n };
            // the key the refined f0 rounds to; no semitone neighbours in the same frame
            let kr = (69.0 + 12.0 * (f0 / 440.0).log2()).round() as i32 - LOW_KEY as i32;
            if (0..KEYS as i32).contains(&kr) && (kr - k as i32).abs() == 1 && !taken[kr as usize] {
                k = kr as usize;
            }
            // bass: decide between the semitone neighbours on the long window (2.7 Hz bins)
            if self.bass_long && key_hz(k) < 110.0 {
                if !self.long_ok {
                    let off = N as isize - LONG as isize;
                    for i in 0..LONG {
                        let (a, b) = get(off + i as isize);
                        self.re2[i] = if stereo { 0.5 * (a + b) } else { a } * self.win2[i];
                        self.im2[i] = 0.0;
                    }
                    self.fft2.run(&mut self.re2, &mut self.im2, false);
                    for i in 0..self.mag2.len() {
                        self.mag2[i] = (self.re2[i] * self.re2[i] + self.im2[i] * self.im2[i]).sqrt();
                    }
                    self.long_ok = true;
                }
                let df2 = RATE as f32 / LONG as f32;
                let sc = |c: usize| -> f32 {
                    let f = key_hz(c);
                    (1..=8)
                        .map(|h| {
                            let x = h as f32 * f * (1.0 + 0.0002 * (h * h) as f32).sqrt() / df2;
                            peak(&self.mag2, x, (0.015 * x).max(0.8)).map_or(0.0, |p| p.1) / (h as f32).sqrt()
                        })
                        .sum()
                };
                let mut bk = (sc(k), k);
                for c in [k.wrapping_sub(1), k + 1] {
                    if c < KEYS && !got[c] {
                        let v = sc(c);
                        if v > bk.0 * 1.15 {
                            bk = (v, c);
                        }
                    }
                }
                k = bk.1;
            }
            // semitone neighbours: below CLUSTER_HZ they are leakage of this note (the bins cannot
            // separate them); above, a 9th/cluster voicing (D#4 + E4) is real and both may sound
            if !self.cluster || key_hz(k) < tun("CLUSTER_HZ", 150.0) {
                for j in k.saturating_sub(1)..=(k + 1).min(KEYS - 1) {
                    taken[j] = true;
                }
            }
            taken[best.1] = true;
            got[k] = true;
            // complex amplitudes by LS against the window transform over ±2 bins
            let mut parts: Vec<Partial> = Vec::with_capacity(nh);
            for p in pk.iter().flatten() {
                let pos = p.0;
                let (mut sr, mut si, mut ww) = (0f32, 0f32, 0f32);
                let ic = pos.round() as isize;
                for kk in ic - 2..=ic + 2 {
                    if kk < 0 || kk as usize >= KMAX {
                        continue;
                    }
                    let w = wk(kk as f32 - pos);
                    sr += xr[kk as usize] * w;
                    si += xi[kk as usize] * w;
                    ww += w * w;
                }
                parts.push(Partial { pos, re: sr / ww.max(1e-9), im: si / ww.max(1e-9) });
            }
            // spectral smoothness: cap each partial at 1.5x its neighbours' mean (shared partials)
            let mags: Vec<f32> = parts.iter().map(|p| (p.re * p.re + p.im * p.im).sqrt()).collect();
            for i in 0..parts.len() {
                let nb: Vec<f32> = [i.wrapping_sub(1), i + 1].iter().filter_map(|&j| mags.get(j).copied()).collect();
                if nb.is_empty() || i == 0 {
                    continue;
                }
                let cap = tun("CAP", 1.8) * nb.iter().sum::<f32>() / nb.len() as f32;
                if mags[i] > cap && mags[i] > 0.0 {
                    let s = cap / mags[i];
                    parts[i].re *= s;
                    parts[i].im *= s;
                }
            }
            // subtract
            for p in &parts {
                let ic = p.pos.round() as isize;
                for kk in ic - 3..=ic + 3 {
                    if kk < 0 || kk as usize >= KMAX {
                        continue;
                    }
                    let w = wk(kk as f32 - p.pos);
                    xr[kk as usize] -= p.re * w;
                    xi[kk as usize] -= p.im * w;
                    self.mag[kk as usize] = (xr[kk as usize].powi(2) + xi[kk as usize].powi(2)).sqrt();
                }
            }
            let amp = parts.iter().map(|p| p.re * p.re + p.im * p.im).sum::<f32>().sqrt();
            if expl > 0.0 && !found.is_empty() {
                // residual energy before this note's subtraction = after + its own share
                let rest: f32 = self.mag[10..KMAX].iter().map(|v| v * v).sum::<f32>();
                let mine = amp * amp * 1.5;
                if mine < expl * (rest + mine) {
                    break;
                }
            }
            act[k] = amp;
            found.push(Found { key: (LOW_KEY as usize + k) as u8, f0, amp, partials: parts });
        }
        FrameOut { act, raw, found, cen, gain: self.g.clone(), hits, chroma, short }
    }
}

pub fn frames(l: &[f32], r: &[f32]) -> Frames {
    let len = l.len().min(r.len());
    let nfr = len / HOP + 1;
    let mut an = Analyzer::new();
    let mut out = Frames {
        frame_s: HOP as f64 / RATE as f64,
        t0: 0.0,
        act: Vec::with_capacity(nfr),
        found: Vec::with_capacity(nfr),
        centre: Vec::with_capacity(nfr),
        gain: Vec::with_capacity(nfr),
    };
    for f in 0..nfr {
        let c0 = (f * HOP) as isize - (N / 2) as isize;
        let fo = an.analyse(
            |i| {
                let s = c0 + i;
                if s >= 0 && (s as usize) < len {
                    (l[s as usize], r[s as usize])
                } else {
                    (0.0, 0.0)
                }
            },
            true,
        );
        out.act.push(fo.act);
        out.found.push(fo.found);
        out.centre.push(fo.cen);
        out.gain.push(fo.gain);
    }
    out
}

/// Notes from the per-key activations: onset on a rise to above ON_DB under the running global
/// reference (or a 5 dB jump: a re-strike), off when it falls 18 dB under its own peak or under OFF_DB.
pub fn notes(fr: &Frames) -> Vec<Note> {
    let on_db = tun("ON_DB", -26.0);
    let off_db = tun("OFF_DB", -36.0);
    let n = fr.act.len();
    let db = |v: f32| 20.0 * v.max(1e-9).log10();
    // global reference: the 99th percentile frame maximum
    let mut mx: Vec<f32> = fr.act.iter().map(|a| a.iter().fold(0f32, |m, &v| m.max(v))).collect();
    mx.sort_by(|a, b| a.total_cmp(b));
    let gref = db(mx.get(mx.len() * 99 / 100).copied().unwrap_or(1.0));
    let mut out = Vec::new();
    for k in 0..KEYS {
        // 3-frame smoothing
        let a: Vec<f32> = (0..n)
            .map(|i| {
                let s: f32 = (i.saturating_sub(1)..(i + 2).min(n)).map(|j| fr.act[j][k]).sum();
                db(s / 3.0) - gref
            })
            .collect();
        let mut on: Option<(usize, f32)> = None;
        let mut below = 0;
        for i in 0..n {
            let v = a[i];
            let rise = i >= 3 && v - a[i - 3] > 5.0;
            match on {
                None => {
                    if v > on_db && (rise || i < 3 || a[i.saturating_sub(3)] < on_db) {
                        on = Some((i, v));
                    }
                }
                Some((s, pk)) => {
                    let pk = pk.max(v);
                    on = Some((s, pk));
                    if rise && i - s > 8 && v > on_db {
                        out.push(mk(fr, k, s, i, pk));
                        on = Some((i, v));
                        below = 0;
                        continue;
                    }
                    if v < off_db || v < pk - 18.0 {
                        below += 1;
                    } else {
                        below = 0;
                    }
                    if below >= 3 {
                        out.push(mk(fr, k, s, i - 2, pk));
                        on = None;
                        below = 0;
                    }
                }
            }
        }
        if let Some((s, pk)) = on {
            out.push(mk(fr, k, s, n, pk));
        }
    }
    // weak and short: transient debris (attacks, releases)
    let weak_vel = (100.0 + 2.0 * tun("WEAK_DB", -14.0)) as u8;
    out.retain(|n| n.dur >= tun("MIN_DUR", 0.12) as f64 && (n.vel >= weak_vel || n.dur >= tun("WEAK_DUR", 0.25) as f64));
    out.sort_by(|a, b| a.t.total_cmp(&b.t).then(a.pitch.cmp(&b.pitch)));
    out
}

fn mk(fr: &Frames, k: usize, s: usize, e: usize, pk_db: f32) -> Note {
    // the window sees an attack up to N/2 early: start at the steepest rise within the next 8 frames
    let a = |i: usize| fr.act.get(i).map_or(0.0, |x| x[k]);
    let s = (s..(s + 8).min(e.saturating_sub(1)).max(s + 1)).max_by(|&x, &y| (a(x + 1) - a(x)).total_cmp(&(a(y + 1) - a(y)))).unwrap_or(s);
    let vel = (100.0 + 2.0 * pk_db).clamp(25.0, 120.0) as u8;
    Note { t: fr.t0 + s as f64 * fr.frame_s, dur: (e - s) as f64 * fr.frame_s, pitch: (LOW_KEY as usize + k) as u8, vel }
}

/// The vocal line: per frame the found series in 120..1000 Hz with the best centre-weighted amplitude,
/// chosen with a pitch-continuity cost (DP); returns per frame the index into `found[f]`.
pub fn vocal_line(fr: &Frames) -> Vec<Option<usize>> {
    let n = fr.found.len();
    let df = RATE as f32 / N as f32;
    let cands: Vec<Vec<(usize, f32, f32)>> = (0..n)
        .map(|f| {
            fr.found[f]
                .iter()
                .enumerate()
                .filter(|(_, x)| (120.0..1000.0).contains(&x.f0))
                .map(|(i, x)| {
                    let (mut c, mut w) = (0f32, 0f32);
                    for p in &x.partials {
                        let a = (p.re * p.re + p.im * p.im).sqrt();
                        let b = (p.pos.round() as usize).min(KMAX - 1);
                        c += a * fr.centre[f][b];
                        w += a;
                    }
                    let cen = if w > 0.0 { c / w } else { 0.0 };
                    let _ = df;
                    (i, x.f0, x.amp * cen.powi(3))
                })
                .collect()
        })
        .collect();
    let mut amps: Vec<f32> = cands.iter().flat_map(|c| c.iter().map(|x| x.2)).collect();
    amps.sort_by(|a, b| a.total_cmp(b));
    let r = amps.get(amps.len() * 95 / 100).copied().unwrap_or(1.0).max(1e-9);
    let sil = tun("V_SIL", 0.25);
    // DP: states = candidates + silence
    let mut acc: Vec<f32> = Vec::new();
    let mut back: Vec<Vec<usize>> = Vec::with_capacity(n);
    let mut prev: Vec<(usize, f32, f32)> = Vec::new();
    for f in 0..n {
        let c = &cands[f];
        let mut next = vec![0f32; c.len() + 1];
        let mut bk = vec![0usize; c.len() + 1];
        for j in 0..=c.len() {
            let obs = if j == c.len() { sil } else { (c[j].2 / r).min(1.5) };
            let mut best = (f32::NEG_INFINITY, 0usize);
            if f == 0 {
                best = (0.0, 0);
            } else {
                for i in 0..=prev.len() {
                    let cost = if i == prev.len() || j == c.len() {
                        0.15
                    } else {
                        let cents = 1200.0 * (c[j].1 / prev[i].1).log2().abs();
                        (cents / 1200.0).min(1.0) * 0.8
                    };
                    let v = acc[i] - cost;
                    if v > best.0 {
                        best = (v, i);
                    }
                }
            }
            next[j] = best.0 + obs;
            bk[j] = best.1;
        }
        acc = next;
        back.push(bk);
        prev = c.clone();
    }
    let mut out = vec![None; n];
    if n == 0 {
        return out;
    }
    let mut j = (0..acc.len()).max_by(|&a, &b| acc[a].total_cmp(&acc[b])).unwrap_or(0);
    for f in (0..n).rev() {
        out[f] = if j < cands[f].len() { Some(cands[f][j].0) } else { None };
        j = back[f][j];
    }
    out
}

/// Resynthesizes the chosen series per frame (unwhitened) by inverse STFT, overlap-add.
pub fn resynth(fr: &Frames, line: &[Option<usize>], len: usize) -> Vec<f32> {
    let fft = Fft::new(N);
    let win = hann(N);
    let mut out = vec![0f32; len + N];
    let mut norm = vec![0f32; len + N];
    let (mut re, mut im) = (vec![0f32; N], vec![0f32; N]);
    for (f, sel) in line.iter().enumerate() {
        re.iter_mut().for_each(|v| *v = 0.0);
        im.iter_mut().for_each(|v| *v = 0.0);
        if let Some(i) = sel {
            for p in &fr.found[f][*i].partials {
                let ic = p.pos.round() as isize;
                for kk in ic - 3..=ic + 3 {
                    if kk <= 0 || kk as usize >= KMAX {
                        continue;
                    }
                    let k = kk as usize;
                    let w = wk(kk as f32 - p.pos) / fr.gain[f][k].max(1e-9);
                    re[k] += p.re * w;
                    im[k] += p.im * w;
                    re[N - k] = re[k];
                    im[N - k] = -im[k];
                }
            }
        }
        fft.run(&mut re, &mut im, true);
        let c0 = (f * HOP) as isize - (N / 2) as isize;
        for i in 0..N {
            let s = c0 + i as isize;
            if s < 0 || s as usize >= len {
                continue;
            }
            let j = (i + N / 2) % N;
            out[s as usize] += re[j] / N as f32 * win[i];
            norm[s as usize] += win[i] * win[i];
        }
    }
    out.truncate(len);
    for (o, n) in out.iter_mut().zip(norm) {
        *o /= n.max(1e-3);
    }
    out
}

/// Melody notes from the vocal line: f0 to the nearest semitone (glides collapse), segments of the
/// same semitone >= 80 ms, gaps < 60 ms bridged.
pub fn line_notes(fr: &Frames, line: &[Option<usize>]) -> Vec<Note> {
    let mut out: Vec<Note> = Vec::new();
    let mut cur: Option<(usize, u8, f32)> = None;
    let push = |out: &mut Vec<Note>, s: usize, e: usize, p: u8, a: f32| {
        let dur = (e - s) as f64 * fr.frame_s;
        if dur >= 0.08 {
            out.push(Note { t: fr.t0 + s as f64 * fr.frame_s, dur, pitch: p, vel: (60.0 + 60.0 * a.min(1.0)) as u8 });
        }
    };
    let mut amax = 1e-9f32;
    for (f, sel) in line.iter().enumerate() {
        if let Some(i) = sel {
            amax = amax.max(fr.found[f][*i].amp);
        }
    }
    let mut dip = f32::INFINITY;
    for (f, sel) in line.iter().enumerate() {
        let p = sel.map(|i| {
            let x = &fr.found[f][i];
            ((69.0 + 12.0 * (x.f0 / 440.0).log2()).round() as u8, x.amp / amax)
        });
        let prev_start = cur.map(|c| c.0);
        match (cur, p) {
            (Some((s, cp, a)), Some((np, na))) if np == cp => {
                // a syllable: the same pitch dips under half its peak and comes back up
                if na > 0.8 * a && dip < 0.5 * a && f - s > 6 {
                    push(&mut out, s, f, cp, a);
                    cur = Some((f, np, na));
                } else {
                    cur = Some((s, cp, a.max(na)));
                }
            }
            (Some((s, cp, a)), _) => {
                push(&mut out, s, f, cp, a);
                cur = p.map(|(np, na)| (f, np, na));
            }
            (None, Some((np, na))) => cur = Some((f, np, na)),
            (None, None) => {}
        }
        if cur.map(|c| c.0) != prev_start {
            dip = f32::INFINITY;
        }
        if let Some((_, na)) = p {
            dip = dip.min(na);
            if cur.is_some_and(|c| na >= c.2) {
                dip = na;
            }
        }
    }
    if let Some((s, cp, a)) = cur {
        push(&mut out, s, line.len(), cp, a);
    }
    // bridge gaps < 60 ms between equal pitches
    let mut merged: Vec<Note> = Vec::new();
    for n in out {
        match merged.last_mut() {
            Some(l) if l.pitch == n.pitch && (1e-6..0.06).contains(&(n.t - (l.t + l.dur))) => l.dur = n.t + n.dur - l.t,
            _ => merged.push(n),
        }
    }
    merged
}

/// Stereo WAV -> (left, right) at `rate` (mono files give the same channel twice).
pub fn read_stereo(bytes: &[u8]) -> Result<(Vec<f32>, Vec<f32>, u32), String> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" {
        return Err("not a WAV file".into());
    }
    let (mut fmt, mut data) = (None, None);
    let mut i = 12;
    while i + 8 <= bytes.len() {
        let id = &bytes[i..i + 4];
        let len = u32::from_le_bytes(bytes[i + 4..i + 8].try_into().unwrap()) as usize;
        let body = &bytes[i + 8..(i + 8 + len).min(bytes.len())];
        if id == b"fmt " && body.len() >= 16 {
            fmt = Some((u16::from_le_bytes([body[0], body[1]]), u16::from_le_bytes([body[2], body[3]]), u32::from_le_bytes(body[4..8].try_into().unwrap()), u16::from_le_bytes([body[14], body[15]])));
        } else if id == b"data" {
            data = Some(body);
        }
        i += 8 + len + (len & 1);
    }
    let (tag, ch, rate, bits) = fmt.ok_or("no fmt")?;
    let data = data.ok_or("no data")?;
    if !(tag == 1 && bits == 16 || tag == 3 && bits == 32) || ch == 0 {
        return Err(format!("unsupported WAV {tag}/{bits}"));
    }
    let bps = bits as usize / 8;
    let (mut l, mut r) = (Vec::new(), Vec::new());
    for fr in data.chunks_exact(bps * ch as usize) {
        let s = |j: usize| -> f32 {
            let b = &fr[j * bps..];
            if bits == 16 {
                i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0
            } else {
                f32::from_le_bytes([b[0], b[1], b[2], b[3]])
            }
        };
        l.push(s(0));
        r.push(s(if ch > 1 { 1 } else { 0 }));
    }
    Ok((l, r, rate))
}

pub fn wav16(pcm: &[f32], rate: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(44 + pcm.len() * 2);
    let dl = (pcm.len() * 2) as u32;
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + dl).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * 2).to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&dl.to_le_bytes());
    for &s in pcm {
        b.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtraction_finds_a_triad_and_stops() {
        let notes = vec![
            Note { t: 0.1, dur: 1.0, pitch: 60, vel: 90 },
            Note { t: 0.1, dur: 1.0, pitch: 64, vel: 90 },
            Note { t: 0.1, dur: 1.0, pitch: 67, vel: 90 },
        ];
        let x = crate::piano::synth_piano(&notes, 1.5, 3);
        let fr = frames(&x, &x);
        let got = super::notes(&fr);
        let ps: std::collections::BTreeSet<u8> = got.iter().map(|n| n.pitch).collect();
        for p in [60, 64, 67] {
            assert!(ps.contains(&p), "{p} missing: {got:?}");
        }
        assert!(ps.len() <= 5, "ghosts: {got:?}");
    }
}

/// Live: streaming subtraction tracker (mono), the default live note core. Same frames as the
/// offline path; a causal per-key segmenter turns the activations into On/Off events.
/// Latency: N/2 (93 ms) for the window plus the confirmation (9 frames, 14 for weak notes): about
/// 200 ms, as the old tracker; the confirmation is what keeps attack transients from sounding.
pub struct SubTracker {
    an: Analyzer,
    /// Mono input from absolute sample `start`.
    buf: Vec<f32>,
    start: u64,
    n_in: u64,
    /// Next frame; its centre is frame * HOP.
    frame: u64,
    seg: Vec<KeySeg>,
    /// Smoothing ring: the last 3 frames' activations.
    ring: [[f32; KEYS]; 3],
    /// dB history per key of the smoothed activation, the last 4 frames.
    hist: Vec<[f32; 4]>,
    /// Raw activation per key for the last 16 frames, indexed frame % 16 (onset refinement).
    rawh: Vec<[f32; 16]>,
    gref: f32,
    /// The last frame's raw per-key scores and found amplitudes, 0..1 (for the UI rings).
    pub raw_view: [f32; KEYS],
    pub found_view: [f32; KEYS],
    raw_peak: f32,
    pub busy: std::time::Duration,
    ctx: [f32; 12],
    long: [f32; 12],
    scale: [bool; 12],
    flux_mean: f32,
    flux_prev: f32,
    onsets: std::collections::VecDeque<u64>,
    gate: bool,
    onset_lock: bool,
    oct: bool,
    holdkey: bool,
    debug_key: Option<usize>,
    decode: bool,
    pump: bool,
    dedouble: bool,
    poly: bool,
    /// Per key: the start frame of its last note.
    last_on: Vec<u64>,
    /// Per key: the frame its last note ended.
    last_off: Vec<u64>,
    /// Per key: the sounding note's level (dB, dropouts ignored) and whether re-strikes use it.
    lvl: Vec<f32>,
    lvl_rule: bool,
    /// Per key: short-window dB for the last 4 frames (attack evidence), indexed frame % 4.
    sh: Vec<[f32; 4]>,
    attack: bool,
    attack_rebirth: bool,
    /// The key's own attack (short window) also counts as its onset for the onset lock.
    attack_lock: bool,
    shh: Vec<[f32; 16]>,
    short_onset: bool,
    /// Notes refused by the gates (for the harness).
    pub blocked: usize,
}

#[derive(Clone, Copy)]
enum KeySeg {
    Idle,
    /// Started at frame s, peak dB pk, seen since frame i0.
    Pending { s: u64, pk: f32, i0: u64, rise: f32 },
    On { s: u64, pk: f32, below: u8 },
    /// Refused by a gate: waits until the key goes quiet before it may start again.
    Blocked,
}

impl Default for SubTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl SubTracker {
    pub fn new() -> SubTracker {
        SubTracker {
            an: Analyzer::new(),
            buf: Vec::with_capacity(2 * N),
            start: 0,
            n_in: 0,
            frame: 0,
            seg: vec![KeySeg::Idle; KEYS],
            ring: [[0.0; KEYS]; 3],
            hist: vec![[-120.0; 4]; KEYS],
            rawh: vec![[0.0; 16]; KEYS],
            gref: -120.0,
            raw_view: [0.0; KEYS],
            found_view: [0.0; KEYS],
            raw_peak: 1e-6,
            busy: std::time::Duration::ZERO,
            ctx: [0.0; 12],
            long: [0.0; 12],
            scale: [true; 12],
            flux_mean: 0.0,
            flux_prev: 0.0,
            onsets: std::collections::VecDeque::with_capacity(33),
            gate: env_on("MIXER_PIANO_GATE", true),
            onset_lock: env_on("MIXER_PIANO_ONSET", true),
            oct: env_on("MIXER_PIANO_OCT", false),
            holdkey: env_on("MIXER_PIANO_HOLDKEY", false),
            decode: env_on("MIXER_PIANO_DECODE", true),
            debug_key: std::env::var("SUB_DEBUG_KEY").ok().and_then(|v| v.parse::<usize>().ok()).map(|m| m.saturating_sub(LOW_KEY as usize)),
            pump: env_on("MIXER_PIANO_PUMP", false),
            dedouble: env_on("MIXER_PIANO_DEDOUBLE", true),
            poly: env_on("MIXER_PIANO_POLY", false),
            last_on: vec![0; KEYS],
            last_off: vec![0; KEYS],
            lvl: vec![-120.0; KEYS],
            sh: vec![[-120.0; 4]; KEYS],
            attack_rebirth: env_on("MIXER_PIANO_ATTACK_REBIRTH", false),
            attack_lock: env_on("MIXER_PIANO_ATTACK_LOCK", true),
            shh: vec![[-120.0; 16]; KEYS],
            short_onset: env_on("MIXER_PIANO_SHORT_ONSET", true),
            attack: env_on("MIXER_PIANO_ATTACK", true),
            lvl_rule: env_on("MIXER_PIANO_LEVELRULE", true),
            blocked: 0,
        }
    }

    pub fn now(&self) -> f64 {
        self.n_in as f64 / RATE as f64
    }

    fn t(f: u64) -> f64 {
        (f * HOP as u64) as f64 / RATE as f64
    }

    pub fn push(&mut self, pcm: &[f32]) -> Vec<crate::piano::Ev> {
        let t0 = std::time::Instant::now();
        self.buf.extend_from_slice(pcm);
        self.n_in += pcm.len() as u64;
        let mut evs = Vec::new();
        while self.frame * HOP as u64 + (N / 2) as u64 <= self.n_in {
            let c0 = (self.frame * HOP as u64) as i64 - (N / 2) as i64;
            let (buf, start) = (&self.buf, self.start as i64);
            let fo = self.an.analyse(
                |i| {
                    let s = c0 + i as i64 - start;
                    let v = if s >= 0 && (s as usize) < buf.len() { buf[s as usize] } else { 0.0 };
                    (v, v)
                },
                false,
            );
            self.step(&fo, &mut evs);
            self.frame += 1;
            // keep the long window's reach before the next frame's centre
            let keep_from = (self.frame * HOP as u64).saturating_sub((LONG - N / 2) as u64);
            if keep_from > self.start + N as u64 {
                let d = (keep_from - self.start) as usize;
                self.buf.drain(..d.min(self.buf.len()));
                self.start += d as u64;
            }
        }
        self.busy += t0.elapsed();
        evs
    }

    /// The birth gates for a note on key `k` starting at frame `s` with peak `pk` dB (re the running
    /// loudest): harmonic context (MIXER_PIANO_GATE), onset lock (MIXER_PIANO_ONSET), octave/fifth
    /// over a stronger sounding note (MIXER_PIANO_OCT).
    fn allow(&self, k: usize, s: u64, pk: f32, rise_db: f32) -> bool {
        let own_rise = rise_db >= tun("RISE_DB", 15.0);
        let pc = (LOW_KEY as usize + k) % 12;
        let pk_of = |j: usize| match self.seg[j] {
            KeySeg::On { pk, .. } => Some(pk),
            _ => None,
        };
        if self.gate {
            let mut idx: Vec<usize> = (0..12).collect();
            idx.sort_by(|&a, &b| self.ctx[b].total_cmp(&self.ctx[a]));
            let top = idx[..tun("GATE_TOP", 4.0) as usize].contains(&pc);
            let strong_in_key = self.scale[pc] && pk > tun("GATE_KEY_DB", -6.0);
            let melody = LOW_KEY as usize + k >= 60 && pk > -14.0 && (k + 1..KEYS).all(|j| pk_of(j).is_none());
            let overwhelming = pk > tun("GATE_OFF_DB", -4.0);
            if !(top || strong_in_key || melody || overwhelming) {
                return false;
            }
        }
        if self.onset_lock && !own_rise {
            let w = tun("ONSET_W", 3.0) as u64;
            if !self.onsets.iter().any(|&o| o + w >= s && o <= s + w) {
                return false;
            }
        }
        if self.dedouble {
            // an octave / 12th / two octaves of a note that started within 40 ms and is stronger
            for d in [12i32, 19, 24, -12, -19, -24] {
                let j = k as i32 + d;
                if j < 0 || j >= KEYS as i32 {
                    continue;
                }
                if let KeySeg::On { s: sj, pk: pj, .. } = self.seg[j as usize] {
                    if sj + 4 >= s && s + 4 >= sj && pj >= pk {
                        return false;
                    }
                }
            }
        }
        if self.poly {
            let sounding = self.seg.iter().filter(|x| matches!(x, KeySeg::On { .. })).count();
            let at_onset = self.onsets.iter().any(|&o| o + 3 >= s && o <= s + 3);
            let cap = if at_onset { tun("POLY_ONSET", 6.0) } else { tun("POLY_HOLD", 4.0) } as usize;
            let top = (k + 1..KEYS).all(|j| !matches!(self.seg[j], KeySeg::On { .. }));
            if sounding >= cap && !top {
                return false;
            }
        }
        if self.oct {
            for d in [12usize, 19, 24] {
                if k >= d {
                    if let Some(p) = pk_of(k - d) {
                        if pk < p - tun("OCT_DB", 6.0) && !own_rise {
                            return false;
                        }
                    }
                }
            }
        }
        true
    }

    fn step(&mut self, fo: &FrameOut, evs: &mut Vec<crate::piano::Ev>) {
        use crate::piano::Ev;
        let on_db = tun("ON_DB", -26.0);
        let off_db = tun("OFF_DB", -36.0);
        let weak_db = tun("WEAK_DB", -14.0);
        let db = |v: f32| 20.0 * v.max(1e-9).log10();
        let fs = HOP as f32 / RATE as f32;
        self.ring[(self.frame % 3) as usize] = fo.act;
        let fmax = fo.act.iter().fold(0f32, |m, &v| m.max(v));
        // running reference: the loudest frame, falling 1.5 dB/s
        self.gref = (self.gref - 1.5 * fs).max(db(fmax));
        let rmax = fo.raw.iter().fold(0f32, |m, &v| m.max(v));
        self.raw_peak = (self.raw_peak * 10f32.powf(-0.15 * fs)).max(rmax).max(1e-6);
        let i = self.frame;
        // harmonic context: chroma over ~1 beat, the key from the long-term chroma
        let a = (fs / 0.75).min(1.0);
        let ak = (fs / 30.0).min(1.0);
        for c in 0..12 {
            self.ctx[c] += a * (fo.chroma[c] - self.ctx[c]);
            self.long[c] += ak * (fo.chroma[c] - self.long[c]);
        }
        if i % 43 == 0 {
            self.scale = crate::arrange::scale_of(crate::arrange::detect_key(&self.long));
        }
        // global onsets: positive flux of the raw per-key scores against its running mean
        let mut flux = 0f32;
        for k in 0..KEYS {
            let r = (fo.raw[k] / self.raw_peak).sqrt().min(1.0);
            flux += (r - self.raw_view[k]).max(0.0);
        }
        self.flux_mean += (fs / 1.0).min(1.0) * (flux - self.flux_mean);
        if flux > tun("ONSET_K", 1.5) * self.flux_mean && flux > self.flux_prev {
            self.onsets.push_back(i);
            while self.onsets.len() > 32 {
                self.onsets.pop_front();
            }
        }
        self.flux_prev = flux;
        let gshift = if self.pump {
            let mut d: Vec<f32> = (0..KEYS)
                .filter(|&k| matches!(self.seg[k], KeySeg::On { .. }))
                .map(|k| db((self.ring[0][k] + self.ring[1][k] + self.ring[2][k]) / 3.0) - self.gref - self.hist[k][0])
                .collect();
            if d.len() >= 2 {
                d.sort_by(|a, b| a.total_cmp(b));
                d[d.len() / 2].max(0.0)
            } else {
                0.0
            }
        } else {
            0.0
        };
        for k in 0..KEYS {
            self.rawh[k][(i % 16) as usize] = fo.act[k];
            self.raw_view[k] = (fo.raw[k] / self.raw_peak).sqrt().min(1.0);
            self.found_view[k] = if fo.act[k] > 0.0 { ((db(fo.act[k]) - self.gref + 40.0) / 40.0).clamp(0.0, 1.0) } else { 0.0 };
            let v = db((self.ring[0][k] + self.ring[1][k] + self.ring[2][k]) / 3.0) - self.gref;
            let h = &mut self.hist[k];
            let v3 = h[0];
            h.rotate_left(1);
            h[3] = v;
            if self.debug_key == Some(k) {
                eprintln!(
                    "f{i} t{:.2} act {:.1} v {v:.1} st {}",
                    Self::t(i),
                    db(fo.act[k]) - self.gref,
                    match self.seg[k] {
                        KeySeg::Idle => "idle",
                        KeySeg::Pending { .. } => "pend",
                        KeySeg::On { .. } => "ON",
                        KeySeg::Blocked => "blk",
                    }
                );
            }
            // pumping (sidechain, compressor): a rise every sounding key shares is not an attack
            let kr = v - v3 - gshift;
            // the key's own attack: short-window energy over its minimum of the previous 3 frames
            let att = {
                let r = &mut self.sh[k];
                let mn = r.iter().enumerate().filter(|(j, _)| *j as u64 != i % 4).map(|(_, &x)| x).fold(f32::INFINITY, f32::min);
                r[(i % 4) as usize] = fo.short[k];
                self.shh[k][(i % 16) as usize] = fo.short[k];
                fo.short[k] - mn
            };
            let rise = kr > 5.0;
            let pitch = (LOW_KEY as usize + k) as u8;
            // the smoothed value is centred one frame back
            let at = i.saturating_sub(1);
            self.seg[k] = match self.seg[k] {
                KeySeg::Idle => {
                    // hold: coming back within 300 ms of its own off is the same note decaying,
                    // not a new strike, unless it is a clear attack
                    let since = i.saturating_sub(self.last_off[k]);
                    // a rebound must rise above the level the note had (decayed), not above the dropout
                    let kr_re = if self.lvl_rule { (v - (self.lvl[k] - tun("LVL_DECAY", 25.0) * fs * since as f32) - gshift).min(kr) } else { kr };
                    let fresh = !(self.holdkey || self.decode) || since > 26 || kr_re >= tun("REBIRTH_DB", 9.0) || (self.attack_rebirth && att >= tun("ATTACK_DB", 11.0));
                    if fresh && v > on_db && (rise || v3 < on_db) {
                        KeySeg::Pending { s: at.saturating_sub(1), pk: v, i0: i, rise: if self.attack_lock { kr.max(att) } else { kr } }
                    } else {
                        KeySeg::Idle
                    }
                }
                KeySeg::Blocked => {
                    if v < off_db || v < -40.0 {
                        KeySeg::Idle
                    } else if rise && v > on_db {
                        // a fresh attack on a blocked key gets a new chance
                        KeySeg::Pending { s: at.saturating_sub(1), pk: v, i0: i, rise: if self.attack_lock { kr.max(att) } else { kr } }
                    } else {
                        KeySeg::Blocked
                    }
                }
                KeySeg::Pending { s, pk, i0, rise: own_rise } => {
                    let pk = pk.max(v);
                    if v < off_db || v < pk - 18.0 {
                        KeySeg::Idle
                    } else if i - i0 >= if pk >= weak_db { tun("CONFIRM", 9.0) as u64 } else { tun("CONFIRM_WEAK", 14.0) as u64 } {
                        let vel = (100.0 + 2.0 * pk).clamp(25.0, 120.0) as u8;
                        // the window sees an attack N/2 early: start at the steepest rise since s
                        let rh = &self.rawh[k];
                        let lo = s.max(i.saturating_sub(15));
                        let s = (lo..i).max_by(|&x, &y| {
                            let d = |j: u64| rh[((j + 1) % 16) as usize] - rh[(j % 16) as usize];
                            d(x).total_cmp(&d(y))
                        }).unwrap_or(s);
                        if self.allow(k, s, pk, own_rise) {
                            self.last_on[k] = s;
                            self.lvl[k] = v;
                            // onset time: the steepest rise of the key's short-window energy near s
                            // (adapts to soft vs sharp attacks); else a fixed shift
                            let t_on = if self.attack && self.short_onset {
                                let sh = &self.shh[k];
                                let lo = s.saturating_sub(4).max(i.saturating_sub(15));
                                let hi = (s + 6).min(i);
                                let j = (lo..hi.max(lo + 1))
                                    .max_by(|&x, &y| {
                                        let d = |j: u64| sh[((j + 1) % 16) as usize] - sh[(j % 16) as usize];
                                        d(x).total_cmp(&d(y))
                                    })
                                    .unwrap_or(s);
                                Self::t(j + 1) - tun("SHORT_LEAD", 0.012) as f64
                            } else {
                                Self::t(s) - tun("ONSET_SHIFT", 0.02) as f64
                            };
                            evs.push(Ev::On { t: t_on.max(0.0), pitch, vel });
                            KeySeg::On { s, pk, below: 0 }
                        } else {
                            self.blocked += 1;
                            KeySeg::Blocked
                        }
                    } else {
                        KeySeg::Pending { s, pk, i0, rise: own_rise }
                    }
                }
                KeySeg::On { s, pk, below } => {
                    let pk = pk.max(v);
                    // the note's own level, ignoring dropouts, falling 6 dB/s: a re-strike must rise
                    // above it (a rebound after the pursuit missed the key for a few frames is not)
                    let lv = self.lvl[k] - tun("LVL_DECAY", 25.0) * fs;
                    let kr = if self.lvl_rule { (v - lv - gshift).min(kr) } else { kr };
                    self.lvl[k] = if v > pk - 18.0 { lv.max(v) } else { lv };
                    let (rs_ok, drop, nbelow) = if self.decode {
                        // interval decoding (fixed lag): the note holds while the key comes back above
                        // its floor within DECODE_LAG frames; it ends where it last was above
                        ((kr >= tun("RESTRIKE_DB", if self.lvl_rule { 4.0 } else { 6.0 }) || (self.attack && att >= tun("ATTACK_DB", 11.0))) && at > s + 21, tun("OFF_DROP", 24.0), tun("DECODE_LAG", if legato() { 40.0 } else { 26.0 }) as u8)
                    } else if self.holdkey {
                        // hold: a re-strike needs a clear new attack on this key, 250 ms after the last
                        (kr >= tun("RESTRIKE_DB", 6.0) && at > s + 21, tun("OFF_DROP", 24.0), tun("HOLD_BELOW", 8.0) as u8)
                    } else {
                        (rise && at > s + 8, 18.0, 3)
                    };
                    if rs_ok && v > on_db {
                        evs.push(Ev::Off { t: Self::t(at.saturating_sub(1)), pitch });
                        KeySeg::Pending { s: at.saturating_sub(1), pk: v, i0: i, rise: if self.attack_lock { kr.max(att) } else { kr } }
                    } else {
                        let below = if v < off_db || v < pk - drop { below + 1 } else { 0 };
                        if below >= nbelow {
                            self.last_off[k] = i;
                            let back = if self.decode { below as u64 } else { 2 };
                            evs.push(Ev::Off { t: Self::t(at.saturating_sub(back).max(s + 1)), pitch });
                            KeySeg::Idle
                        } else {
                            KeySeg::On { s, pk, below }
                        }
                    }
                }
            };
        }
    }

    /// End of input: pads with silence so the last frames are analysed, then everything off.
    pub fn flush(&mut self) -> Vec<crate::piano::Ev> {
        let mut evs = self.push(&vec![0.0; N]);
        evs.extend(self.all_off(self.now()));
        evs
    }

    pub fn all_off(&mut self, t: f64) -> Vec<crate::piano::Ev> {
        let mut evs = Vec::new();
        for k in 0..KEYS {
            if let KeySeg::On { .. } = self.seg[k] {
                evs.push(crate::piano::Ev::Off { t, pitch: (LOW_KEY as usize + k) as u8 });
            }
            self.seg[k] = KeySeg::Idle;
        }
        evs
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    #[test]
    fn streaming_finds_the_triad_in_10ms_chunks() {
        let notes = vec![
            Note { t: 0.2, dur: 1.0, pitch: 60, vel: 90 },
            Note { t: 0.2, dur: 1.0, pitch: 64, vel: 90 },
            Note { t: 0.2, dur: 1.0, pitch: 67, vel: 90 },
        ];
        let x = crate::piano::synth_piano(&notes, 1.6, 3);
        let mut st = SubTracker::new();
        let mut evs = Vec::new();
        for c in x.chunks(220) {
            evs.extend(st.push(c));
        }
        evs.extend(st.flush());
        let got = crate::piano::events_to_notes(&evs);
        for p in [60, 64, 67] {
            let n = got.iter().find(|n| n.pitch == p && n.dur > 0.5);
            assert!(n.is_some_and(|n| (n.t - 0.2).abs() < 0.06), "{p}: {got:?}");
        }
    }
}
