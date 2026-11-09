//! Song -> arrangement (#23): ANALYZE -> MUSICAL MODEL -> ARRANGE, our own DSP, CPU only.
//!
//! The PianoTracker is the low-level source: its per-key harmonic salience on the whitened FFT
//! spectrum (88 keys, every 11.6 ms frame) and its note onsets. On top of that:
//! - onset strength (positive salience flux), tempo (autocorrelation with a tempo prior) and a beat
//!   grid (Ellis dynamic-programming beat tracker);
//! - chroma and bass chroma per beat, the key (Krumhansl-Kessler profiles), and a chord per beat
//!   (templates maj/min/7/maj7/m7/sus2/sus4/dim/aug, bass and key priors, Viterbi smoothing);
//! - the melody: one monophonic top line (salience in G3..C6 with a continuity cost, per 1/8 beat,
//!   snapped to the key unless clearly chromatic);
//! - parts generated from that model, quantized to the grid: Piano (88 keys: bass octaves, voiced
//!   chords with voice-leading, the melody on top), and in the ensemble also Strings, Lead, Bass,
//!   Harp and Drums, each on its own channel and GM program.
use crate::notes::{Note, TrackSpec};
use crate::piano::{self, PianoTracker, DELAY, HOP, KEYS, LOW_KEY, RATE};

pub const NAMES: [&str; 12] = ["C", "C#", "D", "Eb", "E", "F", "F#", "G", "Ab", "A", "Bb", "B"];

/// Chord types: name suffix, intervals, template weights (root heavier).
const TYPES: [(&str, &[u8]); 9] = [
    ("", &[0, 4, 7]),
    ("m", &[0, 3, 7]),
    ("7", &[0, 4, 7, 10]),
    ("maj7", &[0, 4, 7, 11]),
    ("m7", &[0, 3, 7, 10]),
    ("sus2", &[0, 2, 7]),
    ("sus4", &[0, 5, 7]),
    ("dim", &[0, 3, 6]),
    ("aug", &[0, 4, 8]),
];
/// Simpler chords win ties: a small penalty per type.
const TYPE_COST: [f32; 9] = [0.0, 0.0, 0.06, 0.09, 0.06, 0.08, 0.08, 0.10, 0.12];

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Chord {
    pub root: u8,
    /// Index into TYPES.
    pub kind: u8,
}

impl Chord {
    pub fn name(&self) -> String {
        format!("{}{}", NAMES[self.root as usize], TYPES[self.kind as usize].0)
    }
    pub fn pcs(&self) -> Vec<u8> {
        TYPES[self.kind as usize].1.iter().map(|i| (self.root + i) % 12).collect()
    }
}

pub struct Analysis {
    pub frame_s: f64,
    /// Seconds of the first frame's centre.
    pub t0: f64,
    pub sal: Vec<[f32; KEYS]>,
    pub onset: Vec<f32>,
    pub bpm: f64,
    /// Beat times, seconds.
    pub beats: Vec<f64>,
    /// Key: tonic pitch class and major (true) / minor.
    pub key: (u8, bool),
    /// Chord for each beat interval beats[i]..beats[i+1].
    pub chords: Vec<Chord>,
    /// Melody notes (monophonic).
    pub melody: Vec<Note>,
    /// Tracker notes (raw).
    pub raw: Vec<Note>,
}

impl Analysis {
    pub fn key_name(&self) -> String {
        format!("{} {}", NAMES[self.key.0 as usize], if self.key.1 { "major" } else { "minor" })
    }
}

fn frames(mono22: &[f32]) -> (Vec<[f32; KEYS]>, Vec<Note>) {
    let mut tr = PianoTracker::new();
    let mut out = Vec::with_capacity(mono22.len() / HOP + 1);
    let mut evs = Vec::new();
    for ch in mono22.chunks(HOP) {
        evs.extend(tr.push(ch));
        let mut a = [0f32; KEYS];
        a.copy_from_slice(&tr.key_salience()[..KEYS]);
        out.push(a);
    }
    evs.extend(tr.flush());
    (out, piano::events_to_notes(&evs))
}

