//! Separators and the transcriber: StemgenRT streaming (Live), Open-Unmix UMX-HQ (Quality), Basic Pitch notes.
#![allow(dead_code, unused_imports, unused_variables)]
use crate::dsp::{self, Spec, BP_RATE, SEP_RATE};
use crate::models::{ModelSet, S_FILES, U_FILES};
use crate::notes::Note;
use ort::session::Session;
use ort::value::Tensor;
use std::time::Instant;

fn e2s<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

/// Deterministic uniform [0, 1) from an LCG (no `rand` crate).
fn noise_lcg(s: &mut u64) -> f32 {
    *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    ((*s >> 33) as f32) / (1u64 << 31) as f32
}

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
    let mut out = vec![0f32; x.len()];
    let half = x.len() / 2;
    for i in 0..half {
        out[i] = x[2 * i];
        out[half + i] = x[2 * i + 1];
    }
    out
}

/// L..., R... -> LRLR...
pub fn interleave(p: &[f32]) -> Vec<f32> {
    let half = p.len() / 2;
    let mut out = vec![0f32; p.len()];
    for i in 0..half {
        out[2 * i] = p[i];
        out[2 * i + 1] = p[half + i];
    }
    out
}

pub struct Streamer {
    session: Session,
    /// In STATES order.
    states: Vec<Vec<f32>>,
    primed: bool,
}

impl Streamer {
    /// Intra threads 1; parallel: parallel execution with 3 inter threads, else 1 inter thread. File S_FILES[0].
    pub fn load(m: &ModelSet, parallel: bool) -> Result<Streamer, String> {
        let b = Session::builder().map_err(e2s)?.with_intra_threads(1).map_err(e2s)?;
        let b = if parallel {
            b.with_parallel_execution(true).map_err(e2s)?.with_inter_threads(3).map_err(e2s)?
        } else {
            b.with_inter_threads(1).map_err(e2s)?
        };
        let session = b.commit_from_file(m.path(S_FILES[0])).map_err(e2s)?;
        let mut s = Streamer { session, states: Vec::new(), primed: false };
        s.reset();
        Ok(s)
    }

    pub fn reset(&mut self) {
        self.states = STATES
            .iter()
            .map(|(_, shape)| vec![0f32; shape.iter().product::<i64>() as usize])
            .collect();
        self.primed = false;
    }

    /// planar.len() == 256. First call after load/reset -> Ok(None); call n returns call n-1's input,
    /// as [drums, bass, vocals, other], each interleaved (256).
    pub fn process(&mut self, planar: &[f32]) -> Result<Option<[Vec<f32>; 4]>, String> {
        if planar.len() != 2 * BLOCK {
            return Err(format!("process wants {} planar samples, got {}", 2 * BLOCK, planar.len()));
        }
        let mut ins: Vec<(String, Tensor<f32>)> = Vec::with_capacity(1 + STATES.len());
        ins.push((
            "audio_chunk".to_string(),
            Tensor::from_array((vec![1i64, 2, BLOCK as i64], planar.to_vec())).map_err(e2s)?,
        ));
        for (i, (name, shape)) in STATES.iter().enumerate() {
            ins.push((
                name.to_string(),
                Tensor::from_array((shape.to_vec(), self.states[i].clone())).map_err(e2s)?,
            ));
        }
        let out = self.session.run(ins).map_err(e2s)?;
        let (_, sep) = out["separated_chunk"].try_extract_tensor::<f32>().map_err(e2s)?;
        if sep.len() != 4 * 2 * BLOCK {
            return Err(format!("separated_chunk has {} values", sep.len()));
        }
        let stems: [Vec<f32>; 4] = std::array::from_fn(|s| interleave(&sep[s * 2 * BLOCK..(s + 1) * 2 * BLOCK]));
        for (i, (name, _)) in STATES.iter().enumerate() {
            let (_, d) = out[format!("next_{name}").as_str()].try_extract_tensor::<f32>().map_err(e2s)?;
            self.states[i] = d.to_vec();
        }
        if !self.primed {
            self.primed = true;
            return Ok(None);
        }
        let [d, b, v, o] = stems;
        Ok(Some([d, b, v, o]))
    }
}

