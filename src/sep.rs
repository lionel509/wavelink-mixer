//! Separators and the transcriber: StemgenRT streaming (Live), Open-Unmix UMX-HQ (Quality), Basic Pitch notes.
//! Unit 1 skeleton: bodies are `todo!()` until Unit 3.
#![allow(dead_code, unused_imports, unused_variables)]
use crate::models::{ModelSet, S_FILES, U_FILES};
use crate::notes::Note;

// ---- Live (StemgenRT) ----
pub const BLOCK: usize = 128;
pub const BLOCK_MS: f64 = 128.0 * 1000.0 / 44100.0;
pub const PARALLEL: bool = false;

/// Recurrent state tensors, in the order the model expects them.
pub const STATES: [(&str, &[i64]); 13] = [
    ("drums_audio_history", &[1, 2, 896]),
    ("drums_local_hidden", &[2, 20, 96]),
    ("drums_global_hidden", &[2, 1, 192]),
    ("drums_spectral_numerator_tail", &[1, 1, 2, 128]),
    ("drums_waveform_numerator_tail", &[1, 1, 2, 128]),
    ("bass_audio_history", &[1, 2, 3968]),
    ("bass_local_hidden", &[2, 17, 80]),
    ("bass_global_hidden", &[2, 1, 160]),
    ("bass_spectral_numerator_tail", &[1, 1, 2, 128]),
    ("vocals_audio_history", &[1, 2, 896]),
    ("vocals_local_hidden", &[2, 20, 96]),
    ("vocals_global_hidden", &[2, 1, 192]),
    ("vocals_spectral_numerator_tail", &[1, 1, 2, 128]),
];

/// LRLR... -> L..., R...
pub fn deinterleave(x: &[f32]) -> Vec<f32> {
    todo!()
}

/// L..., R... -> LRLR...
pub fn interleave(p: &[f32]) -> Vec<f32> {
    todo!()
}

pub struct Streamer {}

impl Streamer {
    /// Intra threads 1; parallel: parallel execution with 3 inter threads, else 1 inter thread. File S_FILES[0].
    pub fn load(m: &ModelSet, parallel: bool) -> Result<Streamer, String> {
        todo!()
    }

    pub fn reset(&mut self) {
        todo!()
    }

    /// planar.len() == 256. First call after load/reset -> Ok(None); call n returns call n-1's input,
    /// as [drums, bass, vocals, other], each interleaved (256).
    pub fn process(&mut self, planar: &[f32]) -> Result<Option<[Vec<f32>; 4]>, String> {
        todo!()
    }
}

/// Wall ms per call over `calls` calls of LCG noise (0.1), then reset.
pub fn calibrate(s: &mut Streamer, calls: usize) -> Result<Vec<f64>, String> {
    todo!()
}

// ---- Quality (UMX-HQ) ----

pub struct Separator {}

impl Separator {
    /// Each session with intra threads 2; files U_FILES[0..4].
    pub fn load(m: &ModelSet) -> Result<Separator, String> {
        todo!()
    }

    /// Returns [drums, bass, other] (vocals dropped), interleaved stereo at 44.1 kHz.
    pub fn separate(&mut self, pcm44: &[f32]) -> Result<[Vec<f32>; 3], String> {
        todo!()
    }

    /// separate() three times on 2.0 s of LCG stereo (0.3); max ms / 2.0.
    pub fn ms_per_s(&mut self) -> Result<f64, String> {
        todo!()
    }
}

// ---- Notes (Basic Pitch) ----
pub const FPS: f64 = 22050.0 / 256.0;
pub const BP_SAMPLES: usize = 43844;
pub const ONSET_THRESH: f32 = 0.5;
pub const FRAME_THRESH: f32 = 0.3;
pub const MIN_NOTE_FRAMES: usize = 5;
pub const ENERGY_TOL: usize = 11;

pub struct Transcriber {}

impl Transcriber {
    /// File "basic-pitch-nmp.onnx".
    pub fn load(m: &ModelSet) -> Result<Transcriber, String> {
        todo!()
    }

    /// Mono at 44.1 kHz in; notes with t relative to the start of the input.
    pub fn notes(&mut self, mono44: &[f32]) -> Result<Vec<Note>, String> {
        todo!()
    }
}

