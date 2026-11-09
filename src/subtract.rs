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
const KMAX: usize = 1000; // ~5.4 kHz
const MAX_NOTES: usize = 14;

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
pub fn frames(l: &[f32], r: &[f32]) -> Frames {
    let fft = Fft::new(N);
    let win = hann(N);
    let df = RATE as f32 / N as f32;
    let len = l.len().min(r.len());
    let nfr = len / HOP + 1;
    let stop_rel = tun("STOP_REL", 0.22);
    let mut out = Frames {
        frame_s: HOP as f64 / RATE as f64,
        t0: 0.0,
        act: Vec::with_capacity(nfr),
        found: Vec::with_capacity(nfr),
        centre: Vec::with_capacity(nfr),
        gain: Vec::with_capacity(nfr),
    };
    let (mut lr, mut li, mut rr, mut ri) = (vec![0f32; N], vec![0f32; N], vec![0f32; N], vec![0f32; N]);
    let mut mag = vec![0f32; KMAX];
    let mut g = vec![0f32; KMAX];
    for f in 0..nfr {
        let c0 = (f * HOP) as isize - (N / 2) as isize;
        for i in 0..N {
            let s = c0 + i as isize;
            let (a, b) = if s >= 0 && (s as usize) < len { (l[s as usize], r[s as usize]) } else { (0.0, 0.0) };
            // zero-phase: rotate by N/2 so phases refer to the frame centre and W is real
            let j = (i + N / 2) % N;
            lr[j] = a * win[i];
            rr[j] = b * win[i];
            li[j] = 0.0;
            ri[j] = 0.0;
        }
        fft.run(&mut lr, &mut li, false);
        fft.run(&mut rr, &mut ri, false);
        let mut cen = vec![0f32; KMAX];
        let (mut xr, mut xi) = (vec![0f32; KMAX], vec![0f32; KMAX]);
        for k in 0..KMAX {
            let (ar, ai, br, bi) = (lr[k], li[k], rr[k], ri[k]);
            let cross = ar * br + ai * bi; // Re(L conj R)
            let pw = ar * ar + ai * ai + br * br + bi * bi;
            cen[k] = (2.0 * cross / pw.max(1e-20)).max(0.0);
            xr[k] = 0.5 * (ar + br);
            xi[k] = 0.5 * (ai + bi);
            mag[k] = (xr[k] * xr[k] + xi[k] * xi[k]).sqrt();
        }
        whiten(&mag, &mut g);
        for k in 0..KMAX {
            xr[k] *= g[k];
            xi[k] *= g[k];
            mag[k] = (xr[k] * xr[k] + xi[k] * xi[k]).sqrt();
        }
        let mut act = [0f32; KEYS];
        let mut found: Vec<Found> = Vec::new();
        let mut first = 0f32;
        let mut taken = [false; KEYS];
        for _ in 0..MAX_NOTES {
            let mut best = (0f32, usize::MAX);
            for k in 0..KEYS {
                if taken[k] || key_hz(k) > 2100.0 {
                    continue;
                }
                let s = score(&mag, k, df);
                if s > best.0 {
                    best = (s, k);
                }
            }
            if best.1 == usize::MAX || best.0 <= 0.0 {
                break;
            }
            // against noise: the same weights on the residual's mean level (a flat spectrum scores this x ~2)
            let lvl = mag[10..KMAX].iter().sum::<f32>() / (KMAX - 10) as f32;
            let f0b = key_hz(best.1);
            let wsum: f32 = (1..=n_harm(f0b, df)).map(|h| (f0b + 27.0) / (h as f32 * f0b + 320.0)).sum();
            if best.0 < tun("SNR", 4.0) * lvl * wsum {
                break;
            }
            if first == 0.0 {
                first = best.0;
            } else if best.0 < stop_rel * first {
                break;
            }
            let k = best.1;
            taken[k] = true;
            // refine f0 from the partial peaks
            let f0n = key_hz(k);
            let nh = n_harm(f0n, df);
            let mut pk: Vec<Option<(f32, f32)>> = Vec::with_capacity(nh);
            let (mut num, mut den) = (0f32, 0f32);
            for h in 1..=nh {
                let c = h as f32 * f0n * (1.0 + 0.0004 * (h * h) as f32).sqrt() / df;
                let p = peak(&mag, c, (0.03 * c).max(1.0));
                if let Some((pos, a)) = p {
                    if h <= 6 {
                        num += a * pos * df / h as f32;
                        den += a;
                    }
                }
                pk.push(p);
            }
            let f0 = if den > 0.0 { num / den } else { f0n };
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
                let cap = 1.5 * nb.iter().sum::<f32>() / nb.len() as f32;
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
                    mag[kk as usize] = (xr[kk as usize].powi(2) + xi[kk as usize].powi(2)).sqrt();
                }
            }
            let amp = parts.iter().map(|p| p.re * p.re + p.im * p.im).sum::<f32>().sqrt();
            act[k] = amp;
            found.push(Found { key: (LOW_KEY as usize + k) as u8, f0, amp, partials: parts });
        }
        out.act.push(act);
        out.found.push(found);
        out.centre.push(cen);
        out.gain.push(g.clone());
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
    for (f, sel) in line.iter().enumerate() {
        let p = sel.map(|i| {
            let x = &fr.found[f][i];
            ((69.0 + 12.0 * (x.f0 / 440.0).log2()).round() as u8, x.amp / amax)
        });
        match (cur, p) {
            (Some((s, cp, a)), Some((np, na))) if np == cp => cur = Some((s, cp, a.max(na))),
            (Some((s, cp, a)), _) => {
                push(&mut out, s, f, cp, a);
                cur = p.map(|(np, na)| (f, np, na));
            }
            (None, Some((np, na))) => cur = Some((f, np, na)),
            (None, None) => {}
        }
    }
    if let Some((s, cp, a)) = cur {
        push(&mut out, s, line.len(), cp, a);
    }
    // bridge gaps < 60 ms between equal pitches
    let mut merged: Vec<Note> = Vec::new();
    for n in out {
        match merged.last_mut() {
            Some(l) if l.pitch == n.pitch && n.t - (l.t + l.dur) < 0.06 => l.dur = n.t + n.dur - l.t,
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