/// Wall ms per call over `calls` calls of LCG noise (0.1), then reset.
pub fn calibrate(s: &mut Streamer, calls: usize) -> Result<Vec<f64>, String> {
    let mut seed = 7u64;
    let mut times = Vec::with_capacity(calls);
    for _ in 0..calls {
        let x: Vec<f32> = (0..2 * BLOCK).map(|_| (noise_lcg(&mut seed) * 2.0 - 1.0) * 0.1).collect();
        let t0 = Instant::now();
        s.process(&x)?;
        times.push(t0.elapsed().as_secs_f64() * 1000.0);
    }
    s.reset();
    Ok(times)
}

// ---- Quality (UMX-HQ) ----

pub struct Separator {
    /// vocals, drums, bass, other (U_FILES[0..4] order).
    sessions: Vec<Session>,
}

/// One UMX net on a magnitude spectrogram [frames,1,2,2049] -> its estimate, same layout.
fn umx_run(sess: &mut Session, mag: &[f32], frames: usize) -> Result<Vec<f32>, String> {
    let t = Tensor::from_array((vec![frames as i64, 1, 2, 2049], mag.to_vec())).map_err(e2s)?;
    let out = sess.run(ort::inputs!["mag" => t]).map_err(e2s)?;
    let (_, est) = out["est"].try_extract_tensor::<f32>().map_err(e2s)?;
    Ok(est.to_vec())
}

impl Separator {
    /// Each session with intra threads 2; files U_FILES[0..4].
    pub fn load(m: &ModelSet) -> Result<Separator, String> {
        let mut sessions = Vec::with_capacity(4);
        for name in &U_FILES[0..4] {
            let s = Session::builder()
                .map_err(e2s)?
                .with_intra_threads(2)
                .map_err(e2s)?
                .commit_from_file(m.path(name))
                .map_err(e2s)?;
            sessions.push(s);
        }
        Ok(Separator { sessions })
    }

    /// Returns [drums, bass, other] (vocals dropped), interleaved stereo at 44.1 kHz.
    pub fn separate(&mut self, pcm44: &[f32]) -> Result<[Vec<f32>; 3], String> {
        let s = dsp::stft(pcm44);
        let n = s.re.len();
        let mag: Vec<f32> = (0..n).map(|i| s.re[i].hypot(s.im[i])).collect();
        let frames = s.frames;
        let mag_ref = &mag;
        let ests: Vec<Vec<f32>> = std::thread::scope(|sc| {
            let handles: Vec<_> = self
                .sessions
                .iter_mut()
                .map(|sess| sc.spawn(move || umx_run(sess, mag_ref, frames)))
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().map_err(|_| "UMX thread panicked".to_string())?)
                .collect::<Result<Vec<_>, String>>()
        })?;
        let mut den = vec![1e-10f32; n];
        for e in &ests {
            for i in 0..n {
                den[i] += e[i] * e[i];
            }
        }
        let len = pcm44.len() / 2;
        let stem = |j: usize| -> Vec<f32> {
            let e = &ests[j];
            let re: Vec<f32> = (0..n).map(|i| s.re[i] * (e[i] * e[i] / den[i])).collect();
            let im: Vec<f32> = (0..n).map(|i| s.im[i] * (e[i] * e[i] / den[i])).collect();
            dsp::istft(&Spec { frames, re, im }, len)
        };
        Ok([stem(1), stem(2), stem(3)])
    }

    /// separate() three times on 2.0 s of LCG stereo (0.3); max ms / 2.0.
    pub fn ms_per_s(&mut self) -> Result<f64, String> {
        let mut seed = 11u64;
        let x: Vec<f32> = (0..2 * 88200).map(|_| (noise_lcg(&mut seed) * 2.0 - 1.0) * 0.3).collect();
        let mut worst = 0f64;
        for _ in 0..3 {
            let t0 = Instant::now();
            self.separate(&x)?;
            worst = worst.max(t0.elapsed().as_secs_f64() * 1000.0);
        }
        Ok(worst / 2.0)
    }
}