fn onset_strength(sal: &[[f32; KEYS]]) -> Vec<f32> {
    let mut o = vec![0f32; sal.len()];
    let lg = |v: f32| (1.0 + 20.0 * v.max(0.0)).ln();
    // scale salience to its 99th percentile so the log compression is level independent
    let mut all: Vec<f32> = sal.iter().map(|f| f.iter().fold(0f32, |a, &v| a.max(v))).collect();
    all.sort_by(|a, b| a.total_cmp(b));
    let ref_ = all.get(all.len() * 99 / 100).copied().unwrap_or(1.0).max(1e-9);
    for i in 1..sal.len() {
        o[i] = (0..KEYS).map(|k| (lg(sal[i][k] / ref_) - lg(sal[i - 1][k] / ref_)).max(0.0)).sum();
    }
    // subtract a local mean (0.5 s) and half-wave rectify
    let w = 22usize;
    let mut out = vec![0f32; o.len()];
    let mut acc = 0f32;
    for i in 0..o.len() {
        acc += o[i];
        if i >= 2 * w {
            acc -= o[i - 2 * w];
        }
        let c = i.saturating_sub(w);
        let n = (i + 1).min(2 * w) as f32;
        out[c] = (o[c] - acc / n).max(0.0);
    }
    out
}

/// Tempo from the onset autocorrelation with a log-normal prior around 100 BPM; returns the beat
/// period in frames.
fn tempo(o: &[f32], frame_s: f64) -> f64 {
    let ac = |lag: usize| -> f64 {
        if lag == 0 || lag >= o.len() {
            return 0.0;
        }
        (lag..o.len()).map(|i| (o[i] * o[i - lag]) as f64).sum::<f64>() / (o.len() - lag) as f64
    };
    let (lo, hi) = ((60.0 / 160.0 / frame_s) as usize, (60.0 / 50.0 / frame_s) as usize);
    let mut best = (0.0f64, lo);
    let dbg = std::env::var_os("ARR_DEBUG").is_some();
    for lag in lo..=hi {
        // the beat and its bar-level multiples and 8th-note half all line up (comb)
        let s = ac(lag) + 0.5 * ac(2 * lag) + 0.33 * ac(4 * lag) + 0.5 * ac(lag / 2);
        let bpm = 60.0 / (lag as f64 * frame_s);
        let w = (-0.5 * ((bpm / 90.0).log2() / 1.0).powi(2)).exp();
        if dbg && lag % 4 == 0 {
            eprintln!("lag {lag} bpm {bpm:.1} comb {s:.4} w {w:.2}");
        }
        if s * w > best.0 {
            best = (s * w, lag);
        }
    }
    best.1 as f64
}

/// Ellis (2007) dynamic-programming beat tracker.
fn track_beats(o: &[f32], period: f64) -> Vec<usize> {
    let n = o.len();
    if n == 0 {
        return vec![];
    }
    let mut score = vec![0f64; n];
    let mut back = vec![usize::MAX; n];
    let alpha = 400.0;
    for i in 0..n {
        let (a, b) = ((i as f64 - 2.0 * period).max(0.0) as usize, (i as f64 - period / 2.0).max(0.0) as usize);
        let mut best = (f64::NEG_INFINITY, usize::MAX);
        if b > a {
            for j in a..b {
                let x = ((i - j) as f64 / period).ln();
                let s = score[j] - alpha * x * x;
                if s > best.0 {
                    best = (s, j);
                }
            }
        }
        score[i] = o[i] as f64 + best.0.max(0.0);
        back[i] = if best.0 > 0.0 { best.1 } else { usize::MAX };
    }
    // start from the best score in the last period
    let tail = (n as f64 - period).max(0.0) as usize;
    let mut i = (tail..n).max_by(|&a, &b| score[a].total_cmp(&score[b])).unwrap_or(n - 1);
    let mut beats = vec![i];
    while back[i] != usize::MAX {
        i = back[i];
        beats.push(i);
    }
    beats.reverse();
    beats
}

const KK_MAJ: [f32; 12] = [6.35, 2.23, 3.48, 2.33, 4.38, 4.09, 2.52, 5.19, 2.39, 3.66, 2.29, 2.88];
const KK_MIN: [f32; 12] = [6.33, 2.68, 3.52, 5.38, 2.60, 3.53, 2.54, 4.75, 3.98, 2.69, 3.34, 3.17];

