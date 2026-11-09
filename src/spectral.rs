//! "Talking piano" (#23): spectral resynthesis on the 88 keys, not transcription (the speaking
//! piano idea). Every T ms (MIXER_PIANO_SPECTRAL_MS, 40-120, default 60) the raw spectral energy
//! of each key's band (+-half a semitone, harmonics included: the spectrum is what is reproduced)
//! becomes a velocity: dB under the frame's loudest key over a 36 dB range, scaled by an overall
//! AGC; keys under the floor stay silent, only local maxima within +-1 semitone are kept, at most
//! MAX_KEYS per frame (strongest first). Each frame releases the last frame's keys and strikes this
//! frame's (a key that stays loud is struck again, as a player piano speaks); with
//! MIXER_PIANO_SPECTRAL_RING=1 a key that only got quieter rings on instead (re-struck on a 3 dB rise).
//!
//! Multi-resolution: keys below C4 from a 4096-point window, C4-C6 from 2048, above from 1024 (at
//! 22.05 kHz), all centred on the frame time. Latency: half the longest window that is needed for
//! the key (93 ms for the bass, 46 ms for the middle) plus T.
use crate::piano::{Ev, KEYS, LOW_KEY, RATE};
use crate::subtract::Fft;

const SIZES: [usize; 3] = [4096, 2048, 1024];

fn max_keys() -> usize {
    std::env::var("MIXER_PIANO_SPECTRAL_KEYS").ok().and_then(|v| v.parse().ok()).unwrap_or(32).clamp(4, 88)
}

pub fn period_s() -> f64 {
    std::env::var("MIXER_PIANO_SPECTRAL_MS").ok().and_then(|v| v.parse::<f64>().ok()).unwrap_or(60.0).clamp(40.0, 120.0) / 1000.0
}

pub struct Spectral {
    ffts: Vec<Fft>,
    wins: Vec<Vec<f32>>,
    re: Vec<f32>,
    im: Vec<f32>,
    buf: Vec<f32>,
    start: u64,
    n_in: u64,
    /// Next frame's centre, in samples.
    next: u64,
    hop: u64,
    /// Per key: (fft index, lo bin, hi bin, fractional centre bin).
    bands: Vec<(usize, usize, usize, f32)>,
    /// Keys struck last frame with their level (dB).
    struck: Vec<(u8, f32)>,
    agc_db: f32,
    ring: bool,
    pub last_view: [f32; KEYS],
}

impl Default for Spectral {
    fn default() -> Self {
        Self::new()
    }
}