/// note and onset are [frame][pitch] grids (172 x 88).
pub fn frames_to_notes(note: &[Vec<f32>], onset: &[Vec<f32>]) -> Vec<Note> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models;
    use std::time::Instant;

    /// Deterministic uniform [0, 1) from an LCG (no `rand` crate).
    fn lcg(s: &mut u64) -> f32 {
        *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((*s >> 33) as f32) / (1u64 << 31) as f32
    }

    fn grid(frames: usize, pitches: usize) -> Vec<Vec<f32>> {
        vec![vec![0.0; pitches]; frames]
    }

    #[test]
    fn frames_to_notes_single_note() {
        let mut note = grid(172, 88);
        let mut onset = grid(172, 88);
        for f in 20..60 {
            note[f][48] = 0.9;
        }
        onset[20][48] = 0.9;
        let ns = frames_to_notes(&note, &onset);
        assert_eq!(ns.len(), 1, "{ns:?}");
        let n = &ns[0];
        assert_eq!(n.pitch, 69);
        assert!((n.t - 20.0 / FPS).abs() < 1e-3, "t {}", n.t);
        assert!((n.dur - 40.0 / FPS).abs() < 1e-3, "dur {}", n.dur);
        assert_eq!(n.vel, 114);
    }

    #[test]
    fn frames_to_notes_short_blob_dropped() {
        let mut note = grid(172, 88);
        let mut onset = grid(172, 88);
        for f in 20..23 {
            note[f][48] = 0.9;
        }
        onset[20][48] = 0.9;
        assert!(frames_to_notes(&note, &onset).is_empty());
    }

    #[test]
    fn frames_to_notes_gap_tolerance() {
        let mut note = grid(172, 88);
        let mut onset = grid(172, 88);
        for f in 20..40 {
            note[f][48] = 0.9;
        }
        for f in 45..60 {
            note[f][48] = 0.9;
        }
        onset[20][48] = 0.9;
        let ns = frames_to_notes(&note, &onset);
        assert_eq!(ns.len(), 1, "{ns:?}");
        let end = ns[0].t + ns[0].dur;
        assert!((end - 60.0 / FPS).abs() < 1e-3, "end {end}");
    }

    #[test]
    fn states_table() {
        assert_eq!(STATES.len(), 13);
        let mut names: Vec<&str> = STATES.iter().map(|s| s.0).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 13, "names must be unique");
        assert!(STATES.iter().all(|(n, _)| ["drums_", "bass_", "vocals_"].iter().any(|p| n.starts_with(*p))));
        let total: i64 = STATES.iter().map(|s| s.1.iter().product::<i64>()).sum();
        assert_eq!(total, 24032);
    }

    #[test]
    fn interleave_roundtrip() {
        let x: Vec<f32> = (0..16).map(|i| i as f32).collect();
        assert_eq!(interleave(&deinterleave(&x)), x);
        assert_eq!(deinterleave(&[1., 2., 3., 4.]), vec![1., 3., 2., 4.]);
    }

    #[test]
    #[ignore]
    fn stream_reconstructs_mixture() {
        let m = models::ensure(&S_FILES, &|s: String| println!("{s}")).unwrap();
        let mut st = Streamer::load(&m, PARALLEL).unwrap();
        let tone = |i: usize| -> f32 {
            let t = i as f64 / 44100.0;
            let att = (t / 0.010).min(1.0);
            ((2.0 * std::f64::consts::PI * 110.0 * t).sin() * 0.4 * att) as f32
        };
        let click = |i: usize| -> f32 { if i % 22050 < 44 { 0.9 } else { 0.0 } };
        let mut input: Vec<f32> = Vec::new();
        for i in 0..88200 {
            let v = tone(i) + click(i);
            input.push(v);
            input.push(v);
        }
        let mut seed = 5u64;
        for _ in 0..88200 {
            let v = (lcg(&mut seed) * 2.0 - 1.0) * 0.3;
            input.push(v);
            input.push(v);
        }
        let blocks: Vec<Vec<f32>> = input.chunks(256).filter(|c| c.len() == 256).map(|c| c.to_vec()).collect();
        let mut sums = [0f64; 4];
        let mut count = 0usize;
        for (n, b) in blocks.iter().enumerate() {
            let out = st.process(&deinterleave(b)).unwrap();
            if n == 0 {
                assert!(out.is_none(), "the first call must return None");
                continue;
            }
            let [d, bs, v, o] = out.expect("every call after the first returns a block");
            let want = &blocks[n - 1];
            let err = (0..256).map(|k| (d[k] + bs[k] + v[k] + o[k] - want[k]).abs()).fold(0f32, f32::max);
            assert!(err < 1e-3, "block {n}: max err {err}");
            for (i, s) in [&d, &bs, &v, &o].iter().enumerate() {
                sums[i] += s.iter().map(|x| (x * x) as f64).sum::<f64>();
            }
            count += 256;
        }
        let rms: Vec<f64> = sums.iter().map(|s| (s / count as f64).sqrt()).collect();
        println!("rms drums {:.4} bass {:.4} vocals {:.4} other {:.4}", rms[0], rms[1], rms[2], rms[3]);
    }

    #[test]
    #[ignore]
    fn stream_keeps_up() {
        let m = models::ensure(&S_FILES, &|s: String| println!("{s}")).unwrap();
        let mut seq_s = Streamer::load(&m, false).unwrap();
        let seq = calibrate(&mut seq_s, 3000).unwrap();
        let mut par_s = Streamer::load(&m, true).unwrap();
        let par = calibrate(&mut par_s, 3000).unwrap();
        let pct = |v: &[f64], p: f64| -> f64 {
            let mut s = v.to_vec();
            s.sort_by(|a, b| a.partial_cmp(b).unwrap());
            s[((s.len() - 1) as f64 * p).round() as usize]
        };
        println!(
            "seq|par p50/p99/max ms: {:.3}/{:.3}/{:.3} | {:.3}/{:.3}/{:.3}",
            pct(&seq, 0.5), pct(&seq, 0.99), pct(&seq, 1.0),
            pct(&par, 0.5), pct(&par, 0.99), pct(&par, 1.0)
        );
        let chosen: &[f64] = if PARALLEL { &par } else { &seq };
        println!(
            "rt = p50/2.902 = {:.3}; stems::cushion_ms = {}",
            pct(chosen, 0.5) / 2.902,
            crate::stems::cushion_ms(chosen, BLOCK_MS)
        );
        assert!(pct(chosen, 0.99) < 2.902, "p99 {:.3} ms is not under the 2.902 ms hop", pct(chosen, 0.99));
    }

    #[test]
    #[ignore]
    fn umx_separates_bass_from_clicks() {
        let m = models::ensure(&U_FILES, &|s: String| println!("{s}")).unwrap();
        let mut sep = Separator::load(&m).unwrap();
        let n = 66150usize;
        let mut x = Vec::with_capacity(n * 2);
        for i in 0..n {
            let t = i as f64 / 44100.0;
            let click = if i % 22050 < 44 { 0.9 } else { 0.0 };
            let v = ((2.0 * std::f64::consts::PI * 110.0 * t).sin() as f32) * 0.4 + click;
            x.push(v);
            x.push(v);
        }
        let t0 = Instant::now();
        let out = sep.separate(&x).unwrap();
        println!("separate ms {:.1}", t0.elapsed().as_secs_f64() * 1000.0);
        println!("ms_per_s {:.3}", sep.ms_per_s().unwrap());
        for s in out.iter() {
            assert_eq!(s.len(), n * 2);
        }
        let rms = |v: &[f32]| -> f64 { (v.iter().map(|a| (a * a) as f64).sum::<f64>() / v.len() as f64).sqrt() };
        let sum: Vec<f32> = (0..n * 2).map(|k| out[0][k] + out[1][k] + out[2][k]).collect();
        assert!(rms(&sum[..]) >= 0.5 * rms(&x[..]), "sum rms {} vs input {}", rms(&sum[..]), rms(&x[..]));
        let band = |v: &[f32]| -> f64 {
            let s = crate::dsp::stft(v);
            let mut e = 0f64;
            for f in 0..s.frames {
                for k in 0..2049 {
                    let hz = k as f64 * 44100.0 / 4096.0;
                    if (50.0..=200.0).contains(&hz) {
                        let i = (f * 2) * 2049 + k;
                        e += (s.re[i].powi(2) + s.im[i].powi(2)) as f64;
                    }
                }
            }
            e
        };
        let (bass, drums) = (band(&out[1][..]), band(&out[0][..]));
        println!("50-200 Hz energy: bass {bass:.1} drums {drums:.1}");
        assert!(bass > drums, "bass energy {bass} is not above drums {drums}");
    }

    #[test]
    #[ignore]
    fn transcriber_finds_a2() {
        let m = models::ensure(&["basic-pitch-nmp.onnx"], &|s: String| println!("{s}")).unwrap();
        let mut tr = Transcriber::load(&m).unwrap();
        let x: Vec<f32> = (0..88200).map(|i| ((2.0 * std::f64::consts::PI * 110.0 * i as f64 / 44100.0).sin() as f32) * 0.4).collect();
        let t0 = Instant::now();
        let ns = tr.notes(&x).unwrap();
        println!("notes ms {:.1}", t0.elapsed().as_secs_f64() * 1000.0);
        assert!(ns.iter().any(|n| (44..=46).contains(&n.pitch) && n.t < 0.2), "{ns:?}");
    }
}