// ---- Notes (Basic Pitch) ----
pub const FPS: f64 = 22050.0 / 256.0;
pub const BP_SAMPLES: usize = 43844;
pub const ONSET_THRESH: f32 = 0.5;
pub const FRAME_THRESH: f32 = 0.3;
pub const MIN_NOTE_FRAMES: usize = 11;
pub const ENERGY_TOL: usize = 11;

/// Silence is judged per block, so a short quiet note in an otherwise silent window is not averaged away.
pub const SILENCE_BLOCK: usize = 2048;
pub const SILENCE_RMS: f64 = 1e-3;

/// True when every 46 ms block is below about -60 dBFS RMS. Whole-window RMS would drop a quiet 300 ms note.
pub fn is_silent(mono44: &[f32]) -> bool {
    mono44
        .chunks_exact(SILENCE_BLOCK)
        .all(|b| (b.iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>() / SILENCE_BLOCK as f64).sqrt() < SILENCE_RMS)
}

pub struct Transcriber {
    session: Session,
}

impl Transcriber {
    /// File "basic-pitch-nmp.onnx".
    pub fn load(m: &ModelSet) -> Result<Transcriber, String> {
        let session = Session::builder().map_err(e2s)?.commit_from_file(m.path("basic-pitch-nmp.onnx")).map_err(e2s)?;
        Ok(Transcriber { session })
    }

    /// Mono at 44.1 kHz in; notes with t relative to the start of the input.
    pub fn notes(&mut self, mono44: &[f32]) -> Result<Vec<Note>, String> {
        // The model normalises its input, so near-silence (below about -60 dBFS RMS) looks like full-scale
        // noise to it and turns into notes. Treat it as silence.
        if is_silent(mono44) {
            return Ok(Vec::new());
        }
        // Mono -> stereo -> 22.05 kHz, then one channel.
        let mut stereo = Vec::with_capacity(mono44.len() * 2);
        for &v in mono44 {
            stereo.push(v);
            stereo.push(v);
        }
        let rs = dsp::resample(&stereo, SEP_RATE, BP_RATE);
        let mono: Vec<f32> = rs.iter().step_by(2).copied().collect();
        let mut x = vec![0f32; BP_SAMPLES];
        let take = mono.len().min(BP_SAMPLES);
        x[BP_SAMPLES - take..].copy_from_slice(&mono[mono.len() - take..]);

        let in_name = self.session.inputs[0].name.clone();
        let out_names: Vec<String> = self.session.outputs.iter().map(|o| o.name.clone()).collect();
        let t = Tensor::from_array((vec![1i64, BP_SAMPLES as i64, 1], x)).map_err(e2s)?;
        let out = self.session.run(ort::inputs![in_name.as_str() => t]).map_err(e2s)?;

        let (ni, oi) = pick_grids(&out_names)?;
        let grid = |i: usize| -> Result<Vec<Vec<f32>>, String> {
            let (shape, data) = out[out_names[i].as_str()].try_extract_tensor::<f32>().map_err(e2s)?;
            if shape.len() != 3 || shape[1] != 172 || shape[2] != 88 {
                return Err(format!("unexpected Basic Pitch output {} shape {:?}", out_names[i], shape));
            }
            Ok(data.chunks(88).map(|c| c.to_vec()).collect())
        };
        let note = grid(ni)?;
        let onset = grid(oi)?;
        Ok(frames_to_notes(&note, &onset))
    }
}

