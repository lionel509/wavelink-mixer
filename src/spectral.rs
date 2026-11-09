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
    let def = if crate::subtract::env_on("MIXER_PIANO_SPECTRAL_V1", false) { 32 } else { 12 };
    std::env::var("MIXER_PIANO_SPECTRAL_KEYS").ok().and_then(|v| v.parse().ok()).unwrap_or(def).clamp(4, 88)
}

fn tun_s(name: &str, def: f32) -> f32 {
    std::env::var("SPECTRAL_TUNE")
        .ok()
        .and_then(|s| s.split(',').find_map(|kv| kv.split_once('=').filter(|(k, _)| k.trim() == name).and_then(|(_, v)| v.trim().parse().ok())))
        .unwrap_or(def)
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
    /// v1: band energy per key, every key re-struck (MIXER_PIANO_SPECTRAL_V1=1).
    v1: bool,
    pub frames: usize,
    pub keys_total: usize,
    pub last_view: [f32; KEYS],
}

impl Default for Spectral {
    fn default() -> Self {
        Self::new()
    }
}

impl Spectral {
    pub fn new() -> Spectral {
        let v1 = crate::subtract::env_on("MIXER_PIANO_SPECTRAL_V1", false);
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
            ring: crate::subtract::env_on("MIXER_PIANO_SPECTRAL_RING", !v1),
            v1,
            frames: 0,
            keys_total: 0,
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
        if self.v1 {
            return self.frame_v1(evs);
        }
        let t = self.next as f64 / RATE as f64;
        // v2: true sinusoidal peaks (parabolic, >= PROM dB over the local floor), each mapped to its
        // nearest key, weighted for the voice/melody band, the strongest N kept
        let prom = tun_s("PROM", 7.0);
        let mut best = [f32::NEG_INFINITY; KEYS];
        for (s, &n) in SIZES.iter().enumerate() {
            let c0 = self.next as i64 - (n / 2) as i64 - self.start as i64;
            for i in 0..n {
                let j = c0 + i as i64;
                self.re[i] = if j >= 0 && (j as usize) < self.buf.len() { self.buf[j as usize] * self.wins[s][i] } else { 0.0 };
                self.im[i] = 0.0;
            }
            self.ffts[s].run(&mut self.re[..n], &mut self.im[..n], false);
            let df = RATE as f32 / n as f32;
            let half = n / 2;
            let db: Vec<f32> = (0..half).map(|k| 10.0 * ((self.re[k] * self.re[k] + self.im[k] * self.im[k]) / (n * n) as f32).max(1e-14).log10()).collect();
            // this size owns the keys of its range (bass from the longest window)
            let (mlo, mhi) = match s {
                0 => (20.5f32, 59.5f32),
                1 => (59.5, 83.5),
                _ => (83.5, 108.5),
            };
            let mut loc: Vec<f32> = Vec::with_capacity(31);
            for k in 2..half - 2 {
                let m = db[k];
                if m < db[k - 1] || m < db[k + 1] {
                    continue;
                }
                let d = 0.5 * (db[k - 1] - db[k + 1]) / (db[k - 1] - 2.0 * m + db[k + 1]).min(-1e-6);
                let f = (k as f32 + d.clamp(-0.5, 0.5)) * df;
                if f < 26.0 {
                    continue;
                }
                let midi = 69.0 + 12.0 * (f / 440.0).log2();
                if midi < mlo || midi >= mhi {
                    continue;
                }
                loc.clear();
                loc.extend_from_slice(&db[k.saturating_sub(15)..(k + 16).min(half)]);
                loc.sort_by(|a, b| a.total_cmp(b));
                if m - loc[loc.len() / 2] < prom {
                    continue;
                }
                let peak = m - 0.25 * (db[k - 1] - db[k + 1]) * d;
                // emphasis: the voice/melody band; high harmonics above 3 kHz fall 6 dB/oct
                let w = if f > 3000.0 { -6.0 * (f / 3000.0).log2() } else if f < 200.0 { -2.0 } else { 0.0 };
                let key = (midi.round() as i32 - LOW_KEY as i32).clamp(0, KEYS as i32 - 1) as usize;
                best[key] = best[key].max(peak + w);
            }
        }
        let fmax = best.iter().fold(-140f32, |m, &v| m.max(v));
        self.agc_db = (self.agc_db - 3.0 * self.hop as f32 / RATE as f32).max(fmax);
        let level = ((fmax - self.agc_db + 30.0) / 30.0).clamp(0.0, 1.0);
        let mut keys: Vec<(usize, f32)> = (0..KEYS).filter(|&k| best[k] > fmax - 36.0 && fmax > -100.0).map(|k| (k, best[k])).collect();
        keys.sort_by(|a, b| b.1.total_cmp(&a.1));
        keys.truncate(max_keys());
        self.frames += 1;
        self.keys_total += keys.len();
        self.last_view = [0.0; KEYS];
        let mut next: Vec<(u8, f32)> = Vec::with_capacity(keys.len());
        let mut on: Vec<(u8, u8)> = Vec::new();
        for &(k, d) in &keys {
            let pitch = (LOW_KEY as usize + k) as u8;
            let rel = ((d - fmax + 36.0) / 36.0).clamp(0.0, 1.0);
            let vel = (1.0 + 126.0 * rel * (0.35 + 0.65 * level)).round().clamp(1.0, 127.0) as u8;
            self.last_view[k] = rel;
            let prev = self.struck.iter().find(|s| s.0 == pitch).map(|s| s.1);
            // temporal coherence: a key already sounding rings on unless it rose >= 3 dB
            if self.ring && prev.is_some_and(|p| d < p + 3.0) {
                next.push((pitch, d.min(prev.unwrap_or(d))));
                continue;
            }
            on.push((pitch, vel));
            next.push((pitch, d));
        }
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

    fn frame_v1(&mut self, evs: &mut Vec<Ev>) {
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
        self.frames += 1;
        self.keys_total += keys.len();
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
    notes_stats(mono).0
}

/// The notes plus the mean number of keys selected per frame.
pub fn notes_stats(mono: &[f32]) -> (Vec<crate::notes::Note>, f64) {
    let mut s = Spectral::new();
    let mut evs = Vec::new();
    for c in mono.chunks(220) {
        evs.extend(s.push(c));
    }
    evs.extend(s.flush());
    (crate::piano::events_to_notes(&evs), s.keys_total as f64 / s.frames.max(1) as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_steady_tone_is_struck_and_held() {
        let x: Vec<f32> = (0..RATE as usize).map(|i| 0.3 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / RATE as f32).sin()).collect();
        let n = notes(&x);
        let a4: Vec<_> = n.iter().filter(|n| n.pitch == 69).collect();
        let per = period_s();
        // ring mode (default): a steady tone is struck once and held
        assert!(!a4.is_empty() && a4.iter().map(|n| n.dur).sum::<f64>() > 0.8, "{n:?}");
        let _ = per;
        let loudest = n.iter().max_by_key(|n| n.vel).unwrap();
        assert_eq!(loudest.pitch, 69);
    }
}