fn corr(a: &[f32; 12], b: &[f32; 12]) -> f32 {
    let (ma, mb) = (a.iter().sum::<f32>() / 12.0, b.iter().sum::<f32>() / 12.0);
    let (mut n, mut da, mut db) = (0f32, 0f32, 0f32);
    for i in 0..12 {
        n += (a[i] - ma) * (b[i] - mb);
        da += (a[i] - ma).powi(2);
        db += (b[i] - mb).powi(2);
    }
    n / (da * db).sqrt().max(1e-9)
}

fn detect_key(chroma: &[f32; 12]) -> (u8, bool) {
    let mut best = (f32::NEG_INFINITY, 0u8, true);
    for t in 0..12u8 {
        for (prof, maj) in [(&KK_MAJ, true), (&KK_MIN, false)] {
            let mut p = [0f32; 12];
            for i in 0..12 {
                p[(i + t as usize) % 12] = prof[i];
            }
            let c = corr(chroma, &p);
            if c > best.0 {
                best = (c, t, maj);
            }
        }
    }
    (best.1, best.2)
}

fn scale_of(key: (u8, bool)) -> [bool; 12] {
    let iv: &[u8] = if key.1 { &[0, 2, 4, 5, 7, 9, 11] } else { &[0, 2, 3, 5, 7, 8, 10] };
    let mut s = [false; 12];
    for i in iv {
        s[((key.0 + i) % 12) as usize] = true;
    }
    s
}

/// Chroma (all keys from C3 up, sqrt-compressed) and bass chroma (below C3) over frames a..b.
fn chroma(sal: &[[f32; KEYS]], a: usize, b: usize) -> ([f32; 12], [f32; 12]) {
    let (mut c, mut bc) = ([0f32; 12], [0f32; 12]);
    for f in &sal[a..b.max(a + 1).min(sal.len())] {
        for k in 0..KEYS {
            let m = LOW_KEY as usize + k;
            let v = f[k].max(0.0).sqrt();
            if m < 48 {
                bc[m % 12] += v;
            } else if m < 96 {
                c[m % 12] += v;
            }
        }
    }
    let nrm = |x: &mut [f32; 12]| {
        let s = x.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-9);
        x.iter_mut().for_each(|v| *v /= s);
    };
    nrm(&mut c);
    nrm(&mut bc);
    (c, bc)
}

fn chord_scores(c: &[f32; 12], bc: &[f32; 12], scale: &[bool; 12]) -> Vec<f32> {
    let mut out = Vec::with_capacity(12 * TYPES.len());
    for root in 0..12u8 {
        for (ti, (_, iv)) in TYPES.iter().enumerate() {
            let mut tpl = [0f32; 12];
            for (j, i) in iv.iter().enumerate() {
                tpl[((root + i) % 12) as usize] = if j == 0 { 1.2 } else { 1.0 };
            }
            let tn = tpl.iter().map(|v| v * v).sum::<f32>().sqrt();
            let cos: f32 = (0..12).map(|i| c[i] * tpl[i]).sum::<f32>() / tn;
            let bass = bc[root as usize] * 0.25;
            let diatonic = iv.iter().all(|i| scale[((root + i) % 12) as usize]);
            out.push(cos + bass + if diatonic { 0.05 } else { 0.0 } - TYPE_COST[ti]);
        }
    }
    out
}

fn viterbi(scores: &[Vec<f32>], switch: f32) -> Vec<usize> {
    let n = scores.len();
    if n == 0 {
        return vec![];
    }
    let s = scores[0].len();
    let mut acc = scores[0].clone();
    let mut back = vec![vec![0usize; s]; n];
    for t in 1..n {
        let (bi, bv) = acc.iter().enumerate().fold((0, f32::NEG_INFINITY), |m, (i, &v)| if v > m.1 { (i, v) } else { m });
        let mut next = vec![0f32; s];
        for j in 0..s {
            let stay = acc[j];
            let (from, v) = if stay >= bv - switch { (j, stay) } else { (bi, bv - switch) };
            back[t][j] = from;
            next[j] = v + scores[t][j];
        }
        acc = next;
    }
    let mut j = acc.iter().enumerate().fold((0, f32::NEG_INFINITY), |m, (i, &v)| if v > m.1 { (i, v) } else { m }).0;
    let mut path = vec![0usize; n];
    for t in (0..n).rev() {
        path[t] = j;
        j = back[t][j];
    }
    path
}

