//! Song -> piano MIDI with our own DSP (#23 phase 1). Pure Rust, CPU only: no model, no GTK.
//!
//! Per 11.6 ms frame (hop 256 at 22 050 Hz):
//! 1. three real FFTs (4096 / 2048 / 1024, Hann, all centred on the same sample, so the frame is
//!    93 ms behind the input). The long window owns the log axis below E4, the middle one E4..C6,
//!    the short one above. Every FFT peak is parabolically interpolated and lands on a log axis of
//!    3 bins per semitone (A0 up to ~10.5 kHz) as its ratio over a local noise floor (the 20th
//!    percentile of its neighbourhood, so noise sits at 1, and never less than 50 dB under the frame
//!    peak). Only peaks are kept, so a partial's window skirt never lights the keys beside it.
//! 2. salience of each of the 88 keys: a harmonic sum over 20 partials (weight 0.95^(h-1),
//!    cube-root compressed ratios), with partial positions followed up the series with a fitted
//!    string stretch (piano inharmonicity), and spectral smoothness (a partial counts at most the
//!    mean of its neighbours) so a sub-octave candidate cannot ride on the even partials.
//! 3. estimate-and-cancel: take the most salient key, subtract its smoothed partials, repeat (max
//!    polyphony 10). A key whose fundamental sits on a partial of a picked key (octave, twelfth,
//!    double octave, ...) must keep half its salience and most of its first partials after that
//!    cancel. A key also needs a real peak at its fundamental.
//! 4. a per-key tracker turns picked keys into notes: start only at an energy rise of the key's
//!    own partials (larger than its neighbours'), on/off hysteresis, emitted once 60 ms long (shorter
//!    ones never reach the output), off after 2 quiet frames, and re-strikes of a held key from a
//!    rise of its short-window partial energy that coincides with a global spectral-flux onset.
//!    The onset time is the steepest rise of that energy, velocity its peak against a per-note AGC.
//!
//! `PianoTracker::push` is streaming and stateful, so the live tap and the offline path share it.
#![allow(dead_code)]
use crate::notes::Note;

/// Analysis rate. Everything above it is resampled to this first.
pub const RATE: u32 = 22050;
/// Frame hop in samples (11.6 ms).
pub const HOP: usize = 256;
pub const LOW_KEY: u8 = 21;
pub const HIGH_KEY: u8 = 108;
pub const KEYS: usize = 88;
/// Bins per semitone on the log axis. Key k sits on bin 3k+1.
const BPS: usize = 3;
/// The log axis runs up to MIDI 124 (~10.5 kHz), past C8, so high keys keep their 2nd partial.
const TOP_MIDI: usize = 124;
const NBINS: usize = (TOP_MIDI - LOW_KEY as usize) * BPS;
const SIZES: [usize; 3] = [4096, 2048, 1024];
/// Centre delay: a frame centred on sample c needs input up to c + 2048.
pub const DELAY: usize = 2048;
/// Partials summed into the salience, and their weight decay.
const HARMS: usize = 20;
const HARM_DECAY: f32 = 0.95;
/// Partials subtracted on a cancel.
const CANCEL_HARMS: usize = 48;
/// Stretch model for the cancel: partial h sits a*h^2 bins above h*f0. `a` is fitted per note with
/// this prior weight (as if partials up to ~10 had shown no stretch) and capped (B ~ 4e-3).
const STRETCH_PRIOR: f32 = 1e4;
const STRETCH_MAX: f32 = 0.1;
pub const MAX_POLY: usize = 10;
/// Whitening: the 20th percentile of ±FLOOR_HALF FFT bins, times 1.76 (the Rayleigh median / p20
/// ratio, so noise sits at 1), evaluated every FLOOR_STEP bins; plus a floor 50 dB under the frame
/// peak and an absolute floor.
const FLOOR_HALF: usize = 16;
const FLOOR_STEP: usize = 8;
const FLOOR_PCT: f32 = 0.2;
const FLOOR_SCALE: f32 = 1.76;
const DYN_RANGE: f32 = 3e-3;
const ABS_FLOOR: f32 = 1e-5;
/// Salience thresholds (whitened, compressed units). A new key needs ON, a sounding one keeps going
/// down to OFF. REL is relative to the strongest key of the frame.
const ON_ABS: f32 = 7.0;
const OFF_ABS: f32 = 4.5;
/// Keys with fewer partials than this under the axis top get a proportionally lower ABS threshold.
const WFRAC_REF: usize = 6;
const ON_REL: f32 = 0.22;
const OFF_REL: f32 = 0.12;
/// An octave/twelfth/double-octave of a picked key must keep this share of its salience after the cancel.
const GHOST_KEEP: f32 = 0.5;
/// ...and at least 3 of its first 4 partials must keep this share of their linear energy.
const GHOST_PART: f32 = 0.3;
/// A key's fundamental must be a residual peak this far over the floor; below this MIDI key any of its first three partials will do.
const FUND_MIN: f32 = 2.0;
const WEAK_FUND_BELOW: usize = 40;
/// Tracker timing.
pub const MIN_NOTE_S: f64 = 0.060;
const OFF_FRAMES: u32 = 2;
const RETRIG_S: f64 = 0.050;
/// A held key re-strikes when its short-window energy jumps by this factor over its recent minimum.
const RESTRIKE: f32 = 1.4;
const RESTRIKE_LOOK: usize = 6;
const RESTRIKE_PART: f32 = 1.25;
/// Global flux: bins of the 1024 FFT used, and the onset level relative to its running mean.
const GF_BINS: usize = 256;
const GF_ONSET: f32 = 2.0;
/// A new note needs its short-window energy to have risen this much within the last ONSET_WIN frames.
const ONSET_RISE: f32 = 1.5;
const ONSET_WIN: usize = 12;
/// ...and at least this share of the larger rise of its two neighbouring keys.
const NEIGH_RISE: f32 = 0.7;
/// Semitones from a fundamental to its partials 2..8 and 10, 12, 16.
const HARMONIC_STEPS: [usize; 10] = [12, 19, 24, 28, 31, 34, 36, 40, 43, 48];
/// Per-key short-window energy ring (frames).
const RING: usize = 32;

/// Tuning constants are compile-time; test builds can override them from PIANO_TUNE="NAME=v,..."
/// so a parameter sweep needs no rebuild.
#[cfg(not(test))]
macro_rules! tun {
    ($n:ident) => {
        $n
    };
}
#[cfg(test)]
macro_rules! tun {
    ($n:ident) => {
        tune(stringify!($n), $n)
    };
}

