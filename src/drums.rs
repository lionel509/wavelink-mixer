//! Drum template subtraction (#23): find what the kick, snare, hi-hat and cowbell sound like in this
//! song and take them out before the harmonic pursuit, so drum hits stop making ghost notes; their
//! activations are also a GM drum track.
//!
//! Per hop, a short 1024-point frame (46 ms at 22.05 kHz):
//! - percussive estimate P = |X| above the per-bin median of the previous 7 frames (causal HPSS: a
//!   harmonic partial is steady, a hit is not);
//! - P in 48 log bands, fitted with 4 band templates by non-negative least squares (multiplicative
//!   updates); the templates start generic (kick 40-120 Hz plus beater click, snare body ~200 Hz plus
//!   1-8 kHz noise, hats above 5 kHz, cowbell peaks ~560/845 Hz) and adapt to the song on clear hits;
//! - the fitted drum energy per band, spread over the bins in proportion to P, is the drum part:
//!   a per-bin drum share for the stem, and per band the fraction the harmonic analysis removes;
//! - onsets of each activation are drum notes (36 kick, 38 snare, 42 hat, 56 cowbell).
use crate::notes::Note;
use crate::piano::RATE;
use crate::subtract::Fft;

pub const SN: usize = 1024;
const NB: usize = 48;
const HIST: usize = 7;
pub const GM: [u8; 4] = [36, 38, 42, 56];

pub fn band_edges() -> Vec<usize> {
    let df = RATE as f32 / SN as f32;
    (0..=NB).map(|b| ((30.0 * (11000.0f32 / 30.0).powf(b as f32 / NB as f32)) / df).round().max(1.0) as usize).map(|k| k.min(SN / 2)).collect()
}

fn generic_templates() -> [[f32; NB]; 4] {
    let mut t = [[0f32; NB]; 4];
    for b in 0..NB {
        let f = 30.0 * (11000.0f32 / 30.0).powf((b as f32 + 0.5) / NB as f32);
        let g = |c: f32, w: f32| (-0.5 * ((f / c).log2() / w).powi(2)).exp();
        t[0][b] = g(60.0, 0.7) + 0.15 * g(3000.0, 1.0);
        t[1][b] = 0.8 * g(200.0, 0.5) + 0.6 * g(3000.0, 1.2);
        t[2][b] = 1.0 / (1.0 + (-4.0 * (f / 6000.0).log2()).exp());
        t[3][b] = g(560.0, 0.08) + 0.8 * g(845.0, 0.08);
    }
    for row in t.iter_mut() {
        let n = row.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-9);
        row.iter_mut().for_each(|v| *v /= n);
    }
    t
}

pub struct DrumFrame {
    pub act: [f32; 4],
    /// Drum share of each band's magnitude, 0..1.
    pub band_frac: [f32; NB],
    /// Drum share of each bin's magnitude (SN/2 + 1 bins), 0..1.
    pub bin_share: Vec<f32>,
    /// Hits this frame: (GM note, velocity).
    pub hits: Vec<(u8, u8)>,
    /// Band magnitude sums, for the long-window mapping.
    pub band_mag: [f32; NB],
}

pub struct Drums {
    fft: Fft,
    win: Vec<f32>,
    re: Vec<f32>,
    im: Vec<f32>,
    hist: Vec<[f32; HIST]>,
    hpos: usize,
    edges: Vec<usize>,
    pub tpl: [[f32; NB]; 4],
    prev: [[f32; 4]; 2],
    peak: [f32; 4],
    refr: [u32; 4],
}

impl Default for Drums {
    fn default() -> Self {
        Self::new()
    }
}