/// The melody: per 1/8 beat slot, the strongest salience in G3..C6 with a continuity cost (DP),
/// silence when nothing stands out, snapped to the key unless clearly chromatic; equal neighbours
/// merge unless the tracker has an onset on that key.
fn melody(a: &Analysis, raw: &[Note]) -> Vec<Note> {
    let (lo, hi) = (55usize, 84usize);
    let np = hi - lo + 1;
    let scale = scale_of(a.key);
    let mut slots: Vec<(f64, f64)> = Vec::new();
    for w in a.beats.windows(2) {
        let d = (w[1] - w[0]) / 2.0;
        slots.push((w[0], w[0] + d));
        slots.push((w[0] + d, w[1]));
    }
    let fr = |t: f64| (((t - a.t0) / a.frame_s).round().max(0.0) as usize).min(a.sal.len());
    // salience per slot per pitch, and a global reference
    let mut sv: Vec<Vec<f32>> = Vec::with_capacity(slots.len());
    for &(t0, t1) in &slots {
        let (f0, f1) = (fr(t0), fr(t1).max(fr(t0) + 1).min(a.sal.len()));
        let mut v = vec![0f32; np];
        for f in &a.sal[f0.min(a.sal.len())..f1] {
            for p in lo..=hi {
                v[p - lo] += f[p - LOW_KEY as usize].max(0.0);
            }
        }
        let n = (f1 - f0.min(f1)).max(1) as f32;
        v.iter_mut().for_each(|x| *x /= n);
        sv.push(v);
    }
    let mut peaks: Vec<f32> = sv.iter().map(|v| v.iter().fold(0f32, |m, &x| m.max(x))).collect();
    peaks.sort_by(|a, b| a.total_cmp(b));
    let r = peaks.get(peaks.len() * 9 / 10).copied().unwrap_or(1.0).max(1e-9);
    // DP over pitch states + silence (index np)
    let n = sv.len();
    if n == 0 {
        return vec![];
    }
    let obs = |t: usize, p: usize| -> f32 {
        if p == np {
            0.18
        } else {
            let x = sv[t][p] / r;
            // a slight upward bias: the lead sits on top
            (x.min(1.5)) + 0.004 * p as f32
        }
    };
    let jump = |a: usize, b: usize| -> f32 {
        if a == np || b == np {
            0.08
        } else {
            let d = (a as i32 - b as i32).unsigned_abs() as f32;
            0.025 * d.min(12.0)
        }
    };
    let mut acc: Vec<f32> = (0..=np).map(|p| obs(0, p)).collect();
    let mut back = vec![vec![0usize; np + 1]; n];
    for t in 1..n {
        let mut next = vec![0f32; np + 1];
        for j in 0..=np {
            let (mut bi, mut bv) = (0, f32::NEG_INFINITY);
            for i in 0..=np {
                let v = acc[i] - jump(i, j);
                if v > bv {
                    bv = v;
                    bi = i;
                }
            }
            back[t][j] = bi;
            next[j] = bv + obs(t, j);
        }
        acc = next;
    }
    let mut j = (0..=np).max_by(|&x, &y| acc[x].total_cmp(&acc[y])).unwrap_or(np);
    let mut path = vec![np; n];
    for t in (0..n).rev() {
        path[t] = j;
        j = back[t][j];
    }
    // snap, merge
    let mut out: Vec<Note> = Vec::new();
    for (t, &p) in path.iter().enumerate() {
        if p == np {
            continue;
        }
        let mut m = (lo + p) as u8;
        if !scale[(m % 12) as usize] {
            let (dn, up) = (m - 1, m + 1);
            let sal = |q: u8| if (lo as u8..=hi as u8).contains(&q) { sv[t][q as usize - lo] } else { 0.0 };
            let alt = if scale[(up % 12) as usize] && (!scale[(dn % 12) as usize] || sal(up) >= sal(dn)) { up } else { dn };
            if sal(m) < 2.0 * sal(alt) {
                m = alt;
            }
        }
        let (s0, s1) = slots[t];
        let vel = (60.0 + 50.0 * (sv[t][p] / r).min(1.0)) as u8;
        let restrike = raw.iter().any(|x| x.pitch == m && (x.t - s0).abs() < (s1 - s0) * 0.6);
        match out.last_mut() {
            Some(l) if l.pitch == m && (l.t + l.dur - s0).abs() < 1e-6 && !restrike => l.dur = s1 - l.t,
            _ => out.push(Note { t: s0, dur: s1 - s0, pitch: m, vel }),
        }
    }
    out
}