#[cfg(test)]
fn tune(name: &str, def: f32) -> f32 {
    static T: std::sync::OnceLock<Vec<(String, f32)>> = std::sync::OnceLock::new();
    let t = T.get_or_init(|| {
        std::env::var("PIANO_TUNE")
            .unwrap_or_default()
            .split(',')
            .filter_map(|kv| kv.split_once('=').and_then(|(k, v)| Some((k.trim().to_string(), v.trim().parse().ok()?))))
            .collect()
    });
    t.iter().find(|(k, _)| k == name).map_or(def, |x| x.1)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Ev {
    /// `t` is the note start in seconds since the tracker started (sample-accurate at RATE).
    On { t: f64, pitch: u8, vel: u8 },
    Off { t: f64, pitch: u8 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lane {
    Bass,
    Mid,
    High,
}

pub const LANES: [Lane; 3] = [Lane::Bass, Lane::Mid, Lane::High];

/// Bass < C3 (48), Mid C3..=C5 (48..=72), High > C5. The same notes, split by pitch.
pub fn lane(pitch: u8) -> Lane {
    if pitch < 48 {
        Lane::Bass
    } else if pitch <= 72 {
        Lane::Mid
    } else {
        Lane::High
    }
}

impl Lane {
    pub fn index(self) -> usize {
        match self {
            Lane::Bass => 0,
            Lane::Mid => 1,
            Lane::High => 2,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Lane::Bass => "Bass",
            Lane::Mid => "Mid",
            Lane::High => "High",
        }
    }
}

/// Splits notes into [Bass, Mid, High].
pub fn split_lanes(notes: &[Note]) -> [Vec<Note>; 3] {
    let mut out = [Vec::new(), Vec::new(), Vec::new()];
    for n in notes {
        out[lane(n.pitch).index()].push(n.clone());
    }
    out
}

/// Peak-to-floor ratio -> compressed salience units (cube root, 0 at or under the floor).
fn comp(r: f32) -> f32 {
    if r > 1.0 {
        r.cbrt() - 1.0
    } else {
        0.0
    }
}

fn midi_hz(m: f64) -> f64 {
    440.0 * 2f64.powf((m - 69.0) / 12.0)
}

/// Real FFT of length n through an n/2 complex FFT, Hann window, magnitudes in amplitude units
/// (a steady sine of amplitude A reads A on its bin).
struct Rfft {
    n: usize,
    m: usize,
    rev: Vec<u32>,
    tw: Vec<(f32, f32)>,
    post: Vec<(f32, f32)>,
    win: Vec<f32>,
    scale: f32,
    re: Vec<f32>,
    im: Vec<f32>,
    mag: Vec<f32>,
}

impl Rfft {
    fn new(n: usize) -> Rfft {
        let m = n / 2;
        let bits = m.trailing_zeros();
        let rev = (0..m as u32).map(|i| i.reverse_bits() >> (32 - bits)).collect();
        let tw = (0..m / 2)
            .map(|k| {
                let a = -2.0 * std::f64::consts::PI * k as f64 / m as f64;
                (a.cos() as f32, a.sin() as f32)
            })
            .collect();
        let post = (0..=m)
            .map(|k| {
                let a = -2.0 * std::f64::consts::PI * k as f64 / n as f64;
                (a.cos() as f32, a.sin() as f32)
            })
            .collect();
        let win = crate::dsp::hann(n);
        let scale = 2.0 / win.iter().sum::<f32>();
        Rfft { n, m, rev, tw, post, win, scale, re: vec![0.0; m], im: vec![0.0; m], mag: vec![0.0; m + 1] }
    }

    /// `x` is exactly n samples.
    fn run(&mut self, x: &[f32]) {
        let m = self.m;
        for j in 0..m {
            let r = self.rev[j] as usize;
            self.re[r] = x[2 * j] * self.win[2 * j];
            self.im[r] = x[2 * j + 1] * self.win[2 * j + 1];
        }
        let mut len = 2;
        while len <= m {
            let half = len / 2;
            let step = m / len;
            let mut start = 0;
            while start < m {
                for k in 0..half {
                    let (wr, wi) = self.tw[k * step];
                    let a = start + k;
                    let b = a + half;
                    let (br, bi) = (self.re[b], self.im[b]);
                    let tr = br * wr - bi * wi;
                    let ti = br * wi + bi * wr;
                    self.re[b] = self.re[a] - tr;
                    self.im[b] = self.im[a] - ti;
                    self.re[a] += tr;
                    self.im[a] += ti;
                }
                start += len;
            }
            len <<= 1;
        }
        for k in 0..=m {
            let kk = k % m;
            let km = (m - k) % m;
            let (zr, zi) = (self.re[kk], self.im[kk]);
            let (cr, ci) = (self.re[km], -self.im[km]);
            let (er, ei) = (0.5 * (zr + cr), 0.5 * (zi + ci));
            let (dr, di) = (0.5 * (zr - cr), 0.5 * (zi - ci));
            let (or, oi) = (di, -dr);
            let (wr, wi) = self.post[k];
            let xr = er + or * wr - oi * wi;
            let xi = ei + or * wi + oi * wr;
            self.mag[k] = (xr * xr + xi * xi).sqrt() * self.scale;
        }
    }
}

/// Which FFT serves a log bin, and the FFT bin range it takes the max over.
#[derive(Clone, Copy)]
struct Kern {
    size: u8,
    k0: u16,
    k1: u16,
}

fn size_for(midi: f64) -> usize {
    if midi < 64.0 {
        0
    } else if midi < 84.0 {
        1
    } else {
        2
    }
}

fn kern(midi: f64, size: usize, half_width_semi: f64) -> Kern {
    let n = SIZES[size];
    let df = RATE as f64 / n as f64;
    let lo = midi_hz(midi - half_width_semi) / df;
    let hi = midi_hz(midi + half_width_semi) / df;
    let k0 = (lo.round() as usize).min(n / 2);
    let k1 = (hi.round() as usize).clamp(k0, n / 2);
    Kern { size: size as u8, k0: k0 as u16, k1: k1 as u16 }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum KeyState {
    Idle,
    /// Active, not yet long enough to emit. `act` is the first active frame.
    Pending { act: i64 },
    /// Emitted. `t0` is its start in seconds.
    On { t0: f64, since: i64 },
}

/// Streaming song -> piano note tracker. Feed mono PCM at `RATE`.
pub struct PianoTracker {
    ffts: [Rfft; 3],
    /// Per key, per partial 1..=4: the short-window FFT bin range for the onset energy.
    ekern: Vec<[Kern; 4]>,
    /// The same at the resolution of the start gate, and its ring.
    gkern: Vec<[Kern; 4]>,
    genergy: Vec<[f32; RING]>,
    /// Per key, per frame, per partial 1..=4: the fast-window energy.
    henergy: Vec<[[f32; 4]; RING]>,
    /// Global flux ring (relative to its running mean), the previous log spectrum, the mean.
    gflux: [f32; RING],
    gf_prev: Vec<f32>,
    gf_mean: f32,
    /// Input ring: samples from absolute index `buf_start`.
    buf: Vec<f32>,
    buf_start: u64,
    n_in: u64,
    /// Next frame index; its centre is frame * HOP.
    frame: i64,
    /// Per FFT size, per bin: the noise floor.
    floor: [Vec<f32>; 3],
    /// Per FFT size: the bin range whose peaks belong to its part of the log axis.
    peak_range: [(usize, usize); 3],
    lin: Vec<f32>,
    /// Fractional log-axis position of the peak held in each bin.
    pos: Vec<f32>,
    explained: Vec<bool>,
    /// Per key: tracked partial bins for this frame, and how many.
    pb: Vec<[u16; HARMS]>,
    nh: Vec<u8>,
    /// Per key: the share of the full harmonic weight its partials under the axis top can reach (<= 1).
    wfrac: Vec<f32>,
    min_abs: f32,
    /// Window scratch for the FFT input.
    xbuf: Vec<f32>,
    /// Log-axis offset of partial h+1 over the fundamental: 36 log2(h+1) bins.
    hoff: Vec<f32>,
    white: Vec<f32>,
    res_lin: Vec<f32>,
    res: Vec<f32>,
    scratch: Vec<f32>,
    sal0: Vec<f32>,
    salr: Vec<f32>,
    picked: Vec<usize>,
    /// Per key: short-window harmonic energy for the last RING frames, indexed frame % RING.
    energy: Vec<[f32; RING]>,
    state: Vec<KeyState>,
    /// Per key: half the onset-energy window, in frames.
    ehalf: Vec<u8>,
    quiet: Vec<u32>,
    last_trig: Vec<i64>,
    agc_db: Option<f32>,
    /// Cumulative processing time, for the CPU readout.
    pub busy: std::time::Duration,
}

impl Default for PianoTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl PianoTracker {
    pub fn new() -> PianoTracker {
        let ekern_for = |fast: bool| -> Vec<[Kern; 4]> {
            (0..KEYS)
                .map(|k| {
                    let m0 = LOW_KEY as f64 + k as f64;
                    // timing wants the 1024 window (±23 ms) wherever it still resolves the key;
                    // the start gate wants the key on its own, one size shorter than its salience window
                    // fast: the 1024 window (±23 ms) on the first four partials at or above 360 Hz,
                    // where its 21.5 Hz bins still split semitones
                    let (s, h0) = if !fast {
                        ((size_for(m0) + 1).min(2), 1)
                    } else {
                        (2, (360.0 / midi_hz(m0)).ceil().max(1.0) as usize)
                    };
                    let mut a = [Kern { size: 0, k0: 0, k1: 0 }; 4];
                    for (h, slot) in a.iter_mut().enumerate() {
                        let m = m0 + 12.0 * ((h + h0) as f64).log2();
                        *slot = kern(m, s, 0.33);
                    }
                    a
                })
                .collect()
        };
        let ekern = ekern_for(true);
        let gkern = ekern_for(false);
        PianoTracker {
            ffts: [Rfft::new(SIZES[0]), Rfft::new(SIZES[1]), Rfft::new(SIZES[2])],
            ekern,
            gkern,
            genergy: vec![[0.0; RING]; KEYS],
            henergy: vec![[[0.0; 4]; RING]; KEYS],
            gflux: [0.0; RING],
            gf_prev: vec![(1e-5f32).ln(); GF_BINS],
            gf_mean: 0.0,
            buf: Vec::with_capacity(SIZES[0] * 4),
            buf_start: 0,
            n_in: 0,
            frame: 0,
            floor: [vec![0.0; SIZES[0] / 2 + 1], vec![0.0; SIZES[1] / 2 + 1], vec![0.0; SIZES[2] / 2 + 1]],
            peak_range: [0usize, 1, 2].map(|s| {
                let df = RATE as f64 / SIZES[s] as f64;
                let (m_lo, m_hi) = [(LOW_KEY as f64 - 1.0, 64.0), (64.0, 84.0), (84.0, TOP_MIDI as f64 + 1.0)][s];
                let lo = midi_hz(m_lo - 0.5) / df;
                let hi = midi_hz(m_hi + 0.5) / df;
                ((lo.floor() as usize).max(1), (hi.ceil() as usize + 2).min(SIZES[s] / 2))
            }),
            lin: vec![0.0; NBINS],
            pos: vec![0.0; NBINS],
            explained: vec![false; NBINS],
            pb: vec![[0; HARMS]; KEYS],
            nh: vec![0; KEYS],
            wfrac: (0..KEYS)
                .map(|k| {
                    let w = |n: usize| (0..n).map(|h| HARM_DECAY.powi(h as i32)).sum::<f32>();
                    let avail = (1..=HARMS).take_while(|&h| Self::harm_bin(k, h) + 1 < NBINS).count();
                    (w(avail) / w(WFRAC_REF)).min(1.0)
                })
                .collect(),
            white: vec![0.0; NBINS],
            res_lin: vec![0.0; NBINS],
            res: vec![0.0; NBINS],
            scratch: Vec::with_capacity(2 * FLOOR_HALF + 1),
            sal0: vec![0.0; KEYS],
            salr: vec![0.0; KEYS],
            picked: Vec::with_capacity(MAX_POLY),
            energy: vec![[0.0; RING]; KEYS],
            state: vec![KeyState::Idle; KEYS],
            ehalf: vec![(SIZES[2] / 2 / HOP) as u8; KEYS],
            quiet: vec![0; KEYS],
            last_trig: vec![i64::MIN / 2; KEYS],
            agc_db: None,
            min_abs: 0.0,
            xbuf: vec![0.0; SIZES[0]],
            hoff: (1..=CANCEL_HARMS).map(|h| (BPS as f32 * 12.0) * (h as f32).log2()).collect(),
            busy: std::time::Duration::ZERO,
        }
    }

    /// Seconds of input consumed so far.
    pub fn now(&self) -> f64 {
        self.n_in as f64 / RATE as f64
    }

    /// Feeds mono samples at RATE; returns the events that became certain.
    pub fn push(&mut self, pcm: &[f32]) -> Vec<Ev> {
        let t = std::time::Instant::now();
        self.buf.extend_from_slice(pcm);
        self.n_in += pcm.len() as u64;
        let mut out = Vec::new();
        loop {
            let c = self.frame * HOP as i64;
            if c + DELAY as i64 > self.n_in as i64 {
                break;
            }
            self.analyse(c);
            self.track(&mut out);
            self.frame += 1;
            // keep everything the next frame's longest window still needs
            let need_from = (self.frame * HOP as i64 - SIZES[0] as i64 / 2).max(0) as u64;
            if need_from > self.buf_start + SIZES[0] as u64 * 2 {
                let drop = (need_from - self.buf_start) as usize;
                self.buf.drain(..drop);
                self.buf_start += drop as u64;
            }
        }
        self.busy += t.elapsed();
        out
    }

    /// End of stream: runs the last frames over zero padding and closes every note.
    pub fn flush(&mut self) -> Vec<Ev> {
        let mut out = self.push(&vec![0.0; DELAY + HOP]);
        out.extend(self.all_off(self.frame as f64 * HOP as f64 / RATE as f64));
        out
    }

    /// Ends every sounding note at `t` and forgets pending ones (stop, source change).
    pub fn all_off(&mut self, t: f64) -> Vec<Ev> {
        let mut out = Vec::new();
        for k in 0..KEYS {
            if let KeyState::On { t0, .. } = self.state[k] {
                out.push(Ev::Off { t: t.max(t0), pitch: LOW_KEY + k as u8 });
            }
            self.state[k] = KeyState::Idle;
            self.quiet[k] = 0;
        }
        out
    }

    /// Fills `spec` (log axis), `white` and `energy` for the frame centred on sample c.
    fn analyse(&mut self, c: i64) {
        for s in 0..3 {
            let n = SIZES[s];
            let start = c - n as i64 / 2;
            let x = &mut self.xbuf[..n];
            x.iter_mut().for_each(|v| *v = 0.0);
            // samples before the stream start (or already dropped) read as silence
            let lo = (self.buf_start as i64 - start).max(0) as usize;
            let hi = ((self.buf_start as i64 + self.buf.len() as i64) - start).clamp(0, n as i64) as usize;
            if lo < hi {
                let from = (start + lo as i64 - self.buf_start as i64) as usize;
                x[lo..hi].copy_from_slice(&self.buf[from..from + (hi - lo)]);
            }
            self.ffts[s].run(x);
        }
        // noise floor per FFT bin: a low percentile of the neighbourhood, scaled so that for noise it
        // equals the median. A percentile (not the median) keeps working where the window is too
        // short to resolve the partials of a low note, whose main lobes would fill a median.
        let mut peak = 0f32;
        for s in 0..3 {
            let (k_lo, k_hi) = self.peak_range[s];
            let mag = &self.ffts[s].mag;
            for &m in &mag[k_lo..k_hi] {
                peak = peak.max(m);
            }
        }
        let floor_dyn = (peak * tun!(DYN_RANGE)).max(ABS_FLOOR);
        for s in 0..3 {
            let (k_lo, k_hi) = self.peak_range[s];
            let mag = &self.ffts[s].mag;
            let fl = &mut self.floor[s];
            let mut k = k_lo;
            while k < k_hi {
                let w = FLOOR_HALF;
                let lo = k.saturating_sub(w).max(1);
                let hi = (k + w).min(mag.len() - 1);
                self.scratch.clear();
                self.scratch.extend_from_slice(&mag[lo..=hi]);
                let q = (self.scratch.len() as f32 * FLOOR_PCT) as usize;
                let (_, v, _) = self.scratch.select_nth_unstable_by(q, |a, b| a.total_cmp(b));
                let v = (*v * tun!(FLOOR_SCALE)).max(floor_dyn);
                for j in k..(k + FLOOR_STEP).min(k_hi) {
                    fl[j] = v;
                }
                k += FLOOR_STEP;
            }
        }
        // salience works on spectral peaks only: each FFT local maximum, parabolically interpolated,
        // lands on its own log bin as its peak-to-floor ratio, so a partial's window skirt never
        // lights the neighbouring keys
        self.lin.iter_mut().for_each(|v| *v = 0.0);
        for s in 0..3 {
            let (k_lo, k_hi) = self.peak_range[s];
            let df = RATE as f32 / SIZES[s] as f32;
            let mag = &self.ffts[s].mag;
            for k in k_lo.max(1)..k_hi.min(mag.len() - 1) {
                let (a, b, c) = (mag[k - 1], mag[k], mag[k + 1]);
                if !(b > a && b >= c) || b <= self.floor[s][k] {
                    continue;
                }
                let (la, lb, lc) = (a.max(1e-12).ln(), b.ln(), c.max(1e-12).ln());
                let den = la - 2.0 * lb + lc;
                let d = if den.abs() > 1e-12 { (0.5 * (la - lc) / den).clamp(-0.5, 0.5) } else { 0.0 };
                let amp = (lb - 0.25 * (la - lc) * d).exp();
                // ownership by the bin centre, so a peak on a band edge belongs to exactly one FFT
                let fk = k as f32 * df;
                if size_for(69.0 + 12.0 * (fk as f64 / 440.0).log2()) != s {
                    continue;
                }
                let f = (k as f32 + d) * df;
                let midi = 69.0 + 12.0 * (f / 440.0).log2();
                let bin = ((midi - LOW_KEY as f32) * BPS as f32 + 1.0).round();
                if bin < 0.0 || bin >= NBINS as f32 {
                    continue;
                }
                let bin = bin as usize;
                let r = amp / self.floor[s][k];
                if r > self.lin[bin] {
                    self.lin[bin] = r;
                    self.pos[bin] = (midi - LOW_KEY as f32) * BPS as f32 + 1.0;
                }
            }
        }
        for b in 0..NBINS {
            self.white[b] = comp(self.lin[b]);
        }
        let slot = (self.frame.rem_euclid(RING as i64)) as usize;
        // global spectral flux on the 1024 window (log magnitude, up to ~5.5 kHz)
        {
            let mag = &self.ffts[2].mag;
            let mut fl = 0f32;
            for (i, &m) in mag[1..GF_BINS].iter().enumerate() {
                let l = (m + 1e-5).ln();
                fl += (l - self.gf_prev[i]).max(0.0);
                self.gf_prev[i] = l;
            }
            let mean = self.gf_mean;
            self.gflux[slot] = if mean > 0.0 { fl / mean } else { 0.0 };
            // slow running mean (~0.4 s), seeded by the first frames
            self.gf_mean = if mean > 0.0 { mean + (fl - mean) * 0.03 } else { fl.max(1e-3) };
        }
        let sum = |ffts: &[Rfft; 3], kerns: &[Kern; 4]| -> f32 {
            kerns
                .iter()
                .map(|kn| ffts[kn.size as usize].mag[kn.k0 as usize..=kn.k1 as usize].iter().fold(0f32, |a, &m| a.max(m)))
                .sum()
        };
        for k in 0..KEYS {
            let mut tot = 0f32;
            for (h, kn) in self.ekern[k].iter().enumerate() {
                let v = self.ffts[kn.size as usize].mag[kn.k0 as usize..=kn.k1 as usize].iter().fold(0f32, |a, &m| a.max(m));
                self.henergy[k][slot][h] = v;
                tot += v;
            }
            self.energy[k][slot] = tot;
            self.genergy[k][slot] = sum(&self.ffts, &self.gkern[k]);
        }
    }

    fn harm_bin(k: usize, h: usize) -> usize {
        k * BPS + 1 + (BPS as f64 * 12.0 * (h as f64).log2()).round() as usize
    }

    /// Weighted harmonic sum of key k over `spec`, at the partial bins tracked for this frame.
    fn salience(&self, spec: &[f32], k: usize) -> f32 {
        // spectral smoothness: a partial counts at most as much as the mean of its two neighbours,
        // so a sub-octave candidate, whose odd partials are empty, cannot ride on the even ones
        let n = self.nh[k] as usize;
        let mut a = [0f32; HARMS + 1];
        for h in 0..n {
            let c = self.pb[k][h] as usize;
            a[h] = spec[c - 1].max(spec[c]).max(spec[c + 1]);
        }
        let mut s = 0f32;
        let mut w = 1f32;
        let decay = tun!(HARM_DECAY);
        for h in 0..n {
            let v = if h == 0 {
                a[0]
            } else if h + 1 < n {
                a[h].min(0.5 * (a[h - 1] + a[h + 1]))
            } else {
                a[h].min(a[h - 1])
            };
            s += w * v;
            w *= decay;
        }
        s
    }

    /// Follows the partials of key k up the series on the original frame: partial h is looked for
    /// ±1 bin around h*f0 raised by a running least-squares estimate of the string stretch
    /// (inharmonicity, a*h^2 bins), so the high partials of a piano note are found where they are.
    /// Writes the bin of each partial and returns how many fit under the top of the log axis.
    fn partial_bins(&self, k: usize, out: &mut [u16]) -> usize {
        let base = (k * BPS + 1) as f32;
        let (mut num, mut den) = (0f32, STRETCH_PRIOR);
        let mut n = 0;
        for (i, slot) in out.iter_mut().enumerate() {
            let hf = (i + 1) as f32;
            let ideal = base + self.hoff[i];
            let a = (num / den).clamp(0.0, STRETCH_MAX);
            let c = (ideal + a * hf * hf).round() as usize;
            if c + 1 >= NBINS {
                break;
            }
            let mut bm = c;
            for b in c - 1..=c + 1 {
                if self.lin[b] > self.lin[bm] {
                    bm = b;
                }
            }
            if i >= 1 && self.lin[bm] > 2.0 {
                let st = self.pos[bm] - ideal;
                num += st * hf * hf;
                den += hf * hf * hf * hf;
            }
            // a partial found off its centre is used where it is; an empty slot keeps the prediction
            *slot = if self.lin[bm] > 0.0 { bm.clamp(1, NBINS - 2) } else { c.max(1) } as u16;
            n = i + 1;
        }
        n
    }

    /// Subtracts the spectrally smoothed partials of key k, up to the top of the log axis, from the
    /// linear residual `res_lin` (an octave note on top of a picked key adds to its even partials,
    /// which then stand out above their neighbours and survive), refreshes the compressed copy `res`,
    /// and marks the bins as explained. Partials are followed up the series with a running
    /// least-squares estimate of the string stretch (inharmonicity), so the high partials of a low
    /// piano note are found where they really are.
    fn cancel(&mut self, k: usize) {
        let mut bins = [0u16; CANCEL_HARMS];
        let n = self.partial_bins(k, &mut bins);
        let mut found: [(usize, f32); CANCEL_HARMS + 2] = [(0, 0.0); CANCEL_HARMS + 2];
        for h in 1..=n {
            let bm = bins[h - 1] as usize;
            let amp = self.res_lin[bm - 1].max(self.res_lin[bm]).max(self.res_lin[bm + 1]);
            found[h] = (bm, amp);
        }
        for h in 1..=n {
            let amp = |i: usize| found[i].1;
            let smooth = if h == 1 {
                amp(1)
            } else if h < n {
                amp(h).min(0.5 * (amp(h - 1) + amp(h + 1)))
            } else {
                amp(h).min(amp(h - 1))
            };
            let c = found[h].0;
            for b in c - 1..=c + 1 {
                self.res_lin[b] = (self.res_lin[b] - smooth).max(0.0);
                self.res[b] = comp(self.res_lin[b]);
                self.explained[b] = true;
            }
        }
    }

    /// An octave-family candidate shares every partial with the key below it. It is real only if most
    /// of its first partials keep a good share of their energy after that key's cancel: an added
    /// octave lifts all of them, while an uneven partial series lifts only a few.
    fn own_partials(&self, k: usize) -> bool {
        let (mut seen, mut kept) = (0, 0);
        for h in 0..(self.nh[k] as usize).min(4) {
            let c = self.pb[k][h] as usize;
            let orig = self.lin[c - 1].max(self.lin[c]).max(self.lin[c + 1]);
            let res = self.res_lin[c - 1].max(self.res_lin[c]).max(self.res_lin[c + 1]);
            seen += 1;
            if orig > 1.0 && res >= tun!(GHOST_PART) * orig {
                kept += 1;
            }
        }
        seen > 0 && kept * 4 >= seen * 3
    }

    /// A key needs a real peak of its own at its fundamental (in the bottom octaves, where piano
    /// fundamentals are weak, at one of its first three partials). Without this, a key whose even
    /// partials happen to land on another note's partials (a fifth below a partial, ...) collects
    /// enough leftovers to be picked.
    fn has_fundamental(&self, k: usize) -> bool {
        // a sounding key may lose its fundamental peak for a few frames (a re-strike inside the long
        // window interferes with the old phase); it keeps going on its next partials
        let m = if k + (LOW_KEY as usize) < WEAK_FUND_BELOW || self.sounding(k) { 3 } else { 1 };
        (0..(self.nh[k] as usize).min(m)).any(|h| {
            let c = self.pb[k][h] as usize;
            self.res_lin[c - 1].max(self.res_lin[c]).max(self.res_lin[c + 1]) >= tun!(FUND_MIN)
        })
    }

    fn sounding(&self, k: usize) -> bool {
        !matches!(self.state[k], KeyState::Idle)
    }

    /// Estimate-and-cancel over the whitened frame; fills `picked`.
    fn pick(&mut self) {
        self.picked.clear();
        let fund_min = tun!(FUND_MIN);
        for k in 0..KEYS {
            // a key with no peak near its first three partials can never pass `has_fundamental`:
            // skip its partial tracking and salience (most keys, most frames)
            let base = k * BPS + 1;
            let any = (0..3).any(|i| {
                let c = base + self.hoff[i].round() as usize;
                c + 1 < NBINS && self.lin[c - 1].max(self.lin[c]).max(self.lin[c + 1]) >= fund_min
            });
            if !any {
                self.nh[k] = 0;
                continue;
            }
            let mut pb = [0u16; HARMS];
            self.nh[k] = self.partial_bins(k, &mut pb) as u8;
            self.pb[k] = pb;
        }
        for k in 0..KEYS {
            self.sal0[k] = self.salience(&self.white, k);
        }
        self.res.copy_from_slice(&self.white);
        self.res_lin.copy_from_slice(&self.lin);
        self.explained.iter_mut().for_each(|e| *e = false);
        let mut first: Option<f32> = None;
        self.min_abs = tun!(ON_ABS).min(tun!(OFF_ABS));
        for _ in 0..MAX_POLY {
            let mut best: Option<(usize, f32)> = None;
            for k in 0..KEYS {
                if self.picked.contains(&k) {
                    continue; // salr[k] keeps its value from the pick
                }
                // the cancel only lowers partials and the salience is monotone in them, so sal0
                // bounds it: keys that cannot reach the lowest threshold or beat the best are skipped
                let bound = self.sal0[k];
                if bound < self.min_abs * self.wfrac[k] || best.is_some_and(|(_, bs)| bound <= bs) {
                    continue;
                }
                let s = self.salience(&self.res, k);
                self.salr[k] = s;
                let on = self.sounding(k);
                let (abs, rel) = if on { (tun!(OFF_ABS), tun!(OFF_REL)) } else { (tun!(ON_ABS), tun!(ON_REL)) };
                // the top keys have only one or two partials under the Nyquist limit
                let abs = abs * self.wfrac[k];
                if s < abs {
                    continue;
                }
                if let Some(f) = first {
                    if s < rel * f {
                        continue;
                    }
                }
                if !self.has_fundamental(k) {
                    continue;
                }
                // a key whose fundamental sits on a partial of a picked key (octave, twelfth, ...)
                let c1 = k * BPS + 1;
                let ghost = self.explained[c1 - 1] || self.explained[c1] || self.explained[c1 + 1];
                if ghost && (s < tun!(GHOST_KEEP) * self.sal0[k] || !self.own_partials(k)) {
                    continue;
                }
                if best.map_or(true, |(_, bs)| s > bs) {
                    best = Some((k, s));
                }
            }
            let Some((k, s)) = best else { break };
            if first.is_none() {
                first = Some(s);
            }
            self.picked.push(k);
            self.cancel(k);
        }
    }

    fn energy_at(&self, k: usize, f: i64) -> f32 {
        self.energy[k][f.rem_euclid(RING as i64) as usize]
    }

    fn centre_s(f: i64) -> f64 {
        (f * HOP as i64) as f64 / RATE as f64
    }

    /// The frame of the steepest short-window energy rise of key k in [from, to].
    fn onset_frame(&self, k: usize, from: i64, to: i64) -> i64 {
        let from = from.max(to - RING as i64 + 2).max(1);
        let mut best = (to.max(from), f32::MIN);
        for f in from..=to {
            // amplitude through a Hann window rises fastest when the attack crosses the window centre
            let r = self.energy_at(k, f) - self.energy_at(k, f - 1);
            if r > best.1 {
                best = (f, r);
            }
        }
        best.0
    }

    /// Rise of key k's gate energy at frame j over its minimum in the RESTRIKE_LOOK frames before.
    fn rise(&self, k: usize, j: i64) -> f32 {
        let g = |f: i64| self.genergy[k][f.rem_euclid(RING as i64) as usize];
        let mut lo = f32::MAX;
        for i in j - RESTRIKE_LOOK as i64..j {
            lo = lo.min(g(i));
        }
        g(j) / lo.max(1e-7)
    }

    /// Whether key k's gate energy rose by ONSET_RISE within the last ONSET_WIN frames, and rose at
    /// least as much as its neighbouring keys did (a new note a semitone away leaks into this key's
    /// bins and must not restart a key that is only ringing on).
    fn recent_rise(&self, k: usize, f: i64) -> bool {
        (f - ONSET_WIN as i64..=f).any(|j| {
            let r = self.rise(k, j);
            if r <= tun!(ONSET_RISE) {
                return false;
            }
            let nb = |kk: usize| self.rise(kk, j);
            let left = if k > 0 { nb(k - 1) } else { 0.0 };
            let right = if k + 1 < KEYS { nb(k + 1) } else { 0.0 };
            r >= tun!(NEIGH_RISE) * left.max(right)
        })
    }

    /// A key a harmonic interval above a sounding key that was not picked this frame (a re-strike
    /// transient or a dip can drop it for a frame): its partials are that key's, unaccounted for, so
    /// it must not start a note now. In frames where the lower key is picked, the cancel and the
    /// ghost rule have already judged it.
    fn orphan_harmonic(&self, k: usize) -> bool {
        HARMONIC_STEPS.iter().any(|&d| {
            k >= d && self.sounding(k - d) && !self.picked.contains(&(k - d))
        })
    }

    fn velocity(&mut self, k: usize, from: i64, to: i64) -> u8 {
        let mut pk = 0f32;
        for f in from.max(to - RING as i64 + 1)..=to {
            pk = pk.max(self.energy_at(k, f));
        }
        let db = 20.0 * pk.max(1e-7).log10();
        let agc = *self.agc_db.get_or_insert(db);
        // per-note AGC: about the last 10 notes set the reference, so the volume knob doesn't move everything
        self.agc_db = Some(agc + (db - agc) * 0.1);
        (90.0 + 1.5 * (db - agc)).round().clamp(1.0, 127.0) as u8
    }

    fn track(&mut self, out: &mut Vec<Ev>) {
        self.pick();
        let f = self.frame;
        let min_frames = (MIN_NOTE_S * RATE as f64 / HOP as f64).ceil() as i64;
        let retrig = (RETRIG_S * RATE as f64 / HOP as f64).ceil() as i64;
        for k in 0..KEYS {
            let active = self.picked.contains(&k);
            let pitch = LOW_KEY + k as u8;
            match self.state[k] {
                KeyState::Idle => {
                    // a note starts only at an attack of its own: a key that drifts into the pick
                    // list in the middle of other notes (their partials decaying unevenly) does not
                    if active && self.recent_rise(k, f) && !self.orphan_harmonic(k) {
                        self.state[k] = KeyState::Pending { act: f };
                        self.quiet[k] = 0;
                    }
                }
                KeyState::Pending { act } => {
                    if !active {
                        self.quiet[k] += 1;
                        if self.quiet[k] >= OFF_FRAMES {
                            self.state[k] = KeyState::Idle;
                        }
                        continue;
                    }
                    self.quiet[k] = 0;
                    if f - act + 1 >= min_frames {
                        // never reach back past the previous note's attack on this key
                        let o = self.onset_frame(k, (act - ONSET_WIN as i64 - 4).max(self.last_trig[k] + 1), f);
                        let t0 = Self::centre_s(o);
                        let vel = self.velocity(k, o, f);
                        out.push(Ev::On { t: t0, pitch, vel });
                        self.state[k] = KeyState::On { t0, since: o };
                        self.last_trig[k] = o;
                    }
                }
                KeyState::On { t0, since } => {
                    if !active {
                        self.quiet[k] += 1;
                        if self.quiet[k] >= OFF_FRAMES {
                            let t = Self::centre_s(f - self.quiet[k] as i64 + 1).max(t0 + MIN_NOTE_S);
                            out.push(Ev::Off { t, pitch });
                            self.state[k] = KeyState::Idle;
                        }
                        continue;
                    }
                    self.quiet[k] = 0;
                    // a re-strike: the short-window energy jumps over its level since the last
                    // attack settled (half a window after that onset)
                    let settled = self.last_trig[k] + self.ehalf[k] as i64;
                    if f - self.last_trig[k] >= retrig && f - settled >= 2 {
                        let mut lo = f32::MAX;
                        let mut hlo = [f32::MAX; 4];
                        for j in (f - RESTRIKE_LOOK as i64).max(settled)..f {
                            lo = lo.min(self.energy_at(k, j));
                            let he = &self.henergy[k][j.rem_euclid(RING as i64) as usize];
                            for h in 0..4 {
                                hlo[h] = hlo[h].min(he[h]);
                            }
                        }
                        let e = self.energy_at(k, f);
                        // a hammer lifts all the partials at once and shows in the global flux; a
                        // neighbouring chord tone beating inside the wide fast bins lifts one or two
                        // partials and leaves the global flux flat
                        let he = self.henergy[k][f.rem_euclid(RING as i64) as usize];
                        let lifted = (0..4).filter(|&h| he[h] > RESTRIKE_PART * hlo[h].max(1e-6)).count();
                        let global = (0..3).any(|j| self.gflux[(f - j).rem_euclid(RING as i64) as usize] >= tun!(GF_ONSET));
                        if e > tun!(RESTRIKE) * lo.max(1e-6) && lifted >= 3 && global {
                            let t = Self::centre_s(f).max(t0 + MIN_NOTE_S);
                            out.push(Ev::Off { t, pitch });
                            self.state[k] = KeyState::Pending { act: f };
                            self.last_trig[k] = f;
                        }
                    }
                    let _ = since;
                }
            }
        }
    }
}

/// Pairs On/Off events into notes (sorted by start).
pub fn events_to_notes(evs: &[Ev]) -> Vec<Note> {
    let mut open: [Option<(f64, u8)>; 128] = [None; 128];
    let mut out = Vec::new();
    for e in evs {
        match *e {
            Ev::On { t, pitch, vel } => {
                if let Some((t0, v0)) = open[pitch as usize].take() {
                    out.push(Note { t: t0, dur: (t - t0).max(0.0), pitch, vel: v0 });
                }
                open[pitch as usize] = Some((t, vel));
            }
            Ev::Off { t, pitch } => {
                if let Some((t0, v0)) = open[pitch as usize].take() {
                    out.push(Note { t: t0, dur: (t - t0).max(0.0), pitch, vel: v0 });
                }
            }
        }
    }
    out.sort_by(|a, b| a.t.total_cmp(&b.t).then(a.pitch.cmp(&b.pitch)));
    out
}

/// Streaming 2:1 decimator (63-tap windowed-sinc low-pass at 0.45 of the output rate), for the
/// 44.1 kHz tap. Keeps a linear history buffer and computes only the kept (even) outputs.
pub struct Decim2 {
    taps: Vec<f32>,
    buf: Vec<f32>,
    /// Absolute input index of buf[0], and of the next input sample.
    base: u64,
    n_in: u64,
}

impl Default for Decim2 {
    fn default() -> Self {
        Self::new()
    }
}

impl Decim2 {
    const N: usize = 63;

    pub fn new() -> Decim2 {
        let n = Self::N;
        let fc = 0.45 / 2.0; // cycles per input sample
        let mid = (n - 1) as f64 / 2.0;
        let mut taps: Vec<f32> = (0..n)
            .map(|i| {
                let d = i as f64 - mid;
                let sinc = if d == 0.0 { 2.0 * fc } else { (2.0 * std::f64::consts::PI * fc * d).sin() / (std::f64::consts::PI * d) };
                let w = 0.42 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / (n - 1) as f64).cos() + 0.08 * (4.0 * std::f64::consts::PI * i as f64 / (n - 1) as f64).cos();
                (sinc * w) as f32
            })
            .collect();
        let s: f32 = taps.iter().sum();
        for t in taps.iter_mut() {
            *t /= s;
        }
        // N-1 zeros of history, so the first output sees silence before the stream
        Decim2 { taps, buf: vec![0.0; n - 1], base: 0, n_in: 0 }
    }

    /// Group delay in output samples.
    pub const DELAY_OUT: usize = 16;

    pub fn push(&mut self, x: &[f32], out: &mut Vec<f32>) {
        let n = Self::N;
        self.buf.extend_from_slice(x);
        // output j uses inputs 2j-(N-1) ..= 2j; in buf coordinates input i sits at i - base + N - 1
        let first = self.n_in.div_ceil(2);
        self.n_in += x.len() as u64;
        let last = (self.n_in + 1) / 2; // exclusive
        for j in first..last {
            let end = (2 * j - self.base) as usize + n; // exclusive, buf coordinates
            let w = &self.buf[end - n..end];
            out.push(w.iter().zip(&self.taps).map(|(a, b)| a * b).sum());
        }
        // keep the last N-1 samples before the next even input
        let next_even = last * 2;
        let keep_from = (next_even - self.base) as usize; // buf index of next_even - (N - 1)
        if keep_from > 4096 {
            self.buf.drain(..keep_from);
            self.base += keep_from as u64;
        }
    }
}

/// Interleaved stereo at 44.1 kHz -> mono at RATE, streaming.
pub struct Downmix {
    d: Decim2,
    mono: Vec<f32>,
}

impl Default for Downmix {
    fn default() -> Self {
        Self::new()
    }
}

impl Downmix {
    pub fn new() -> Downmix {
        Downmix { d: Decim2::new(), mono: Vec::new() }
    }
    pub fn push(&mut self, stereo: &[f32]) -> Vec<f32> {
        self.mono.clear();
        self.mono.extend(stereo.chunks_exact(2).map(|c| 0.5 * (c[0] + c[1])));
        let mut out = Vec::with_capacity(self.mono.len() / 2 + 1);
        let m = std::mem::take(&mut self.mono);
        self.d.push(&m, &mut out);
        self.mono = m;
        out
    }
}

/// Mono PCM at any rate -> mono at RATE.
pub fn to_rate(mono: &[f32], rate: u32) -> Vec<f32> {
    if rate == RATE {
        return mono.to_vec();
    }
    if rate == 2 * RATE {
        let mut d = Decim2::new();
        let mut out = Vec::with_capacity(mono.len() / 2 + 1);
        d.push(mono, &mut out);
        d.push(&vec![0.0; 2 * Decim2::DELAY_OUT], &mut out);
        return out[Decim2::DELAY_OUT..].to_vec();
    }
    let st: Vec<f32> = mono.iter().flat_map(|&v| [v, v]).collect();
    crate::dsp::resample(&st, rate, RATE).iter().step_by(2).copied().collect()
}

/// Offline: mono PCM at `rate` -> piano notes.
pub fn transcribe(mono: &[f32], rate: u32) -> Vec<Note> {
    let x = to_rate(mono, rate);
    let mut tr = PianoTracker::new();
    let mut ev = tr.push(&x);
    ev.extend(tr.flush());
    events_to_notes(&ev)
}

fn lcg_unit(s: &mut u64) -> f32 {
    *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    ((*s >> 33) as f32) / (1u64 << 31) as f32
}

/// Piano-like tone sum at RATE: partials up to 0.45 * RATE with stretch sqrt(1 + B h^2), 1/h^0.9 roll-off
/// with a fixed per-partial wobble, a weak fundamental in the bottom octave, exponential decay (faster
/// for high keys and high partials), 3 ms attack and a 30 ms release tail.
pub fn synth_piano(notes: &[Note], secs: f64, seed: u64) -> Vec<f32> {
    let len = (secs * RATE as f64) as usize;
    let mut out = vec![0f32; len];
    let mut s = seed;
    for nt in notes {
        let p = nt.pitch as f64;
        let f0 = 440.0 * 2f64.powf((p - 69.0) / 12.0);
        let b = 2e-4 * 2f64.powf((p - 60.0) / 24.0);
        let tau = (1.6 * 2f64.powf(-(p - 60.0) / 24.0)).clamp(0.25, 4.0);
        let amp = 0.25 * (nt.vel as f64 / 127.0).powi(2);
        let i0 = (nt.t * RATE as f64) as usize;
        let rel_at = nt.dur;
        let end = ((nt.t + nt.dur + 0.15) * RATE as f64) as usize;
        for h in 1..60usize {
            let hf = h as f64;
            let fh = hf * f0 * (1.0 + b * hf * hf).sqrt();
            if fh > 0.45 * RATE as f64 {
                break;
            }
            let mut a = amp / hf.powf(0.9) * (1.0 + 0.3 * (hf * 1.7 + p).sin());
            if h == 1 && p < 33.0 {
                a *= 0.4;
            }
            let tau_h = tau / (1.0 + 0.12 * (hf - 1.0));
            let ph = lcg_unit(&mut s) as f64 * std::f64::consts::TAU;
            let w = std::f64::consts::TAU * fh / RATE as f64;
            for i in i0..end.min(len) {
                let t = (i - i0) as f64 / RATE as f64;
                let att = (t / 0.003).min(1.0);
                let rel = if t > rel_at { (-(t - rel_at) / 0.03).exp() } else { 1.0 };
                let env = att * rel * (-t / tau_h).exp();
                out[i] += (a * env * (w * (i - i0) as f64 + ph).sin()) as f32;
            }
        }
    }
    out
}

/// Reads a RIFF WAV (PCM 16/24/32-bit or float 32/64, any channel count) as mono f32 and its rate.
pub fn read_wav(bytes: &[u8]) -> Result<(Vec<f32>, u32), String> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a WAV file".into());
    }
    let (mut fmt, mut data): (Option<(u16, u16, u32, u16)>, Option<&[u8]>) = (None, None);
    let mut i = 12;
    while i + 8 <= bytes.len() {
        let id = &bytes[i..i + 4];
        let len = u32::from_le_bytes(bytes[i + 4..i + 8].try_into().unwrap()) as usize;
        let body = &bytes[i + 8..(i + 8 + len).min(bytes.len())];
        if id == b"fmt " && body.len() >= 16 {
            let mut tag = u16::from_le_bytes([body[0], body[1]]);
            if tag == 0xfffe && body.len() >= 26 {
                tag = u16::from_le_bytes([body[24], body[25]]);
            }
            let ch = u16::from_le_bytes([body[2], body[3]]);
            let rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
            let bits = u16::from_le_bytes([body[14], body[15]]);
            fmt = Some((tag, ch, rate, bits));
        } else if id == b"data" {
            data = Some(body);
        }
        i += 8 + len + (len & 1);
    }
    let (tag, ch, rate, bits) = fmt.ok_or("no fmt chunk")?;
    let data = data.ok_or("no data chunk")?;
    if ch == 0 || rate == 0 {
        return Err("bad fmt chunk".into());
    }
    let bps = (bits as usize).div_ceil(8);
    let sample = |b: &[u8]| -> Option<f32> {
        Some(match (tag, bits) {
            (1, 16) => i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0,
            (1, 24) => (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8388608.0,
            (1, 32) => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2147483648.0,
            (1, 8) => (b[0] as f32 - 128.0) / 128.0,
            (3, 32) => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            (3, 64) => f64::from_le_bytes(b[..8].try_into().ok()?) as f32,
            _ => return None,
        })
    };
    let frame = bps * ch as usize;
    let mut out = Vec::with_capacity(data.len() / frame.max(1));
    for fr in data.chunks_exact(frame) {
        let mut acc = 0f32;
        for c in fr.chunks_exact(bps) {
            acc += sample(c).ok_or_else(|| format!("unsupported WAV format {tag}/{bits}-bit"))?;
        }
        out.push(acc / ch as f32);
    }
    Ok((out, rate))
}

/// mir_eval-style note scores: onset within `tol` seconds, exact pitch, offsets ignored.
/// Each reference note matches at most one estimate (closest onset first). Returns (precision, recall, f1).
pub fn note_f1(est: &[Note], gt: &[Note], tol: f64) -> (f64, f64, f64) {
    let mut pairs: Vec<(f64, usize, usize)> = Vec::new();
    for (i, g) in gt.iter().enumerate() {
        for (j, e) in est.iter().enumerate() {
            let d = (e.t - g.t).abs();
            if e.pitch == g.pitch && d <= tol {
                pairs.push((d, i, j));
            }
        }
    }
    pairs.sort_by(|a, b| a.0.total_cmp(&b.0));
    let (mut ug, mut ue) = (vec![false; gt.len()], vec![false; est.len()]);
    let mut hit = 0usize;
    for (_, i, j) in pairs {
        if !ug[i] && !ue[j] {
            ug[i] = true;
            ue[j] = true;
            hit += 1;
        }
    }
    let p = if est.is_empty() { if gt.is_empty() { 1.0 } else { 0.0 } } else { hit as f64 / est.len() as f64 };
    let r = if gt.is_empty() { 1.0 } else { hit as f64 / gt.len() as f64 };
    let f = if p + r > 0.0 { 2.0 * p * r / (p + r) } else { 0.0 };
    (p, r, f)
}

#[cfg(test)]
#[path = "piano_tests.rs"]
mod tests;