/// Indices (note, onset) into the model's output names. The outputs are fixed: `StatefulPartitionedCall:0`
/// is the contour [1,172,264] (unused), `:1` is `note`, `:2` is `onset`. No guessing from the data.
pub fn pick_grids(names: &[String]) -> Result<(usize, usize), String> {
    let find = |n: &str| names.iter().position(|x| x == n);
    match (find("StatefulPartitionedCall:1"), find("StatefulPartitionedCall:2")) {
        (Some(note), Some(onset)) => Ok((note, onset)),
        _ => Err(format!("unexpected Basic Pitch outputs: {names:?}")),
    }
}

/// Port of basic-pitch `get_infered_onsets` (n_diff = 2): where the note grid rises over the last two
/// frames, raise the onset grid to the rise scaled to the global onset maximum.
pub fn infer_onsets(note: &[Vec<f32>], onset: &[Vec<f32>]) -> Vec<Vec<f32>> {
    const N_DIFF: usize = 2;
    let frames = note.len();
    let mut out: Vec<Vec<f32>> = onset.to_vec();
    if frames == 0 {
        return out;
    }
    let pitches = note[0].len();
    let at = |f: usize, n: usize, p: usize| -> f32 { if f >= n { note[f - n][p] } else { 0.0 } };
    let mut diff = vec![vec![0f32; pitches]; frames];
    let mut dmax = 0f32;
    for f in N_DIFF..frames {
        for p in 0..pitches {
            let d = (1..=N_DIFF).map(|n| note[f][p] - at(f, n, p)).fold(f32::INFINITY, f32::min).max(0.0);
            diff[f][p] = d;
            dmax = dmax.max(d);
        }
    }
    if dmax <= 0.0 {
        return out;
    }
    let omax = onset.iter().flat_map(|r| r.iter().copied()).fold(0f32, f32::max);
    for f in 0..frames {
        for p in 0..pitches {
            out[f][p] = out[f][p].max(omax * diff[f][p] / dmax);
        }
    }
    out
}

