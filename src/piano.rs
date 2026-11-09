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
/// The log axis runs up to MIDI 121 (~8.9 kHz), past C8: high keys keep part of their 2nd partial,
/// and the band just under the 11 kHz Nyquist (resampler transition, aliases) stays out.
const TOP_MIDI: usize = 121;
const NBINS: usize = (TOP_MIDI - LOW_KEY as usize) * BPS;
const SIZES: [usize; 3] = [4096, 2048, 1024];
/// Centre delay: a frame centred on sample c needs input up to c + 2048, plus the decimators' 24.
pub const DELAY: usize = 2048 + 32;
/// Partials summed into the salience, and their weight decay.
const HARMS: usize = 20;
const HARM_DECAY: f32 = 0.95;
/// Partials subtracted on a cancel.
const CANCEL_HARMS: usize = 48;
/// Stretch model for the cancel: partial h sits a*h^2 bins above h*f0. `a` is fitted per note with
/// this prior weight (as if partials up to ~10 had shown no stretch) and capped (B ~ 4e-3).
const STRETCH_PRIOR: f32 = 1e4;
const STRETCH_MAX: f32 = 0.1;
/// Cancel envelope: partial h of a picked note is subtracted at most up to ENV_K * ref * h^-ENV_BETA,
/// ref from its first two partials (the second scaled by 2^BETA * ENV_H2).
const ENV_BETA: f32 = 1.0;
const ENV_K: f32 = 1.5;
const ENV_H2: f32 = 0.7;
/// The cap is at least the strongest of the 2 partials either side, times ENV_NEIGH (0 = off).
const ENV_NEIGH: f32 = 1.0;
const ENV_NEIGH_FROM: f32 = 55.0;
pub const MAX_POLY: usize = 10;
/// Whitening: the 20th percentile of ±FLOOR_HALF FFT bins, times 1.76 (the Rayleigh median / p20
/// ratio, so noise sits at 1), evaluated every FLOOR_STEP bins; plus a floor 50 dB under the frame
/// peak and an absolute floor.
const FLOOR_HALF: usize = 16;
const FLOOR_STEP: usize = 8;
const FLOOR_PCT: f32 = 0.2;
const FLOOR_SCALE: f32 = 1.76;
const DYN_RANGE: f32 = 3e-3;
const ABS_FLOOR: f32 = 1e-6;
/// DC blocker pole at RATE (~16 Hz corner).
const DC_POLE: f32 = 0.9955;
/// Salience thresholds (whitened, compressed units). A new key needs ON, a sounding one keeps going
/// down to OFF. REL is relative to the strongest key of the frame.
const ON_ABS: f32 = 7.0;
const OFF_ABS: f32 = 4.5;
/// Keys with fewer partials than this under the axis top get a proportionally lower ABS threshold.
const WFRAC_REF: usize = 6;
/// Floor of the unclaimed-weight scaling of ABS (1 = off).
const UNCLAIMED_MIN: f32 = 0.6;
const ON_REL: f32 = 0.3;
const OFF_REL: f32 = 0.12;
/// An octave/twelfth/double-octave of a picked key must keep this share of its salience after the cancel.
const GHOST_KEEP: f32 = 0.55;
/// ...and at least 3 of its first 4 partials must keep this share of their linear energy.
const GHOST_PART: f32 = 0.3;
/// A key's fundamental must be a residual peak this far over the floor; below this MIDI key any of its first three partials will do.
const FUND_MIN: f32 = 5.0;
const WEAK_FUND_BELOW: usize = 40;
/// Tracker timing.
pub const MIN_NOTE_S: f64 = 0.060;
const OFF_FRAMES: u32 = 2;
/// Extra quiet frames for keys judged on the 4096 window.
const OFF_EXTRA_LOW: f32 = 2.0;
const OFF_EXTRA_MID: f32 = 0.0;
const RETRIG_S: f64 = 0.050;
/// A held key re-strikes when its short-window energy jumps by this factor over its recent minimum.
const RESTRIKE: f32 = 1.4;
/// ...or this steep a rise within 4 frames, with no flux onset needed.
const RESTRIKE_FAST: f32 = 2.0;
/// Frames a strong flux onset stays latched for the slower energy rise to follow it.
const RESTRIKE_LATCH: usize = 8;
/// ...and come out of a dip to this share of the note's peak so far.
const RESTRIKE_DIP: f32 = 0.8;
/// A key that ended within RESTART_WIN frames restarts only after its fast energy dipped to this
/// share of the ended note's peak.
const RESTART_WIN: i64 = 16;
const RESTART_DIP: f32 = 0.6;
/// A new key under this share of the salience of a picked semitone neighbour does not start.
const SEMI_K: f32 = 0.7;
/// Half the fast onset-energy window (1024), in frames.
const EHALF: i64 = (SIZES[2] / 2 / HOP) as i64;
const RESTRIKE_PART: f32 = 1.4;
/// Global flux: bins of the 1024 FFT used, and the onset level relative to its running mean.
const GF_BINS: usize = 256;
const GF_ONSET: f32 = 4.0;
/// Floor of the flux baseline (sum of log-rises over the bins), the flux range under the frame peak,
/// and the per-bin dead band (log units per frame).
const GF_FLOOR: f32 = 4.0;
const GF_RANGE: f32 = 1e-2;
const GF_DEAD: f32 = 0.1;
/// A new note needs its short-window energy to have risen this much within the last ONSET_WIN frames.
const ONSET_RISE: f32 = 1.5;
const ONSET_WIN: usize = 12;
/// ...and at least this share of the larger rise of its two neighbouring keys.
const NEIGH_RISE: f32 = 0.7;
/// The new-note rise is measured over this many frames (a steep attack, not a slow creep).
const RISE_LOOK: f32 = 6.0;
/// A new note must reach the loudest key's level minus NEW_LVL_GLOBAL dB and its own key's recent
/// peak minus NEW_LVL_KEY dB (both references decay at 3 dB/s).
const NEW_LVL_GLOBAL: f32 = 30.0;
const NEW_LVL_KEY: f32 = 20.0;
const LEVEL_DECAY: f32 = 0.995_99; // 3 dB/s at 86.13 frames/s
/// Flux a new note next to a sounding key needs near its rise (0 = off).
const GF_NEAR: f32 = 0.0;
/// Re-strike: vetoed when the key's pitch spread over 8 frames exceeds RS_STAB semitones; the fast
/// path needs a flux of FAST_FLUX within 4 frames.
const RS_STAB: f32 = 0.2;
const FAST_FLUX: f32 = 99.0;
const RS_STAB_FROM: usize = 50;
/// A re-strike must reach the note's own peak minus RS_LVL dB.
const RS_LVL: f32 = 24.0;
/// Onset: walk back from the steepest rise while the energy stays above this share of the level reached.
const ONSET_FRAC: f32 = 0.2;
/// Starting keys count as re-strike rivals (1 = on).
const RIVAL_NEW: f32 = 1.0;
/// A re-strike must lift the key's own 1st or 3rd gate partial by this factor (0 = off).
const OWN_LIFT: f32 = 1.25;
/// Partial-balance change that counts as a strike on the fast re-strike path.
const SHAPE_MIN: f32 = 0.03;
const SHAPE_MAX: f32 = 0.55;
/// Pitch is only measured on peaks this far over the floor (a faint peak's position is noise).
const PITCH_MIN: f32 = 8.0;
/// Intervals at which a starting key shares partials with a held key below it.
const SHARED_STEPS: [usize; 8] = [7, 12, 19, 24, 28, 31, 34, 36];
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
    /// `t` is the note start in seconds since the tracker started, on the 11.6 ms frame grid (the
    /// centre of the frame with the steepest attack), not sample-accurate.
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