pub fn analyze(mono: &[f32], rate: u32) -> Analysis {
    let x = piano::to_rate(mono, rate);
    let (sal, raw) = frames(&x);
    let frame_s = HOP as f64 / RATE as f64;
    // salience frame i was computed once (i+1)*HOP samples were in; its centre lags by DELAY
    let t0 = (HOP as f64 - DELAY as f64) / RATE as f64;
    let onset = onset_strength(&sal);
    let period = tempo(&onset, frame_s);
    let bf = track_beats(&onset, period);
    let mut beats: Vec<f64> = bf.iter().map(|&i| t0 + i as f64 * frame_s).filter(|&t| t >= 0.0).collect();
    if beats.len() < 2 {
        beats = (0..((x.len() as f64 / RATE as f64) / 0.6) as usize).map(|i| i as f64 * 0.6).collect();
    }
    let fr = |t: f64| (((t - t0) / frame_s).round().max(0.0) as usize).min(sal.len());
    let (total, _) = chroma(&sal, 0, sal.len());
    let key = detect_key(&total);
    let scale = scale_of(key);
    let scores: Vec<Vec<f32>> = beats
        .windows(2)
        .map(|w| {
            let (c, bc) = chroma(&sal, fr(w[0]), fr(w[1]));
            chord_scores(&c, &bc, &scale)
        })
        .collect();
    let path = viterbi(&scores, tun_f("ARR_SWITCH", 0.05));
    let chords = path.iter().map(|&s| Chord { root: (s / TYPES.len()) as u8, kind: (s % TYPES.len()) as u8 }).collect();
    let bpm = 60.0 / (period * frame_s);
    let mut a = Analysis { frame_s, t0, sal, onset, bpm, beats, key, chords, melody: vec![], raw: vec![] };
    a.melody = melody(&a, &raw);
    a.raw = raw;
    a
}

/// One arranged part.
pub struct Part {
    pub name: &'static str,
    pub ch: u8,
    pub prog: u8,
    pub rgb: (u8, u8, u8),
    pub notes: Vec<Note>,
}

/// Chord spans: (start, end, chord), merging equal neighbours.
fn spans(a: &Analysis) -> Vec<(f64, f64, Chord)> {
    let mut out: Vec<(f64, f64, Chord)> = Vec::new();
    for (i, c) in a.chords.iter().enumerate() {
        let (t0, t1) = (a.beats[i], a.beats[i + 1]);
        match out.last_mut() {
            Some(l) if l.2 == *c => l.1 = t1,
            _ => out.push((t0, t1, *c)),
        }
    }
    out
}

/// Voicing of `pcs` in [lo, hi], closest to `prev` (sum of moves), at most 4 notes.
fn voice(pcs: &[u8], lo: u8, hi: u8, prev: &[u8]) -> Vec<u8> {
    let cands: Vec<Vec<u8>> = (0..pcs.len())
        .map(|inv| {
            let mut v = Vec::new();
            let mut p = lo;
            for j in 0..pcs.len().min(4) {
                let pc = pcs[(inv + j) % pcs.len()];
                while p % 12 != pc {
                    p += 1;
                }
                v.push(p);
                p += 1;
            }
            v
        })
        .flat_map(|v| [v.clone(), v.iter().map(|&x| x + 12).collect()])
        .filter(|v: &Vec<u8>| v.iter().all(|&x| x >= lo && x <= hi))
        .collect();
    let cost = |v: &Vec<u8>| -> i32 {
        if prev.is_empty() {
            (v.iter().map(|&x| x as i32).sum::<i32>() / v.len() as i32 - (lo as i32 + hi as i32) / 2).abs()
        } else {
            v.iter().map(|&x| prev.iter().map(|&q| (x as i32 - q as i32).abs()).min().unwrap_or(0)).sum()
        }
    };
    cands.into_iter().min_by_key(cost).unwrap_or_default()
}