/// note and onset are [frame][pitch] grids (172 x 88).
pub fn frames_to_notes(note: &[Vec<f32>], onset: &[Vec<f32>]) -> Vec<Note> {
    let frames = note.len();
    if frames == 0 {
        return Vec::new();
    }
    let pitches = note[0].len();
    let mut work: Vec<Vec<f32>> = note.to_vec();
    let inferred = infer_onsets(note, onset);
    let onset = &inferred[..];

    // Onset candidates: at or above the threshold and a local maximum in time.
    let mut cands: Vec<(f32, usize, usize)> = Vec::new();
    for f in 0..frames {
        for p in 0..pitches {
            let v = onset[f][p];
            if v < ONSET_THRESH {
                continue;
            }
            let prev = if f > 0 { onset[f - 1][p] } else { f32::NEG_INFINITY };
            let next = if f + 1 < frames { onset[f + 1][p] } else { f32::NEG_INFINITY };
            if v >= prev && v >= next {
                cands.push((v, f, p));
            }
        }
    }
    // deterministic: strongest first, then earliest frame, then lowest pitch
    cands.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));

    let mut out: Vec<Note> = Vec::new();
    for &(_, f, p) in &cands {
        if work[f][p] < FRAME_THRESH {
            continue;
        }
        // Walk forward, tolerating up to ENERGY_TOL quiet frames in a row.
        let mut k = 0usize;
        let mut end = f;
        let mut fp = f + 1;
        loop {
            if fp >= frames {
                end = frames - k;
                break;
            }
            if work[fp][p] < FRAME_THRESH {
                k += 1;
            } else {
                k = 0;
            }
            if k > ENERGY_TOL {
                end = fp + 1 - k;
                break;
            }
            fp += 1;
        }
        // upstream: `if i - note_start_idx <= min_note_len: continue`, so a note needs at least 12 frames
        if end - f <= MIN_NOTE_FRAMES {
            continue;
        }
        let mean = work[f..end].iter().map(|r| r[p]).sum::<f32>() / (end - f) as f32;
        // a note's note grid must average at least the frame threshold (velocity >= 38)
        if mean < FRAME_THRESH {
            continue;
        }
        let vel = (127.0 * mean).round().clamp(0.0, 127.0) as u8;
        out.push(Note { t: f as f64 / FPS, dur: (end - f) as f64 / FPS, pitch: (p + 21) as u8, vel });
        for r in work[f..end].iter_mut() {
            r[p] = 0.0;
        }
    }
    out.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal).then(a.pitch.cmp(&b.pitch)));
    out
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
        // 1 s silence, then 1 s of A2. Sweep the attack over one model hop (256 samples at 22.05 kHz is 512 at
        // 44.1 kHz). The model window starts 256/22050 s in; the note lands up to ~0.07 s late (CQT latency)
        // and never more than one frame early.
        let mut bad = Vec::new();
        for shift in (0..512usize).step_by(64) {
            let attack = 44100 + shift;
            let x: Vec<f32> = (0..88200)
                .map(|i| if i < attack { 0.0 } else { ((2.0 * std::f64::consts::PI * 110.0 * i as f64 / 44100.0).sin() as f32) * 0.4 })
                .collect();
            let t0 = Instant::now();
            let ns = tr.notes(&x).unwrap();
            let expect = attack as f64 / 44100.0 - 256.0 / 22050.0;
            let lags: Vec<f64> = ns.iter().filter(|n| (44..=46).contains(&n.pitch)).map(|n| n.t - expect).collect();
            println!("shift {shift:3} expect {expect:.4} lag(s) {lags:?} ({:.1} ms)", t0.elapsed().as_secs_f64() * 1000.0);
            if !lags.iter().any(|l| (-1.0 / FPS..=0.10).contains(l)) {
                bad.push((shift, lags));
            }
        }
        assert!(bad.is_empty(), "no A2 note within [-1/FPS, 0.10] s of the attack at (shift, lags): {bad:?}");
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn pick_grids_by_name() {
        let n = names(&["StatefulPartitionedCall:0", "StatefulPartitionedCall:1", "StatefulPartitionedCall:2"]);
        assert_eq!(pick_grids(&n), Ok((1, 2)));
        // the same names in another order: note is still `:1`, onset `:2`
        let n = names(&["StatefulPartitionedCall:2", "StatefulPartitionedCall:0", "StatefulPartitionedCall:1"]);
        assert_eq!(pick_grids(&n), Ok((2, 0)));
        let n = names(&["StatefulPartitionedCall:1", "StatefulPartitionedCall:2", "StatefulPartitionedCall:0"]);
        assert_eq!(pick_grids(&n), Ok((0, 1)));
    }

    #[test]
    fn pick_grids_missing_name_is_error() {
        let n = names(&["StatefulPartitionedCall:0", "StatefulPartitionedCall:1"]);
        let e = pick_grids(&n).unwrap_err();
        assert!(e.contains("unexpected Basic Pitch outputs"), "{e}");
        assert!(pick_grids(&[]).is_err());
    }

    #[test]
    fn frames_to_notes_infers_onset_from_note_rise() {
        let mut note = vec![vec![0f32; 88]; 172];
        let mut onset = vec![vec![0f32; 88]; 172];
        // a strong note with a real onset peak: sets the global onset max
        for f in 100..130 {
            note[f][30] = 0.9;
        }
        onset[100][30] = 0.9;
        // a note whose onset grid never passes 0.3: only the rise of the note grid shows it
        for f in 20..60 {
            note[f][48] = 0.9;
        }
        for f in 0..172 {
            onset[f][48] = 0.3f32.min(onset[f][48] + 0.1);
        }
        let ns = frames_to_notes(&note, &onset);
        assert_eq!(ns.len(), 2, "{ns:?}");
        assert!(ns.iter().any(|n| n.pitch == 51), "{ns:?}");
        let n = ns.iter().find(|n| n.pitch == 69).unwrap_or_else(|| panic!("no pitch 69: {ns:?}"));
        assert!((n.t - 20.0 / FPS).abs() < 3.0 / FPS, "{ns:?}");
    }

    /// F18 (breaker r3 B1): note grid noise in [0, 0.35), onset noise in [0, 0.45) and one real onset peak
    /// of 0.6 elsewhere. No pitch has a real attack, so there must be no notes.
    #[test]
    fn r3_noise_produces_no_notes() {
        let mut total = 0usize;
        let mut worst = Vec::new();
        for seed in 1..=20u64 {
            let mut x = seed * 7919;
            let mut note = vec![vec![0f32; 88]; 172];
            let mut onset = vec![vec![0f32; 88]; 172];
            for f in 0..172 {
                for p in 0..88 {
                    note[f][p] = lcg(&mut x) * 0.35;
                    onset[f][p] = lcg(&mut x) * 0.45;
                }
            }
            for f in 0..172 {
                note[f][10] = 0.0;
            }
            onset[100][10] = 0.6;
            let ns = frames_to_notes(&note, &onset);
            if !ns.is_empty() {
                total += ns.len();
                worst.push((seed, ns.len(), ns.first().cloned()));
            }
        }
        assert_eq!(total, 0, "spurious notes on noise: {worst:?}");
    }

    /// F18: upstream's minimum note length is 11 frames, and a note must be longer than that (>= 12).
    #[test]
    fn frames_to_notes_upstream_min_len() {
        for (len, want) in [(11usize, 0usize), (12, 1)] {
            let mut note = vec![vec![0f32; 88]; 172];
            let mut onset = vec![vec![0f32; 88]; 172];
            for f in 40..40 + len {
                note[f][30] = 0.9;
            }
            onset[40][30] = 0.9;
            let ns = frames_to_notes(&note, &onset);
            assert_eq!(ns.len(), want, "len {len}: {ns:?}");
        }
    }

    /// F18 (ignored, CPU, model): report-only, white noise note counts at amplitude 0.05 and 0.2.
    #[test]
    #[ignore]
    fn r3_model_white_noise_no_notes() {
        let m = models::ensure(&["basic-pitch-nmp.onnx"], &|s: String| println!("{s}")).unwrap();
        let mut tr = Transcriber::load(&m).unwrap();
        let mut x = 12345u64;
        let mut bad = Vec::new();
        for amp in [0.05f32, 0.2] {
            let noise: Vec<f32> = (0..88200).map(|_| (lcg(&mut x) * 2.0 - 1.0) * amp).collect();
            let ns = tr.notes(&noise).unwrap();
            println!("white noise amp {amp}: {} notes, first {:?}", ns.len(), ns.first());
            if !ns.is_empty() {
                bad.push((amp, ns.len()));
            }
        }
        let tone: Vec<f32> = (0..88200).map(|i| ((2.0 * std::f64::consts::PI * 220.0 * i as f64 / 44100.0).sin() as f32) * 0.4).collect();
        let ns = tr.notes(&tone).unwrap();
        println!("steady 220 Hz tone: {} notes, first {:?}", ns.len(), ns.first());
        // report-only: the model's own output on white noise is not a gate (see transcriber_silence_no_notes)
        println!("white noise notes (amp, count): {bad:?}");
    }

    /// F18: a note's note grid must average at least FRAME_THRESH. A 13-frame blob alternating 0.32 / 0.05
    /// (7 voiced of 13, so it passes a half-density rule) with an onset peak at its first frame is no note.
    #[test]
    fn frames_to_notes_sparse_note_dropped() {
        let mut note = vec![vec![0f32; 88]; 172];
        let mut onset = vec![vec![0f32; 88]; 172];
        for k in 0..13 {
            note[40 + k][30] = if k % 2 == 0 { 0.32 } else { 0.05 };
        }
        onset[40][30] = 0.9;
        let ns = frames_to_notes(&note, &onset);
        assert!(ns.is_empty(), "{ns:?}");
    }

    /// F18 (ignored, CPU, model): digital silence and about -60 dBFS noise are not music.
    #[test]
    #[ignore]
    fn transcriber_silence_no_notes() {
        let m = models::ensure(&["basic-pitch-nmp.onnx"], &|s: String| println!("{s}")).unwrap();
        let mut tr = Transcriber::load(&m).unwrap();
        let ns = tr.notes(&vec![0f32; 88200]).unwrap();
        println!("silence: {} notes {ns:?}", ns.len());
        let mut x = 777u64;
        let noise: Vec<f32> = (0..88200).map(|_| (lcg(&mut x) * 2.0 - 1.0) * 0.001).collect();
        let nn = tr.notes(&noise).unwrap();
        println!("noise +-0.001: {} notes {nn:?}", nn.len());
        assert!(ns.is_empty(), "notes on silence: {ns:?}");
        assert!(nn.is_empty(), "notes on -60 dBFS noise: {nn:?}");
    }

    /// F23: a short quiet note in silence is signal. 110 Hz at 0.0035 over samples 37 485..50 715 (300 ms).
    #[test]
    fn is_silent_quiet_note_in_silence_is_signal() {
        let mut x = vec![0f32; 88200];
        for i in 37485..50715 {
            x[i] = ((2.0 * std::f64::consts::PI * 110.0 * i as f64 / 44100.0).sin() as f32) * 0.0035;
        }
        assert!(!is_silent(&x), "quiet note in silence treated as silent");
    }

    /// F23: pins the floor. Zeros and +-0.001 noise are silent, +-0.004 noise is not.
    #[test]
    fn is_silent_floor_noise_is_silent() {
        assert!(is_silent(&vec![0f32; 88200]));
        let noise = |amp: f32| -> Vec<f32> {
            let mut s = 777u64;
            (0..88200).map(|_| (lcg(&mut s) * 2.0 - 1.0) * amp).collect()
        };
        assert!(is_silent(&noise(0.001)));
        assert!(!is_silent(&noise(0.004)));
    }

    /// F23 (ignored, CPU, model): a real 300 ms A2 at 0.0035 (about -49 dBFS peak) in silence must survive the
    /// silence gate. Cherry-picked from breaker_r4::r4_model_quiet_short_note_survives_rms_gate (d318a9c),
    /// simplified to use Transcriber::notes only.
    #[test]
    #[ignore]
    fn r4_model_quiet_short_note_survives_rms_gate() {
        let m = models::ensure(&["basic-pitch-nmp.onnx"], &|_: String| {}).unwrap();
        let mut tr = Transcriber::load(&m).unwrap();
        let f0 = 440.0 * 2f64.powf((45.0 - 69.0) / 12.0);
        let render = |amp: f32| -> Vec<f32> {
            let mut x = vec![0f32; 88200];
            let i0 = 44100usize;
            let i1 = i0 + (0.3 * 44100.0) as usize;
            for i in i0..i1 {
                let tt = (i - i0) as f64 / 44100.0;
                x[i] = ((2.0 * std::f64::consts::PI * f0 * tt).sin() as f32) * amp;
            }
            x
        };
        let seen = |ns: &[Note]| ns.iter().any(|n| (44..=46).contains(&n.pitch) && (0.9..1.4).contains(&n.t));
        let loud = tr.notes(&render(0.4)).unwrap();
        let quiet = tr.notes(&render(0.0035)).unwrap();
        println!("loud {loud:?}\nquiet {quiet:?}");
        assert!(seen(&loud), "precondition: model should see the loud note: {loud:?}");
        assert!(seen(&quiet), "silence gate dropped a real 300 ms A2 at -49 dBFS peak: {quiet:?}");
    }
}