/// The linear factor of `db` decibels down.
fn db_down(db: f32) -> f32 {
    10f32.powf(-db / 20.0)
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
        Rfft { m, rev, tw, post, win, scale, re: vec![0.0; m], im: vec![0.0; m], mag: vec![0.0; m + 1] }
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

/// Streaming 2:1 decimator for the long analysis windows: a 17-tap Kaiser low-pass (cut at a
/// quarter of the output rate, far above the band each long window serves), output m = sum over
/// h[i] * x[2m - i], so it has a delay of 8 input samples.
struct Dec {
    h: [f32; DEC_TAPS],
    /// Output samples from absolute index `base`.
    out: Vec<f32>,
    base: i64,
    next: i64,
}

const DEC_TAPS: usize = 17;

impl Dec {
    fn new() -> Dec {
        let mid = (DEC_TAPS - 1) as f64 / 2.0;
        let fc = 0.125; // cycles per input sample: a quarter of the output rate
        let mut h = [0f32; DEC_TAPS];
        for (i, v) in h.iter_mut().enumerate() {
            let d = i as f64 - mid;
            let sinc = if d == 0.0 { 2.0 * fc } else { (2.0 * std::f64::consts::PI * fc * d).sin() / (std::f64::consts::PI * d) };
            *v = (sinc * kaiser(d / mid, 6.0)) as f32;
        }
        let sum: f32 = h.iter().sum();
        h.iter_mut().for_each(|v| *v /= sum);
        Dec { h, out: Vec::new(), base: 0, next: 0 }
    }

    /// Computes every output whose newest input is available. `src` holds input samples from
    /// absolute index `src_base` to `src_end` (exclusive); inputs before 0 are silence.
    fn feed(&mut self, src: &[f32], src_base: i64) {
        let src_end = src_base + src.len() as i64;
        while 2 * self.next < src_end {
            let top = 2 * self.next;
            let mut acc = 0f32;
            for (i, &h) in self.h.iter().enumerate() {
                let j = top - i as i64;
                if j >= src_base {
                    acc += h * src[(j - src_base) as usize];
                }
            }
            self.out.push(acc);
            self.next += 1;
        }
    }

    /// Copies out[from .. from + dst.len()] (absolute indices), silence where it doesn't exist.
    fn window(&self, from: i64, dst: &mut [f32]) {
        for (i, v) in dst.iter_mut().enumerate() {
            let j = from + i as i64 - self.base;
            *v = if j >= 0 && (j as usize) < self.out.len() { self.out[j as usize] } else { 0.0 };
        }
    }

    /// Forgets outputs before absolute index `keep_from`.
    fn trim(&mut self, keep_from: i64) {
        let k = keep_from - self.base;
        if k > 2048 {
            self.out.drain(..k as usize);
            self.base += k;
        }
    }
}

/// The FFT length actually run for each analysis size: the 4096 and 2048 windows run as 1024-point
/// FFTs on the input decimated by 4 and 2 (same duration and bin spacing, a quarter and a half of the
/// work), since they only serve the log axis below C6.
const FFT_N: usize = 1024;

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
    let k0 = (lo.round() as usize).min(FFT_N / 2);
    let k1 = (hi.round() as usize).clamp(k0, FFT_N / 2);
    Kern { size: size as u8, k0: k0 as u16, k1: k1 as u16 }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum KeyState {
    Idle,
    /// Active, not yet long enough to emit. `act` is the first active frame.
    Pending { act: i64, floor: i64, proven: bool },
    /// Emitted. `t0` is its start in seconds.
    On { t0: f64 },
}

/// Streaming song -> piano note tracker. Feed mono PCM at `RATE`.
pub struct PianoTracker {
    ffts: [Rfft; 3],
    /// Per key, per partial 1..=4: the short-window FFT bin range for the onset energy.
    ekern: Vec<[Kern; 4]>,
    /// The same at the resolution of the start gate, and its ring.
    gkern: Vec<[Kern; 4]>,
    genergy: Vec<[f32; RING]>,
    /// Per key, per frame, per partial 1..=4: the gate-window energy.
    hgenergy: Vec<[[f32; 4]; RING]>,
    /// Per key, per frame, per partial 1..=4: the fast-window energy.
    henergy: Vec<[[f32; 4]; RING]>,
    /// Global flux ring (relative to its running mean), the previous log spectrum, the mean.
    gflux: [f32; RING],
    gf_prev: Vec<f32>,
    /// Raw flux of the last RING frames (the baseline).
    gf_raw: [f32; RING],
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
    /// The noise floor the peak in each bin was divided by (amplitude = lin * fl).
    fl: Vec<f32>,
    explained: Vec<bool>,
    /// Per key: tracked partial bins for this frame, and how many.
    pb: Vec<[u16; HARMS]>,
    nh: Vec<u8>,
    /// Per key: the share of the full harmonic weight its partials under the axis top can reach (<= 1).
    wfrac: Vec<f32>,
    min_abs: f32,
    /// Window scratch for the FFT input.
    xbuf: Vec<f32>,
    /// The input decimated by 2 (for the 2048 window) and by 4 (for the 4096 window).
    dec2: Dec,
    dec4: Dec,
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
    /// Per key: the frame its last note ended (a new note's onset can't reach back past it).
    ended: Vec<i64>,
    quiet: Vec<u32>,
    /// Per key: peak fast energy of the sounding note so far.
    peak_e: Vec<f32>,
    /// Per key, per frame: pitch of its strongest low partial relative to the key, in semitones (NaN
    /// when it has no peak there): the pitch-stability evidence.
    ppos: Vec<[f32; RING]>,
    /// Per key, per frame: its gate-energy peak, decaying 3 dB/s (the level a new note must reach).
    kpk: Vec<[f32; RING]>,
    /// Per frame: the loudest key's gate energy, decaying the same way.
    gref: [f32; RING],
    last_trig: Vec<i64>,
    agc_db: Option<f32>,
    dc_x: f32,
    dc_y: f32,
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
            ffts: [Rfft::new(FFT_N), Rfft::new(FFT_N), Rfft::new(FFT_N)],
            dec2: Dec::new(),
            dec4: Dec::new(),
            ekern,
            gkern,
            genergy: vec![[0.0; RING]; KEYS],
            hgenergy: vec![[[0.0; 4]; RING]; KEYS],
            henergy: vec![[[0.0; 4]; RING]; KEYS],
            gflux: [0.0; RING],
            gf_prev: vec![0.0; GF_BINS],
            gf_raw: [0.0; RING],
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
                ((lo.floor() as usize).max(1), (hi.ceil() as usize + 2).min(FFT_N / 2))
            }),
            lin: vec![0.0; NBINS],
            pos: vec![0.0; NBINS],
            fl: vec![0.0; NBINS],
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
            ended: vec![i64::MIN / 2; KEYS],
            quiet: vec![0; KEYS],
            peak_e: vec![0.0; KEYS],
            ppos: vec![[f32::NAN; RING]; KEYS],
            kpk: vec![[0.0; RING]; KEYS],
            gref: [0.0; RING],
            last_trig: vec![i64::MIN / 2; KEYS],
            agc_db: None,
            dc_x: 0.0,
            dc_y: 0.0,
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
        // DC blocker (one-pole high-pass at ~16 Hz): an offset or step can't seed the flux baseline
        // or leak into the bottom bins
        self.buf.reserve(pcm.len());
        for &x in pcm {
            let x = if x.is_finite() { x } else { 0.0 };
            let y = x - self.dc_x + DC_POLE * self.dc_y;
            self.dc_x = x;
            self.dc_y = y;
            self.buf.push(y);
        }
        self.dec2.feed(&self.buf, self.buf_start as i64);
        self.dec4.feed(&self.dec2.out, self.dec2.base);
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
                self.dec2.trim(self.buf_start as i64 / 2 - 64);
                self.dec4.trim(self.buf_start as i64 / 4 - 64);
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
        // 4096 window: 1024 samples of the input decimated by 4, centred on c (decimator delays
        // compensated: 8 input samples for the first stage, 8 more stage-1 samples for the second)
        {
            let q = (c + 2 * 8 + 8) / 4;
            let mut x = std::mem::take(&mut self.xbuf);
            self.dec4.window(q - FFT_N as i64 / 2, &mut x[..FFT_N]);
            self.ffts[0].run(&x[..FFT_N]);
            let m = (c + 8) / 2;
            self.dec2.window(m - FFT_N as i64 / 2, &mut x[..FFT_N]);
            self.ffts[1].run(&x[..FFT_N]);
            self.xbuf = x;
        }
        {
            let n = FFT_N;
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
            self.ffts[2].run(x);
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
                    self.fl[bin] = self.floor[s][k];
                    self.pos[bin] = (midi - LOW_KEY as f32) * BPS as f32 + 1.0;
                }
            }
        }
        for b in 0..NBINS {
            self.white[b] = comp(self.lin[b]);
        }
        let slot = (self.frame.rem_euclid(RING as i64)) as usize;
        // global spectral flux on the 1024 window: log magnitudes compressed 60 dB under the frame
        // peak (level-invariant, and noise-level bins can't dominate), against the mean of the last
        // RING frames with a floor, so a steady tone's near-zero flux never makes a swell an onset
        {
            let mag = &self.ffts[2].mag;
            let pk = mag[1..GF_BINS].iter().fold(0f32, |a, &m| a.max(m));
            let eps = (pk * tun!(GF_RANGE)).max(1e-12);
            let dead = tun!(GF_DEAD);
            let mut fl = 0f32;
            for (i, &m) in mag[1..GF_BINS].iter().enumerate() {
                let l = (m + eps).ln() - eps.ln();
                // a dead band per bin: slow swells and window-phase wiggles add nothing
                fl += (l - self.gf_prev[i] - dead).max(0.0);
                self.gf_prev[i] = l;
            }
            let base = self.gf_raw.iter().sum::<f32>() / RING as f32;
            self.gflux[slot] = fl / base.max(tun!(GF_FLOOR));
            self.gf_raw[slot] = fl;
        }
        for k in 0..KEYS {
            let mut tot = 0f32;
            for (h, kn) in self.ekern[k].iter().enumerate() {
                let v = self.ffts[kn.size as usize].mag[kn.k0 as usize..=kn.k1 as usize].iter().fold(0f32, |a, &m| a.max(m));
                self.henergy[k][slot][h] = v;
                tot += v;
            }
            self.energy[k][slot] = tot;
            let mut gt = 0f32;
            for (h, kn) in self.gkern[k].iter().enumerate() {
                let v = self.ffts[kn.size as usize].mag[kn.k0 as usize..=kn.k1 as usize].iter().fold(0f32, |a, &m| a.max(m));
                self.hgenergy[k][slot][h] = v;
                gt += v;
            }
            self.genergy[k][slot] = gt;
        }
        // decaying level references for the new-note gate
        let prev = (self.frame - 1).rem_euclid(RING as i64) as usize;
        let mut top = 0f32;
        for k in 0..KEYS {
            let g = self.genergy[k][slot];
            top = top.max(g);
            self.kpk[k][slot] = (self.kpk[k][prev] * LEVEL_DECAY).max(g);
        }
        self.gref[slot] = (self.gref[prev] * LEVEL_DECAY).max(top);
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
        // (bin of the residual peak, its amplitude): amplitudes, not ratios, so partials in different
        // FFT bands (different floors) compare
        let mut found: [(usize, f32); CANCEL_HARMS + 2] = [(0, 0.0); CANCEL_HARMS + 2];
        for h in 1..=n {
            let c = bins[h - 1] as usize;
            let mut bm = c;
            for b in c - 1..=c + 1 {
                if self.res_lin[b] > self.res_lin[bm] {
                    bm = b;
                }
            }
            found[h] = (bm, self.res_lin[bm] * self.fl[bm]);
        }
        let amp = |i: usize| found[i].1;
        // a smooth decaying envelope from the note's own lowest partials: a chord tone or melody note
        // sitting on a high partial of a bass note lifts that partial far above what the bass alone
        // would put there, and the excess survives the cancel
        let beta = tun!(ENV_BETA);
        let reference = if n >= 2 { amp(1).max(amp(2) * 2f32.powf(beta) * tun!(ENV_H2)) } else { amp(1) };
        for h in 1..=n {
            let smooth = if h == 1 {
                amp(1)
            } else if h < n {
                amp(h).min(0.5 * (amp(h - 1) + amp(h + 1)))
            } else {
                amp(h).min(amp(h - 1))
            };
            // the cap is the larger of the decaying envelope and the partial's neighbours: a smooth
            // bright series (brass, reeds, whose middle partials stand above the 1st) is cancelled in
            // full; a partial standing alone above its neighbours (a melody or chord tone on top of
            // this note) keeps its excess
            let mut cap = reference * tun!(ENV_K) * (h as f32).powf(-beta);
            if tun!(ENV_NEIGH) > 0.0 && k + LOW_KEY as usize >= tun!(ENV_NEIGH_FROM) as usize {
                let nb = [h.wrapping_sub(2), h.wrapping_sub(1), h + 1, h + 2]
                    .iter()
                    .filter(|&&i| i >= 1 && i <= n)
                    .map(|&i| amp(i))
                    .fold(0f32, f32::max);
                cap = cap.max(nb * tun!(ENV_NEIGH));
            }
            let sub = if h >= 2 { smooth.min(cap) } else { smooth };
            let c = found[h].0;
            if self.fl[c] > 0.0 {
                self.res_lin[c] = (self.res_lin[c] - sub / self.fl[c]).max(0.0);
                self.res[c] = comp(self.res_lin[c]);
            }
            for b in c - 1..=(c + 1).min(NBINS - 1) {
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

    /// The share of key k's harmonic weight on partials no earlier pick has claimed, floored at
    /// UNCLAIMED_MIN: a key whose partials were partly taken is judged on the evidence left to it
    /// rather than against the full absolute threshold.
    fn unclaimed(&self, k: usize) -> f32 {
        let lo = tun!(UNCLAIMED_MIN);
        if lo >= 1.0 || self.picked.is_empty() {
            return 1.0;
        }
        let (mut all, mut free, mut w) = (0f32, 0f32, 1f32);
        let decay = tun!(HARM_DECAY);
        for h in 0..self.nh[k] as usize {
            let c = self.pb[k][h] as usize;
            all += w;
            if !self.explained[c] {
                free += w;
            }
            w *= decay;
        }
        if all <= 0.0 {
            1.0
        } else {
            (free / all).max(lo)
        }
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
                let abs = abs * self.wfrac[k] * self.unclaimed(k);
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
        // a slow attack (bowed, blown) rises over many frames and its steepest point comes late: walk
        // back to where the rise began, the first frame at ONSET_FRAC of the level reached
        let frac = tun!(ONSET_FRAC);
        if frac > 0.0 {
            let top = (best.0..=to).map(|j| self.energy_at(k, j)).fold(0f32, f32::max);
            let mut j = best.0;
            while j > from && self.energy_at(k, j - 1) >= frac * top && self.energy_at(k, j - 1) < self.energy_at(k, j) {
                j -= 1;
            }
            return j;
        }
        best.0
    }

    /// Rise of key k's gate energy at frame j over its minimum in the RISE_LOOK frames before.
    fn rise(&self, k: usize, j: i64) -> f32 {
        let g = |f: i64| self.genergy[k][f.rem_euclid(RING as i64) as usize];
        let mut lo = f32::MAX;
        for i in j - tun!(RISE_LOOK) as i64..j {
            lo = lo.min(g(i));
        }
        g(j) / lo.max(1e-12)
    }

    /// Whether key k's gate energy rose by ONSET_RISE within the last ONSET_WIN frames, and rose at
    /// least as much as its neighbouring keys did (a new note a semitone away leaks into this key's
    /// bins and must not restart a key that is only ringing on).
    fn recent_rise(&self, k: usize, f: i64) -> bool {
        let ring = |j: i64| j.rem_euclid(RING as i64) as usize;
        (f - ONSET_WIN as i64..=f).any(|j| {
            let r = self.rise(k, j);
            if r <= tun!(ONSET_RISE) {
                return false;
            }
            let nb = |kk: usize| self.rise(kk, j);
            let left = if k > 0 { nb(k - 1) } else { 0.0 };
            let right = if k + 1 < KEYS { nb(k + 1) } else { 0.0 };
            if r < tun!(NEIGH_RISE) * left.max(right) {
                return false;
            }
            // level: the rise must reach a level near the loudest key of the moment and near this
            // key's own recent peak (taken before the rise began), so a reverb tail 30+ dB down whose
            // level beats in the comb filters is not a new note
            let e = self.genergy[k][ring(j)];
            let before = self.kpk[k][ring(j - tun!(RISE_LOOK) as i64 - 1)];
            if e < self.gref[ring(j)] * db_down(tun!(NEW_LVL_GLOBAL)) || e < before * db_down(tun!(NEW_LVL_KEY)) {
                return false;
            }
            // next to a sounding key (within 2 semitones of it, its octave or its twelfth), where a
            // vibrato or glide sweeps that key's partials through its neighbours, a new note also
            // needs an attack: a broadband flux onset within 2 frames of its rise
            let near = (0..KEYS).any(|q| q != k && self.sounding(q) && [0i64, 12, 19].iter().any(|&d| (k as i64 - q as i64 - d).abs() <= 2));
            !near || (j - 2..=(j + 2).min(f)).any(|i| self.gflux[ring(i)] >= tun!(GF_NEAR))
        })
    }

    /// Spread (trimmed max - min, semitones) of key k's measured pitch over the last n frames up to f,
    /// if it was measured in at least 3 of them.
    fn pitch_spread(&self, k: usize, f: i64, n: i64) -> Option<f32> {
        let mut v: Vec<f32> = (f - n + 1..=f).map(|j| self.ppos[k][j.rem_euclid(RING as i64) as usize]).filter(|v| v.is_finite()).collect();
        if v.len() < 3 {
            return None;
        }
        v.sort_by(|a, b| a.total_cmp(b));
        // robust spread: 4x the median absolute deviation (a few transient frames at a strike don't
        // count; a vibrato moves the pitch most of the time). For a sine of amplitude A it reads
        // about 2.8 A, for a steady tone with rare outliers near 0
        let med = v[v.len() / 2];
        let mut d: Vec<f32> = v.iter().map(|x| (x - med).abs()).collect();
        d.sort_by(|a, b| a.total_cmp(b));
        Some(4.0 * d[d.len() / 2])
    }

    /// Records key k's pitch for this frame: the position of its strongest peak among partials 1..3,
    /// relative to where that partial belongs, in semitones.
    fn measure_pitch(&mut self, k: usize) {
        let slot = self.frame.rem_euclid(RING as i64) as usize;
        let mut best: Option<(f32, f32)> = None;
        for h in 0..(self.nh[k] as usize).min(3) {
            let c = self.pb[k][h] as usize;
            for b in c - 1..=c + 1 {
                if self.lin[b] >= tun!(PITCH_MIN) && best.is_none_or(|x| self.lin[b] > x.0) {
                    let ideal = (k * BPS + 1) as f32 + self.hoff[h];
                    best = Some((self.lin[b], (self.pos[b] - ideal) / BPS as f32));
                }
            }
        }
        self.ppos[k][slot] = best.map_or(f32::NAN, |b| b.1);
    }

    /// A key a harmonic interval above a sounding key that was not picked this frame (a re-strike
    /// transient or a dip can drop it for a frame): its partials are that key's, unaccounted for, so
    /// it must not start a note now. In frames where the lower key is picked, the cancel and the
    /// ghost rule have already judged it.
    fn orphan_harmonic(&self, k: usize) -> bool {
        HARMONIC_STEPS.iter().any(|&d| {
            k >= d
                && ((self.sounding(k - d) && !self.picked.contains(&(k - d)))
                    // or a held key being re-struck right now: its attack lifts all its partials
                    || matches!(self.state[k - d], KeyState::Pending { proven: true, .. }))
        })
    }

    /// Velocity from the peak fast energy in [from, to], against the AGC reference `agc` (the first
    /// notes of a stream are their own reference). Returns (velocity, the peak in dB).
    fn velocity(&self, k: usize, from: i64, to: i64, agc: Option<f32>) -> (u8, f32) {
        let mut pk = 0f32;
        for f in from.max(to - RING as i64 + 1)..=to {
            pk = pk.max(self.energy_at(k, f));
        }
        let db = 20.0 * pk.max(1e-12).log10();
        let reference = agc.unwrap_or(db);
        (((90.0 + 1.5 * (db - reference)).round().clamp(1.0, 127.0)) as u8, db)
    }

    /// Whether a held key k re-strikes at frame f (see the constants): all of its fast partials lift
    /// together, either steeply (a 4-frame rise of RESTRIKE_FAST, faster than any tremolo or swell),
    /// or by RESTRIKE within RESTRIKE_LATCH frames after a strong broadband flux onset (a hammer on a
    /// sampled or real piano, whose partials can rise slowly over the damper dip). Only frames after
    /// the last attack settled count.
    fn restrike(&self, k: usize, f: i64) -> Option<f32> {
        let settled = self.last_trig[k] + EHALF + 1;
        let e = self.energy_at(k, f);
        let he = self.henergy[k][f.rem_euclid(RING as i64) as usize];
        let rise = |look: i64| -> (f32, usize) {
            let mut lo = f32::MAX;
            let mut hlo = [f32::MAX; 4];
            let from = (f - look).max(settled);
            if from >= f {
                return (0.0, 0);
            }
            for j in from..f {
                lo = lo.min(self.energy_at(k, j));
                let hj = &self.henergy[k][j.rem_euclid(RING as i64) as usize];
                for h in 0..4 {
                    hlo[h] = hlo[h].min(hj[h]);
                }
            }
            // only partials that carry energy count (a near-silent partial wobbles by any ratio)
            let top = he.iter().fold(0f32, |a, &v| a.max(v));
            let lifted = (0..4).filter(|&h| he[h] >= 0.15 * top && he[h] > tun!(RESTRIKE_PART) * hlo[h].max(1e-12)).count();
            // the rise must come out of a dip: somewhere in the window the energy fell under DIP times
            // its peak before that point (since the last attack). A slow attack (bowed, blown, sung)
            // keeps rising and never dips, so it is not a re-strike
            let mut m = (self.last_trig[k].max(f - RING as i64 + 1)..from).map(|j| self.energy_at(k, j)).fold(0f32, f32::max);
            let mut dipped = false;
            for j in from..f {
                let ej = self.energy_at(k, j);
                if ej <= tun!(RESTRIKE_DIP) * m {
                    dipped = true;
                }
                m = m.max(ej);
            }
            if !dipped {
                return (0.0, 0);
            }
            (e / lo.max(1e-12), lifted)
        };
        // a modulated tone (vibrato, chorus) moves in pitch; a re-strike does not
        // (measured before the attack frames, whose transient blurs every peak position)
        // (from D3 up: below it the long windows smear a re-strike's transient over too many frames
        // for the pitch track to stay clean)
        if k + LOW_KEY as usize >= RS_STAB_FROM && self.pitch_spread(k, f - 1, 12).is_some_and(|sp| sp > tun!(RS_STAB)) {
            return None;
        }
        let gfl = |j: i64| self.gflux[j.rem_euclid(RING as i64) as usize];
        // the strike must lift the key's own 1st or 3rd partial (gate window): a key an octave or
        // twelfth above struck over it lifts only its 2nd/4th or 3rd partials
        if tun!(OWN_LIFT) > 0.0 {
            let hg = |j: i64, h: usize| self.hgenergy[k][j.rem_euclid(RING as i64) as usize][h];
            let lifted = |h: usize| {
                let lo = (f - 6..f).map(|j| hg(j, h)).fold(f32::MAX, f32::min);
                hg(f, h) >= tun!(OWN_LIFT) * lo.max(1e-12)
            };
            if !lifted(0) && !lifted(2) {
                return None;
            }
        }
        // a re-strike reaches a level near the note's own peak: a bump in its reverb tail, tens of dB
        // down, is not a new strike
        if self.energy_at(k, f) < self.peak_e[k] * db_down(tun!(RS_LVL)) {
            return None;
        }
        let (r4, l4) = rise(4);
        // the fast path still needs attack evidence near the rise: broadband flux, or a change in
        // the balance of the key's own partials (a new strike's partials start in their own phase
        // and brightness, so their ratios move while the old tone dies; a tremolo or swell scales
        // every partial alike and leaves the balance where it was)
        let sc = self.shape_change(k, f);
        // (but not a scramble: chorus and bowing noise reshuffle the partials far more than a
        // re-strike of the same string does)
        let attack = (f - 4..=f).any(|j| gfl(j) >= tun!(FAST_FLUX)) || (sc >= tun!(SHAPE_MIN) && sc <= tun!(SHAPE_MAX));
        if r4 >= tun!(RESTRIKE_FAST) && l4 >= 3 && attack {
            return Some(r4);
        }
        let latch = RESTRIKE_LATCH as i64;
        let g = |j: i64| self.gflux[j.rem_euclid(RING as i64) as usize];
        // a rising flux onset after the last attack settled (a chord's own attack flux is falling by
        // then); the frame after it must exist, so f itself only counts as that neighbour
        // (an attack can spread its flux over two frames: their sum counts too)
        let onset = ((f - latch).max(settled + 1)..f)
            .any(|j| (g(j) >= tun!(GF_ONSET) || g(j) + g(j + 1) >= 1.6 * tun!(GF_ONSET)) && g(j) > g(j - 1));
        if !onset {
            return None;
        }
        let (r8, l8) = rise(latch + 2);
        (r8 >= tun!(RESTRIKE) && l8 >= 3).then_some(r8)
    }

    /// The largest change, over the last 5 frames, of the balance of key k's fast partials: the mean
    /// absolute change of each partial's log share of the key's energy against 3 frames earlier.
    fn shape_change(&self, k: usize, f: i64) -> f32 {
        let he = |j: i64| self.henergy[k][j.rem_euclid(RING as i64) as usize];
        let share = |j: i64| -> [f32; 4] {
            let e = he(j);
            let tot: f32 = e.iter().sum::<f32>().max(1e-12);
            e.map(|v| (v.max(1e-12) / tot).ln())
        };
        (f - 4..=f)
            .map(|j| {
                let (a, b) = (share(j), share(j - 3));
                (0..4).map(|h| (a[h] - b[h]).abs()).sum::<f32>() / 4.0
            })
            .fold(0f32, f32::max)
    }

    fn track(&mut self, out: &mut Vec<Ev>) {
        self.pick();
        let f = self.frame;
        let min_frames = (MIN_NOTE_S * RATE as f64 / HOP as f64).ceil() as i64;
        let retrig = (RETRIG_S * RATE as f64 / HOP as f64).ceil() as i64;
        // velocities of this frame's notes are measured against the AGC as it was before the frame
        let agc0 = self.agc_db;
        let mut agc_new: Vec<f32> = Vec::new();
        // re-strike candidates first: when keys that share partials (an octave, twelfth, ...) rise
        // together, only the one with the larger own rise is re-struck (C3 struck lifts the partials
        // of a held C2 too)
        for k in 0..KEYS {
            if self.nh[k] > 0 {
                self.measure_pitch(k);
            } else {
                self.ppos[k][f.rem_euclid(RING as i64) as usize] = f32::NAN;
            }
        }
        // keys about to start a note this frame (rivals of held keys whose partials they share)
        let starting: Vec<bool> = (0..KEYS)
            .map(|k| tun!(RIVAL_NEW) > 0.5 && matches!(self.state[k], KeyState::Idle) && self.picked.contains(&k) && self.recent_rise(k, f))
            .collect();
        let mut rs: Vec<Option<f32>> = vec![None; KEYS];
        for k in 0..KEYS {
            // running peak of the sounding note's fast energy, measured up to the previous frame
            if matches!(self.state[k], KeyState::On { .. }) {
                let prev = self.energy_at(k, f - 1);
                self.peak_e[k] = self.peak_e[k].max(prev);
            }
            if matches!(self.state[k], KeyState::On { .. }) && self.picked.contains(&k) && f - self.last_trig[k] >= retrig {
                rs[k] = self.restrike(k, f);
            }
        }
        let strike: Vec<bool> = (0..KEYS)
            .map(|k| {
                let Some(r) = rs[k] else { return false };
                // a new key a fifth, octave, twelfth... above starting now is the attack, not a
                // re-strike of the held key under it
                if SHARED_STEPS.iter().any(|&d| k + d < KEYS && starting[k + d]) {
                    return false;
                }
                !HARMONIC_STEPS.iter().any(|&d| {
                    (k + d < KEYS && rs[k + d].is_some_and(|q| q >= r)) || (k >= d && rs[k - d].is_some_and(|q| q > r))
                })
            })
            .collect();
        for k in 0..KEYS {
            let active = self.picked.contains(&k);
            let pitch = LOW_KEY + k as u8;
            match self.state[k] {
                KeyState::Idle => {
                    // a note starts only at an attack of its own: a key that drifts into the pick
                    // list in the middle of other notes (their partials decaying unevenly) does not
                    if active && self.recent_rise(k, f) && !self.orphan_harmonic(k) && self.restart_ok(k, f) && !self.semitone_shadow(k) {
                        self.state[k] = KeyState::Pending { act: f, floor: self.ended[k] + 1, proven: false };
                        self.quiet[k] = 0;
                    }
                }
                KeyState::Pending { act, floor, proven } => {
                    if !active && !proven {
                        self.quiet[k] += 1;
                        if self.quiet[k] >= Self::off_frames(k) {
                            self.state[k] = KeyState::Idle;
                        }
                        continue;
                    }
                    self.quiet[k] = 0;
                    // a new note goes out after MIN_NOTE_S of being picked; a re-strike once its
                    // attack has peaked (it was already sounding, so it needs no 60 ms proof)
                    let due = if proven { f >= act + EHALF + 2 } else { f - act + 1 >= min_frames };
                    if due {
                        // never reach back past the previous note's attack or its end on this key
                        let from = (act - ONSET_WIN as i64 - 4).max(self.last_trig[k] + 1).max(floor);
                        let o = self.onset_frame(k, from, f);
                        let t0 = Self::centre_s(o);
                        let (vel, db) = self.velocity(k, o, f, agc0);
                        agc_new.push(db);
                        out.push(Ev::On { t: t0, pitch, vel });
                        self.state[k] = KeyState::On { t0 };
                        self.peak_e[k] = (o..f).map(|j| self.energy_at(k, j)).fold(0f32, f32::max);
                        self.last_trig[k] = o;
                    }
                }
                KeyState::On { t0 } => {
                    if !active {
                        self.quiet[k] += 1;
                        if self.quiet[k] >= Self::off_frames(k) {
                            let fo = f - self.quiet[k] as i64 + 1;
                            let t = Self::centre_s(fo).max(t0 + MIN_NOTE_S);
                            out.push(Ev::Off { t, pitch });
                            self.state[k] = KeyState::Idle;
                            self.ended[k] = Self::frame_of(t);
                        }
                        continue;
                    }
                    self.quiet[k] = 0;
                    if strike[k] {
                        // the old note ends where the new attack starts, so the two never overlap
                        let o = self.onset_frame(k, (f - RESTRIKE_LATCH as i64 - 2).max(self.last_trig[k] + 1), f);
                        let t = Self::centre_s(o).max(t0 + MIN_NOTE_S);
                        out.push(Ev::Off { t, pitch });
                        // the key was already sounding, so the new strike needs no 60 ms proof: it
                        // goes out once its attack has peaked (for the velocity), and a dip right
                        // after the strike can't lose it
                        let fl = Self::frame_of(t);
                        self.ended[k] = fl;
                        self.state[k] = KeyState::Pending { act: f, floor: fl, proven: true };
                        self.last_trig[k] = fl - 1;
                    }
                }
            }
        }
        if !agc_new.is_empty() {
            let mean = agc_new.iter().sum::<f32>() / agc_new.len() as f32;
            let a = agc0.unwrap_or(mean);
            // per-note AGC: about the last 10 notes set the reference
            self.agc_db = Some(a + (mean - a) * (0.1 * agc_new.len() as f32).min(1.0));
        }
    }

    /// A new key right next to a much stronger picked key (a semitone away) is its shadow: an
    /// intermodulation or clipping product on the stronger note's partials, not a minor second.
    fn semitone_shadow(&self, k: usize) -> bool {
        let sk = self.salr[k];
        [k.wrapping_sub(1), k + 1].iter().any(|&q| q < KEYS && self.picked.contains(&q) && sk < tun!(SEMI_K) * self.salr[q])
    }

    /// A key that ended moments ago restarts only out of a real dip of its own energy: a bowed or
    /// sung note that the pick dropped for a frame or two in its slow attack, and that is still
    /// rising, is not a new note.
    fn restart_ok(&self, k: usize, f: i64) -> bool {
        let e0 = self.ended[k];
        if f - e0 > RESTART_WIN || self.peak_e[k] <= 0.0 {
            return true;
        }
        let lo = (e0.max(f - RING as i64 + 1)..=f).map(|j| self.energy_at(k, j)).fold(f32::MAX, f32::min);
        lo <= tun!(RESTART_DIP) * self.peak_e[k]
    }

    /// Quiet frames before a key is off: 2 where the short windows judge it, more where the long
    /// window does (a re-strike or a damper dip blurs a low key's peak for several 11.6 ms frames).
    fn off_frames(k: usize) -> u32 {
        match size_for(LOW_KEY as f64 + k as f64) {
            0 => OFF_FRAMES + tun!(OFF_EXTRA_LOW) as u32,
            1 => OFF_FRAMES + tun!(OFF_EXTRA_MID) as u32,
            _ => OFF_FRAMES,
        }
    }

    /// The first frame whose centre is at or after t.
    fn frame_of(t: f64) -> i64 {
        (t * RATE as f64 / HOP as f64 - 1e-9).ceil() as i64
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
    /// Halfband length: (N - 1) / 2 = 94 input samples of delay, 47 output samples.
    const N: usize = 189;

    pub fn new() -> Decim2 {
        let n = Self::N;
        let mid = (n - 1) as f64 / 2.0;
        // halfband (cut at a quarter of the input rate), Kaiser beta 10: about -120 dB from 12 kHz, flat to 10 kHz,
        // so nothing above the analysis band folds back into it (an ultrasonic tone stays silent at
        // any input level)
        let mut taps: Vec<f32> = (0..n)
            .map(|i| {
                let d = i as f64 - mid;
                let sinc = if d == 0.0 { 0.5 } else { (std::f64::consts::PI * 0.5 * d).sin() / (std::f64::consts::PI * d) };
                (sinc * kaiser(d / mid, 12.0)) as f32
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
    pub const DELAY_OUT: usize = (Self::N - 1) / 4;

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
            // halfband: every other tap is zero except the centre, and the rest are symmetric
            let mid = (n - 1) / 2;
            let mut acc = w[mid] * self.taps[mid];
            let mut i = 1;
            while i <= mid {
                acc += (w[mid - i] + w[mid + i]) * self.taps[mid + i];
                i += 2;
            }
            out.push(acc);
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
    /// Output samples still to drop: the decimator's group delay, dropped like `to_rate` does, so the
    /// live grid matches the offline one sample for sample.
    skip: usize,
}

impl Default for Downmix {
    fn default() -> Self {
        Self::new()
    }
}

impl Downmix {
    pub fn new() -> Downmix {
        Downmix { d: Decim2::new(), mono: Vec::new(), skip: Decim2::DELAY_OUT }
    }
    pub fn push(&mut self, stereo: &[f32]) -> Vec<f32> {
        self.mono.clear();
        self.mono.extend(stereo.chunks_exact(2).map(|c| 0.5 * (c[0] + c[1])));
        let mut out = Vec::with_capacity(self.mono.len() / 2 + 1);
        let m = std::mem::take(&mut self.mono);
        self.d.push(&m, &mut out);
        self.mono = m;
        if self.skip > 0 {
            let k = self.skip.min(out.len());
            out.drain(..k);
            self.skip -= k;
        }
        out
    }
}

/// Kaiser window at x in [-1, 1].
fn kaiser(x: f64, beta: f64) -> f64 {
    fn i0(x: f64) -> f64 {
        let (mut sum, mut term) = (1.0, 1.0);
        for k in 1..40 {
            term *= (x / 2.0 / k as f64).powi(2);
            sum += term;
        }
        sum
    }
    if x.abs() > 1.0 {
        0.0
    } else {
        i0(beta * (1.0 - x * x).sqrt()) / i0(beta)
    }
}

/// Mono resampling for any pair of rates: a Kaiser-windowed sinc (cut at 0.45 of the lower rate,
/// 32 zero crossings a side, beta 12) read from a table, so it costs a multiply-add per tap instead
/// of a sin and a cos (`dsp::resample` took 23.6 ms per 0.5 s for 48 kHz files).
pub fn resample_mono(x: &[f32], from: u32, to: u32) -> Vec<f32> {
    const ZC: f64 = 32.0;
    const OVER: usize = 2048;
    let lo = from.min(to) as f64;
    let fc = 0.45 * lo / from as f64; // cutoff, cycles per input sample
    let half = ZC * from as f64 / lo; // input samples per side
    let table: Vec<f32> = (0..=(ZC as usize * OVER + 1))
        .map(|i| {
            let u = i as f64 / OVER as f64; // in zero crossings of the lower rate
            let d = u * from as f64 / lo; // in input samples
            let arg = 2.0 * fc * d;
            let sinc = if arg.abs() < 1e-12 { 1.0 } else { (std::f64::consts::PI * arg).sin() / (std::f64::consts::PI * arg) };
            (2.0 * fc * sinc * kaiser(u / ZC, 12.0)) as f32
        })
        .collect();
    let g = |d: f64| -> f32 {
        let u = d.abs() * lo / from as f64 * OVER as f64;
        let i = u as usize;
        if i + 1 >= table.len() {
            return 0.0;
        }
        let fr = (u - i as f64) as f32;
        table[i] * (1.0 - fr) + table[i + 1] * fr
    };
    let out_n = (x.len() as f64 * to as f64 / from as f64).round() as usize;
    let mut out = Vec::with_capacity(out_n);
    for j in 0..out_n {
        let t = j as f64 * from as f64 / to as f64;
        let (m0, m1) = ((t - half).ceil().max(0.0) as usize, ((t + half).floor() as usize).min(x.len().saturating_sub(1)));
        let (mut acc, mut norm) = (0f32, 0f32);
        let mut m = (t - half).ceil() as i64;
        while (m as f64) <= t + half {
            let h = g(t - m as f64);
            norm += h;
            if m >= m0 as i64 && m <= m1 as i64 && !x.is_empty() {
                acc += h * x[m as usize];
            }
            m += 1;
        }
        out.push(if norm != 0.0 { acc / norm } else { 0.0 });
    }
    out
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
    resample_mono(mono, rate, RATE)
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
    if bps == 0 || bits > 64 {
        return Err(format!("unsupported WAV format {tag}/{bits}-bit"));
    }
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

/// Test renders go to ~/.cache/<name>/run-<pid>: unique per test process, so two worktrees or
/// sessions running the FluidSynth tests at once never overwrite each other's files mid-read.
#[cfg(test)]
pub(crate) fn test_render_dir(name: &str) -> std::path::PathBuf {
    let d = crate::config::glib_home().join(".cache").join(name).join(format!("run-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[cfg(test)]
#[path = "piano_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "piano_breaker_tests.rs"]
mod breaker_tests;