impl Spectral {
    pub fn new() -> Spectral {
        let bands = (0..KEYS)
            .map(|k| {
                let m = LOW_KEY as f32 + k as f32;
                let s = if m < 60.0 { 0 } else if m < 84.0 { 1 } else { 2 };
                let df = RATE as f32 / SIZES[s] as f32;
                let f = 440.0 * 2f32.powf((m - 69.0) / 12.0);
                let (lo, hi) = (f * 2f32.powf(-1.0 / 24.0) / df, f * 2f32.powf(1.0 / 24.0) / df);
                (s, lo.ceil() as usize, (hi.floor() as usize).max(lo.ceil() as usize), f / df)
            })
            .collect();
        Spectral {
            ffts: SIZES.iter().map(|&n| Fft::new(n)).collect(),
            wins: SIZES.iter().map(|&n| (0..n).map(|i| (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / n as f64).cos()) as f32).collect()).collect(),
            re: vec![0.0; SIZES[0]],
            im: vec![0.0; SIZES[0]],
            buf: Vec::with_capacity(2 * SIZES[0]),
            start: 0,
            n_in: 0,
            next: 0,
            hop: (period_s() * RATE as f64).round() as u64,
            bands,
            struck: Vec::new(),
            agc_db: -60.0,
            ring: std::env::var("MIXER_PIANO_SPECTRAL_RING").as_deref() == Ok("1"),
            last_view: [0.0; KEYS],
        }
    }

    pub fn now(&self) -> f64 {
        self.n_in as f64 / RATE as f64
    }

    pub fn push(&mut self, pcm: &[f32]) -> Vec<Ev> {
        self.buf.extend_from_slice(pcm);
        self.n_in += pcm.len() as u64;
        let mut evs = Vec::new();
        while self.next + (SIZES[0] / 2) as u64 <= self.n_in {
            self.frame(&mut evs);
            self.next += self.hop;
            let keep = self.next.saturating_sub((SIZES[0] / 2) as u64);
            if keep > self.start + SIZES[0] as u64 {
                let d = (keep - self.start) as usize;
                self.buf.drain(..d.min(self.buf.len()));
                self.start += d as u64;
            }
        }
        evs
    }

    fn frame(&mut self, evs: &mut Vec<Ev>) {
        let t = self.next as f64 / RATE as f64;
        let mut e = [0f32; KEYS];
        for (s, &n) in SIZES.iter().enumerate() {
            let c0 = self.next as i64 - (n / 2) as i64 - self.start as i64;
            for i in 0..n {
                let j = c0 + i as i64;
                self.re[i] = if j >= 0 && (j as usize) < self.buf.len() { self.buf[j as usize] * self.wins[s][i] } else { 0.0 };
                self.im[i] = 0.0;
            }
            self.ffts[s].run(&mut self.re[..n], &mut self.im[..n], false);
            let mag2 = |k: usize| self.re[k] * self.re[k] + self.im[k] * self.im[k];
            for (k, &(fs, lo, hi, c)) in self.bands.iter().enumerate() {
                if fs != s {
                    continue;
                }
                e[k] = if hi > lo && hi < n / 2 {
                    (lo..=hi).map(mag2).sum::<f32>()
                } else {
                    // band narrower than a bin: interpolate the magnitude at the key
                    let b = c.floor() as usize;
                    let fr = c - b as f32;
                    let (a, bb) = (mag2(b.min(n / 2)).sqrt(), mag2((b + 1).min(n / 2)).sqrt());
                    let m = a + (bb - a) * fr;
                    m * m
                };
                // per-size scaling: energy of a sinusoid grows with the window length
                e[k] /= (n * n) as f32;
            }
        }
        let db: Vec<f32> = e.iter().map(|&v| 10.0 * v.max(1e-14).log10()).collect();
        let fmax = db.iter().fold(-140f32, |m, &v| m.max(v));
        // AGC: the running loudest frame, falling 3 dB/s
        self.agc_db = (self.agc_db - 3.0 * self.hop as f32 / RATE as f32).max(fmax);
        let level = ((fmax - self.agc_db + 30.0) / 30.0).clamp(0.0, 1.0);
        let mut keys: Vec<(usize, f32)> = (0..KEYS)
            .filter(|&k| db[k] > fmax - 36.0 && fmax > -100.0)
            .filter(|&k| (k == 0 || db[k] >= db[k - 1]) && (k + 1 == KEYS || db[k] >= db[k + 1]))
            .map(|k| (k, db[k]))
            .collect();
        keys.sort_by(|a, b| b.1.total_cmp(&a.1));
        keys.truncate(max_keys());
        self.last_view = [0.0; KEYS];
        let mut next: Vec<(u8, f32)> = Vec::with_capacity(keys.len());
        let mut on: Vec<(u8, u8)> = Vec::new();
        for &(k, d) in &keys {
            let pitch = (LOW_KEY as usize + k) as u8;
            let rel = ((d - fmax + 36.0) / 36.0).clamp(0.0, 1.0);
            let vel = (1.0 + 126.0 * rel * (0.35 + 0.65 * level)).round().clamp(1.0, 127.0) as u8;
            self.last_view[k] = rel;
            let prev = self.struck.iter().find(|s| s.0 == pitch).map(|s| s.1);
            if self.ring && prev.is_some_and(|p| d < p + 3.0) {
                next.push((pitch, d.min(prev.unwrap_or(d))));
                continue;
            }
            on.push((pitch, vel));
            next.push((pitch, d));
        }
        // release what was struck and is not kept ringing, then strike
        for &(p, _) in &self.struck {
            let keep = next.iter().any(|n| n.0 == p) && !on.iter().any(|o| o.0 == p);
            if !keep {
                evs.push(Ev::Off { t, pitch: p });
            }
        }
        for &(p, v) in &on {
            evs.push(Ev::On { t, pitch: p, vel: v });
        }
        self.struck = next;
    }

    pub fn all_off(&mut self, t: f64) -> Vec<Ev> {
        let evs = self.struck.iter().map(|&(p, _)| Ev::Off { t, pitch: p }).collect();
        self.struck.clear();
        evs
    }

    pub fn flush(&mut self) -> Vec<Ev> {
        let mut evs = self.push(&vec![0.0; SIZES[0]]);
        evs.extend(self.all_off(self.now()));
        evs
    }
}

/// Offline: the talking-piano notes for mono PCM at RATE.
pub fn notes(mono: &[f32]) -> Vec<crate::notes::Note> {
    let mut s = Spectral::new();
    let mut evs = Vec::new();
    for c in mono.chunks(220) {
        evs.extend(s.push(c));
    }
    evs.extend(s.flush());
    crate::piano::events_to_notes(&evs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tone_strikes_its_key_every_period() {
        let x: Vec<f32> = (0..RATE as usize).map(|i| 0.3 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / RATE as f32).sin()).collect();
        let n = notes(&x);
        let a4: Vec<_> = n.iter().filter(|n| n.pitch == 69).collect();
        let per = period_s();
        assert!(a4.len() as f64 >= 0.7 / per, "{} strikes of A4: {n:?}", a4.len());
        assert!(a4.iter().all(|n| n.dur <= per + 1e-6));
        let loudest = n.iter().max_by_key(|n| n.vel).unwrap();
        assert_eq!(loudest.pitch, 69);
    }
}