fn beats_in(a: &Analysis, t0: f64, t1: f64) -> Vec<f64> {
    a.beats.iter().copied().filter(|&b| b >= t0 - 1e-6 && b < t1 - 1e-6).collect()
}

/// Loudness 0..1 around t (onset/salience energy), for velocities.
fn loud(a: &Analysis, t: f64) -> f32 {
    let i = (((t - a.t0) / a.frame_s).max(0.0) as usize).min(a.sal.len().saturating_sub(1));
    let s: f32 = a.sal.get(i).map_or(0.0, |f| f.iter().sum());
    s
}

pub fn arrange(a: &Analysis, piano_only: bool) -> Vec<Part> {
    let sp = spans(a);
    let mut lmax = 1e-9f32;
    for i in (0..a.sal.len()).step_by(8) {
        lmax = lmax.max(a.sal[i].iter().sum());
    }
    let vel = |t: f64, base: f32, span: f32| -> u8 { (base + span * (loud(a, t) / lmax).sqrt().min(1.0)).round().clamp(1.0, 127.0) as u8 };
    let beat = if a.beats.len() > 1 { (a.beats[a.beats.len() - 1] - a.beats[0]) / (a.beats.len() - 1) as f64 } else { 0.6 };
    let mut piano: Vec<Note> = Vec::new();
    let (mut strings, mut bass, mut harp) = (Vec::new(), Vec::new(), Vec::new());
    let mut prev_rh: Vec<u8> = Vec::new();
    let mut prev_pad: Vec<u8> = Vec::new();
    for &(t0, t1, c) in &sp {
        let pcs = c.pcs();
        let root = c.root;
        let bs = beats_in(a, t0, t1);
        // left hand: root octave (C1..B2 bass + octave) re-struck every 2 beats, the fifth on the off-beats
        let low = 36 + ((root + 12 - 0) % 12);
        let low = if low > 45 { low - 12 } else { low };
        for (i, &b) in bs.iter().enumerate() {
            if i % 2 == 0 {
                let end = bs.get(i + 2).copied().unwrap_or(t1);
                piano.push(Note { t: b, dur: end - b, pitch: low, vel: vel(b, 58.0, 30.0) });
                piano.push(Note { t: b, dur: end - b, pitch: low + 12, vel: vel(b, 50.0, 26.0) });
            } else {
                let end = bs.get(i + 1).copied().unwrap_or(t1);
                piano.push(Note { t: b, dur: end - b, pitch: low + 19, vel: vel(b, 44.0, 24.0) });
            }
        }
        // right hand chord: voiced in E3..G4 with voice-leading, struck on the chord and every 2 beats
        let rh = voice(&pcs, 52, 70, &prev_rh);
        for (i, &b) in bs.iter().enumerate() {
            if i % 2 == 0 {
                let end = bs.get(i + 2).copied().unwrap_or(t1);
                for &p in &rh {
                    piano.push(Note { t: b + 0.01, dur: end - b - 0.01, pitch: p, vel: vel(b, 46.0, 28.0) });
                }
            }
        }
        prev_rh = rh;
        // strings: sustained, voiced lower-middle, the whole span
        let pad = voice(&pcs, 48, 67, &prev_pad);
        for &p in &pad {
            strings.push(Note { t: t0, dur: t1 - t0, pitch: p, vel: vel(t0, 40.0, 30.0) });
        }
        prev_pad = pad;
        // bass: root on each beat 1 and 3 (every 2 beats), a fifth passing on 4 when the span is long
        for (i, &b) in bs.iter().enumerate() {
            if i % 2 == 0 {
                let end = bs.get(i + 2).copied().unwrap_or(t1);
                bass.push(Note { t: b, dur: (end - b) * 0.95, pitch: 28 + (root + 12 - 4) % 12, vel: vel(b, 70.0, 30.0) });
            }
        }
        // harp: chord arpeggio in 8ths, up through two octaves from C4
        let arp: Vec<u8> = {
            let mut v = voice(&pcs, 60, 72, &[]);
            let up: Vec<u8> = v.iter().map(|x| x + 12).collect();
            v.extend(up);
            v
        };
        let mut k = 0usize;
        for &b in &bs {
            for h in 0..2 {
                let t = b + h as f64 * beat / 2.0;
                if t >= t1 {
                    break;
                }
                harp.push(Note { t, dur: beat * 0.9, pitch: arp[k % arp.len()], vel: vel(t, 38.0, 30.0) });
                k += 1;
            }
        }
    }
    // melody on top of the piano (octave up if it collides with the right-hand chord)
    let mut lead = Vec::new();
    for m in &a.melody {
        let mut p = m.pitch;
        while p < 67 {
            p += 12;
        }
        piano.push(Note { t: m.t, dur: m.dur, pitch: p, vel: (m.vel as f32 * 0.9 + 12.0).min(120.0) as u8 });
        lead.push(Note { t: m.t, dur: m.dur, pitch: p, vel: m.vel });
    }
    let mut parts = vec![Part { name: "Piano", ch: 0, prog: 0, rgb: (0xc8, 0xd6, 0xe5), notes: piano }];
    if !piano_only {
        parts.push(Part { name: "Strings", ch: 1, prog: 48, rgb: (0x9b, 0x59, 0xb6), notes: strings });
        parts.push(Part { name: "Lead", ch: 2, prog: 73, rgb: (0xfe, 0xca, 0x57), notes: lead });
        parts.push(Part { name: "Bass", ch: 3, prog: 32, rgb: (0x54, 0xa0, 0xff), notes: bass });
        parts.push(Part { name: "Harp", ch: 4, prog: 46, rgb: (0x1d, 0xd1, 0xa1), notes: harp });
    }
    parts
}