impl Drums {
    pub fn new() -> Drums {
        Drums {
            fft: Fft::new(SN),
            win: (0..SN).map(|i| (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / SN as f64).cos()) as f32).collect(),
            re: vec![0.0; SN],
            im: vec![0.0; SN],
            hist: vec![[0.0; HIST]; SN / 2 + 1],
            hpos: 0,
            edges: band_edges(),
            tpl: generic_templates(),
            prev: [[0.0; 4]; 2],
            peak: [1e-6; 4],
            refr: [0; 4],
        }
    }

    /// `get(i)`, i in 0..SN: the short window's samples. Returns the frame's drums; `spec` receives the
    /// complex spectrum (re, im) when the caller wants to mask it (the stem).
    pub fn frame(&mut self, get: impl Fn(usize) -> f32, spec: Option<(&mut Vec<f32>, &mut Vec<f32>)>) -> DrumFrame {
        for i in 0..SN {
            self.re[i] = get(i) * self.win[i];
            self.im[i] = 0.0;
        }
        self.fft.run(&mut self.re, &mut self.im, false);
        let nb = SN / 2 + 1;
        let mut mag = vec![0f32; nb];
        let mut p = vec![0f32; nb];
        for k in 0..nb {
            mag[k] = (self.re[k] * self.re[k] + self.im[k] * self.im[k]).sqrt();
            let mut h = self.hist[k];
            h.sort_by(|a, b| a.total_cmp(b));
            p[k] = (mag[k] - 1.3 * h[HIST / 2]).max(0.0);
            self.hist[k][self.hpos] = mag[k];
        }
        self.hpos = (self.hpos + 1) % HIST;
        let (mut pb, mut xb) = ([0f32; NB], [0f32; NB]);
        for b in 0..NB {
            for k in self.edges[b]..self.edges[b + 1].max(self.edges[b] + 1).min(nb) {
                pb[b] += p[k];
                xb[b] += mag[k];
            }
        }
        // NNLS by multiplicative updates
        let mut a = [0f32; 4];
        for j in 0..4 {
            a[j] = (0..NB).map(|b| self.tpl[j][b] * pb[b]).sum::<f32>().max(1e-9);
        }
        for _ in 0..12 {
            let mut est = [0f32; NB];
            for b in 0..NB {
                est[b] = (0..4).map(|j| self.tpl[j][b] * a[j]).sum::<f32>() + 1e-9;
            }
            for j in 0..4 {
                let num: f32 = (0..NB).map(|b| self.tpl[j][b] * pb[b]).sum();
                let den: f32 = (0..NB).map(|b| self.tpl[j][b] * est[b]).sum::<f32>() + 1e-9;
                a[j] *= num / den;
            }
        }
        let mut db = [0f32; NB];
        let mut band_frac = [0f32; NB];
        for b in 0..NB {
            db[b] = (0..4).map(|j| self.tpl[j][b] * a[j]).sum::<f32>().min(pb[b]);
            band_frac[b] = if xb[b] > 0.0 { (db[b] / xb[b]).clamp(0.0, 1.0) } else { 0.0 };
        }
        let mut bin_share = vec![0f32; nb];
        for b in 0..NB {
            if pb[b] <= 0.0 {
                continue;
            }
            for k in self.edges[b]..self.edges[b + 1].max(self.edges[b] + 1).min(nb) {
                let d = db[b] * p[k] / pb[b];
                bin_share[k] = if mag[k] > 0.0 { (d / mag[k]).clamp(0.0, 1.0) } else { 0.0 };
            }
        }
        // hits: a rise over the last two frames, above a quarter of the running peak, 70 ms apart
        let fs = crate::subtract::HOP as f32 / RATE as f32;
        let mut hits = Vec::new();
        let tot: f32 = a.iter().sum::<f32>().max(1e-9);
        for j in 0..4 {
            self.peak[j] = (self.peak[j] * 10f32.powf(-0.15 * fs)).max(a[j]);
            self.refr[j] = self.refr[j].saturating_sub(1);
            let base = 0.5 * (self.prev[0][j] + self.prev[1][j]);
            if self.refr[j] == 0 && a[j] > 0.25 * self.peak[j] && a[j] > 1.8 * base + 1e-9 {
                let vel = (40.0 + 80.0 * (a[j] / self.peak[j]).sqrt()).min(120.0) as u8;
                hits.push((GM[j], vel));
                self.refr[j] = 6;
                // adapt this template to the song on a clear hit
                if a[j] > 0.5 * tot {
                    let n = pb.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-9);
                    let mut s = 0f32;
                    for b in 0..NB {
                        self.tpl[j][b] = 0.97 * self.tpl[j][b] + 0.03 * pb[b] / n;
                        s += self.tpl[j][b] * self.tpl[j][b];
                    }
                    let s = s.sqrt().max(1e-9);
                    self.tpl[j].iter_mut().for_each(|v| *v /= s);
                }
            }
        }
        self.prev[1] = self.prev[0];
        self.prev[0] = a;
        if let Some((re, im)) = spec {
            re.clear();
            im.clear();
            re.extend_from_slice(&self.re);
            im.extend_from_slice(&self.im);
        }
        DrumFrame { act: a, band_frac, bin_share, hits, band_mag: xb }
    }
}

/// Offline: the drum stem (short STFT masked by the drum share, overlap-add) and the drum notes.
pub fn stem_and_notes(mono: &[f32]) -> (Vec<f32>, Vec<Note>) {
    let hop = crate::subtract::HOP;
    let mut d = Drums::new();
    let inv = Fft::new(SN);
    let win = d.win.clone();
    let mut out = vec![0f32; mono.len()];
    let mut norm = vec![0f32; mono.len()];
    let (mut re, mut im) = (Vec::new(), Vec::new());
    let mut notes = Vec::new();
    let nfr = mono.len() / hop + 1;
    for f in 0..nfr {
        let c0 = (f * hop) as isize - (SN / 2) as isize;
        let fr = d.frame(
            |i| {
                let s = c0 + i as isize;
                if s >= 0 && (s as usize) < mono.len() {
                    mono[s as usize]
                } else {
                    0.0
                }
            },
            Some((&mut re, &mut im)),
        );
        for k in 0..=SN / 2 {
            let m = fr.bin_share[k];
            re[k] *= m;
            im[k] *= m;
            if k > 0 && k < SN / 2 {
                re[SN - k] = re[k];
                im[SN - k] = -im[k];
            }
        }
        inv.run(&mut re, &mut im, true);
        for i in 0..SN {
            let s = c0 + i as isize;
            if s >= 0 && (s as usize) < mono.len() {
                out[s as usize] += re[i] / SN as f32 * win[i];
                norm[s as usize] += win[i] * win[i];
            }
        }
        let t = (f * hop) as f64 / RATE as f64;
        for &(p, v) in &fr.hits {
            notes.push(Note { t, dur: 0.1, pitch: p, vel: v });
        }
    }
    for (o, n) in out.iter_mut().zip(norm) {
        *o /= n.max(1e-3);
    }
    (out, notes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kick_and_hat_hits_are_found_and_named() {
        // kick: 60 Hz decaying sine burst every 0.5 s; hat: high noise burst on the off-beats
        let n = RATE as usize * 3;
        let mut x = vec![0f32; n];
        let mut seed = 1u64;
        for b in 0..6 {
            let s0 = (b as f64 * 0.5 * RATE as f64) as usize + 2000;
            for i in 0..4000 {
                let t = i as f32 / RATE as f32;
                x[s0 + i] += 0.6 * (2.0 * std::f32::consts::PI * 60.0 * t).sin() * (-t * 25.0).exp();
            }
            let h0 = s0 + RATE as usize / 4;
            for i in 0..1500 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let w = ((seed >> 33) as f32 / (1u64 << 31) as f32) - 0.5;
                if h0 + i < n {
                    x[h0 + i] += 0.3 * w * (-(i as f32) / 300.0).exp();
                }
            }
        }
        // crude high-pass on the hat noise is implicit: white noise is mostly above 5 kHz in band terms
        let (_, notes) = stem_and_notes(&x);
        let kicks = notes.iter().filter(|n| n.pitch == 36).count();
        let hats = notes.iter().filter(|n| n.pitch == 42).count();
        assert!((5..=7).contains(&kicks), "kicks {kicks}: {notes:?}");
        assert!(hats >= 4, "hats {hats}: {notes:?}");
    }
}