pub fn midi(parts: &[Part]) -> Vec<u8> {
    let tracks: Vec<(TrackSpec, Vec<Note>)> =
        parts.iter().map(|p| (TrackSpec { ch: p.ch, prog: Some(p.prog), name: Some(p.name.to_string()) }, p.notes.clone())).collect();
    crate::notes::smf_bytes(&tracks, Some(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_and_chords_on_a_synth_progression() {
        // F - Bb - C - F, block chords, 2 s each at 120 BPM-ish pulses (re-struck every 0.5 s)
        let prog: [&[u8]; 4] = [&[41, 57, 60, 65, 69], &[46, 58, 62, 65, 70], &[48, 55, 60, 64, 67], &[41, 57, 60, 65, 69]];
        let mut notes = Vec::new();
        for (i, c) in prog.iter().enumerate() {
            for s in 0..4 {
                for &p in c.iter() {
                    notes.push(Note { t: i as f64 * 2.0 + s as f64 * 0.5, dur: 0.45, pitch: p, vel: 90 });
                }
            }
        }
        let x = piano::synth_piano(&notes, 8.5, 1);
        let a = analyze(&x, RATE);
        assert_eq!(a.key, (5, true), "key {}", a.key_name());
        assert!((a.bpm - 120.0).abs() < 8.0 || (a.bpm - 60.0).abs() < 4.0, "bpm {}", a.bpm);
        let at = |t: f64| a.beats.windows(2).position(|w| w[0] <= t && t < w[1]).map(|i| a.chords[i].name());
        assert_eq!(at(1.0).as_deref(), Some("F"));
        assert_eq!(at(3.0).as_deref(), Some("Bb"));
        assert_eq!(at(5.0).as_deref(), Some("C"));
    }
}

/// Tuning override for experiments: ARR_TUNE="NAME=v,...".
fn tun_f(name: &str, def: f32) -> f32 {
    std::env::var("ARR_TUNE")
        .ok()
        .and_then(|s| s.split(',').find_map(|kv| kv.split_once('=').filter(|(k, _)| k.trim() == name).and_then(|(_, v)| v.trim().parse().ok())))
        .unwrap_or(def)
}
