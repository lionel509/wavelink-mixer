//! Breaker round 4 (fix round 3: F17 to F19). Test-only, never production code.
//!
//! `old_frames_to_notes` is `frames_to_notes` as of 4959b42 (before F18): minimum 5 frames, no mean floor.
//! `Rig::run` replays the production front end on the real model without the RMS gate, so the table can
//! show what the gate and the F18 rules each drop.
use crate::dsp::{self, BP_RATE, SEP_RATE};
use crate::models;
use crate::notes::Note;
use crate::sep::{frames_to_notes, infer_onsets, pick_grids, Transcriber, BP_SAMPLES, ENERGY_TOL, FPS, FRAME_THRESH, ONSET_THRESH};
use ort::session::Session;
use ort::value::Tensor;
use std::f64::consts::PI;

const RATE: f64 = 44100.0;
const WIN: usize = 88200;

fn lcg(s: &mut u64) -> f32 {
    *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    ((*s >> 33) as f32) / (1u64 << 31) as f32
}

fn grids0() -> (Vec<Vec<f32>>, Vec<Vec<f32>>) {
    (vec![vec![0f32; 88]; 172], vec![vec![0f32; 88]; 172])
}

/// frames_to_notes as of 4959b42: `end - f < 5`, no mean floor.
fn old_frames_to_notes(note: &[Vec<f32>], onset: &[Vec<f32>]) -> Vec<Note> {
    const OLD_MIN: usize = 5;
    let frames = note.len();
    if frames == 0 {
        return Vec::new();
    }
    let pitches = note[0].len();
    let mut work: Vec<Vec<f32>> = note.to_vec();
    let inferred = infer_onsets(note, onset);
    let onset = &inferred[..];
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
    cands.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    let mut out: Vec<Note> = Vec::new();
    for &(_, f, p) in &cands {
        if work[f][p] < FRAME_THRESH {
            continue;
        }
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
        if end - f < OLD_MIN {
            continue;
        }
        let mean = work[f..end].iter().map(|r| r[p]).sum::<f32>() / (end - f) as f32;
        let vel = (127.0 * mean).round().clamp(0.0, 127.0) as u8;
        out.push(Note { t: f as f64 / FPS, dur: (end - f) as f64 / FPS, pitch: (p + 21) as u8, vel });
        for r in work[f..end].iter_mut() {
            r[p] = 0.0;
        }
    }
    out.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal).then(a.pitch.cmp(&b.pitch)));
    out
}

// ---------- signals ----------

#[derive(Clone, Copy)]
struct Part {
    midi: f64,
    amp: f32,
    start: f64,
    dur: f64,
    tau: f64,
    vib: f64,
}

fn part(midi: f64, amp: f32, start: f64, dur: f64) -> Part {
    Part { midi, amp, start, dur, tau: 0.0, vib: 0.0 }
}

fn render(total: usize, parts: &[Part]) -> Vec<f32> {
    let mut x = vec![0f32; total];
    for p in parts {
        let f0 = 440.0 * 2f64.powf((p.midi - 69.0) / 12.0);
        let eps = 2f64.powf(p.vib / 1200.0) - 1.0;
        let fm = 5.5;
        let i0 = (p.start * RATE) as usize;
        let i1 = (((p.start + p.dur) * RATE) as usize).min(total);
        for i in i0..i1 {
            let tt = i as f64 / RATE - p.start;
            let phase = if p.vib != 0.0 {
                2.0 * PI * f0 * (tt + eps / (2.0 * PI * fm) * (1.0 - (2.0 * PI * fm * tt).cos()))
            } else {
                2.0 * PI * f0 * tt
            };
            let env = if p.tau > 0.0 { (-tt / p.tau).exp() } else { 1.0 };
            x[i] += (phase.sin() * env) as f32 * p.amp;
        }
    }
    x
}

fn white(total: usize, amp: f32, seed: u64) -> Vec<f32> {
    let mut s = seed;
    (0..total).map(|_| (lcg(&mut s) * 2.0 - 1.0) * amp).collect()
}

fn rms(x: &[f32]) -> f64 {
    (x.iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>() / x.len().max(1) as f64).sqrt()
}

// ---------- model rig ----------

struct Rig {
    tr: Transcriber,
    raw: Session,
}

fn rig() -> Rig {
    let m = models::ensure(&["basic-pitch-nmp.onnx"], &|_: String| {}).unwrap();
    let tr = Transcriber::load(&m).unwrap();
    let raw = Session::builder().unwrap().commit_from_file(m.path("basic-pitch-nmp.onnx")).unwrap();
    Rig { tr, raw }
}

/// Front end of Transcriber::notes without the RMS gate. Returns (note, onset) grids.
fn grids(sess: &mut Session, mono44: &[f32]) -> (Vec<Vec<f32>>, Vec<Vec<f32>>) {
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
    let in_name = sess.inputs[0].name.clone();
    let out_names: Vec<String> = sess.outputs.iter().map(|o| o.name.clone()).collect();
    let (ni, oi) = pick_grids(&out_names).unwrap();
    let t = Tensor::from_array((vec![1i64, BP_SAMPLES as i64, 1], x)).unwrap();
    let out = sess.run(ort::inputs![in_name.as_str() => t]).unwrap();
    let grid = |i: usize| -> Vec<Vec<f32>> {
        let (shape, data) = out[out_names[i].as_str()].try_extract_tensor::<f32>().unwrap();
        assert!(shape.len() == 3 && shape[1] == 172 && shape[2] == 88, "shape {:?}", shape);
        data.chunks(88).map(|c| c.to_vec()).collect()
    };
    (grid(ni), grid(oi))
}

struct Out {
    rms: f64,
    prod: Vec<Note>,
    gated: Vec<Note>,
    ungated: Vec<Note>,
    old: Vec<Note>,
}

fn run(r: &mut Rig, x: &[f32]) -> Out {
    let level = rms(x);
    let prod = r.tr.notes(x).unwrap();
    let (g, o) = grids(&mut r.raw, x);
    let ungated = frames_to_notes(&g, &o);
    let old = old_frames_to_notes(&g, &o);
    let gated = if level < 1e-3 { Vec::new() } else { ungated.clone() };
    assert_eq!(format!("{prod:?}"), format!("{gated:?}"), "replica drifted from Transcriber::notes");
    Out { rms: level, prod, gated, ungated, old }
}

fn fmt(ns: &[Note]) -> String {
    if ns.is_empty() {
        return "-".into();
    }
    ns.iter().map(|n| format!("p{}@{:.2}/{:.0}ms", n.pitch, n.t, n.dur * 1000.0)).collect::<Vec<_>>().join(" ")
}


// ---------- probes: pure, fast ----------

/// Probe: the recall-vs-noise grid for F18 on synthetic grids. Noise ceiling and onset peak are swept, plus
/// a soft real note (note grid 0.45 for 30 frames, onset peak 0.5) inside the noise. Print only, except the
/// B1 region the review pinned (ceiling 0.35, peak 0.6 at most) must give no notes.
#[test]
fn r4_pure_noise_recall_grid() {
    let mut b1_bad = Vec::new();
    println!("ceiling omax | noise notes (10 seeds, new rules) | soft note recall (10 seeds, grid 0.45 + onset 0.5)");
    for ceil in [0.30f32, 0.35, 0.40, 0.45, 0.50] {
        for omax in [0.5f32, 0.6, 0.8, 1.0] {
            let mut noise_notes = 0usize;
            let mut recall = 0usize;
            let mut old_recall = 0usize;
            let mut old_noise = 0usize;
            for seed in 1..=10u64 {
                let mut x = seed * 7919;
                let (mut note, mut onset) = grids0();
                for f in 0..172 {
                    for p in 0..88 {
                        note[f][p] = lcg(&mut x) * ceil;
                        onset[f][p] = lcg(&mut x) * 0.45;
                    }
                }
                for f in 0..172 {
                    note[f][10] = 0.0;
                }
                onset[100][10] = omax;
                noise_notes += frames_to_notes(&note, &onset).len();
                old_noise += old_frames_to_notes(&note, &onset).len();
                // soft real note at pitch 50 inside the same noise
                let (mut n2, mut o2) = (note.clone(), onset.clone());
                for f in 60..90 {
                    n2[f][50] = 0.45;
                }
                o2[60][50] = 0.5;
                let hit = |ns: &[Note]| ns.iter().any(|n| n.pitch == 71 && n.t >= 60.0 / 86.13 - 0.05 && n.t <= 60.0 / 86.13 + 0.05);
                if hit(&frames_to_notes(&n2, &o2)) {
                    recall += 1;
                }
                if hit(&old_frames_to_notes(&n2, &o2)) {
                    old_recall += 1;
                }
            }
            println!("{ceil:.2} {omax:.1} | noise notes new {noise_notes} old {old_noise} | soft recall new {recall}/10 old {old_recall}/10");
            if ceil <= 0.35 && omax <= 0.6 && noise_notes > 0 {
                b1_bad.push((ceil, omax, noise_notes));
            }
        }
    }
    assert!(b1_bad.is_empty(), "noise produced notes inside the B1 region (ceiling, omax, notes): {b1_bad:?}");
}

/// Probe: a note with a dense, real-looking grid at exactly 12 frames is kept, 11 is dropped (F18 rule).
#[test]
fn r4_pure_min_len_boundary() {
    for (len, want) in [(11usize, 0usize), (12, 1), (13, 1)] {
        let (mut note, mut onset) = grids0();
        for f in 40..40 + len {
            note[f][30] = 0.9;
        }
        onset[40][30] = 0.9;
        let ns = frames_to_notes(&note, &onset);
        println!("len {len}: {ns:?}");
        assert_eq!(ns.len(), want, "len {len}: {ns:?}");
    }
}

/// Probe: a soft held note whose grid stays at 0.31 (just over FRAME_THRESH) for 20 frames must survive the mean
/// floor, and so must a 0.31 note with a short 3-frame dip (bridged by ENERGY_TOL).
#[test]
fn r4_pure_soft_note_survives_mean_floor() {
    let (mut note, mut onset) = grids0();
    for f in 40..60 {
        note[f][30] = 0.31;
    }
    onset[40][30] = 0.9;
    let a = frames_to_notes(&note, &onset);
    println!("soft flat: {a:?}");
    assert_eq!(a.len(), 1, "{a:?}");
    let (mut note, mut onset) = grids0();
    for f in 40..70 {
        note[f][30] = if (45..48).contains(&f) { 0.05 } else { 0.31 };
    }
    onset[40][30] = 0.9;
    let b = frames_to_notes(&note, &onset);
    println!("soft dip: {b:?}");
    assert_eq!(b.len(), 1, "{b:?}");
}

// ---------- model probes (ignored: CPU, real models) ----------

/// Report: the recall-vs-noise table on the real model. Prints one row per case with old (4959b42 rules),
/// ungated (F18 rules, no RMS gate) and prod (the shipped Transcriber::notes). Asserts only the replica check.
#[test]
#[ignore]
fn r4_model_recall_vs_noise_table() {
    let mut r = rig();
    let mut cases: Vec<(String, Vec<f32>, &str)> = Vec::new();
    // sustained, 1 s at 0.5 s in a 2 s window, amplitude 0.4
    for (name, m) in [("E1", 28.0), ("A1", 33.0), ("A2", 45.0), ("A3", 57.0), ("A4", 69.0)] {
        cases.push((format!("sustain {name} a0.4"), render(WIN, &[part(m, 0.4, 0.5, 1.0)]), "1"));
    }
    for amp in [0.1f32, 0.02, 0.0025, 0.0021, 0.0019, 0.0015] {
        cases.push((format!("sustain A2 a{amp}"), render(WIN, &[part(45.0, amp, 0.5, 1.0)]), "1"));
    }
    // short A2 notes at 0.4
    for d in [0.10f64, 0.12, 0.14, 0.16, 0.20, 0.25, 0.30] {
        cases.push((format!("short A2 {:.0}ms a0.4", d * 1000.0), render(WIN, &[part(45.0, 0.4, 0.5, d)]), "1"));
    }
    // short A2 in silence, quiet: the RMS gate sees the whole 2 s window
    for amp in [0.004f32, 0.0045, 0.002] {
        cases.push((format!("short A2 200ms a{amp} in silence"), render(WIN, &[part(45.0, amp, 1.0, 0.2)]), "1"));
    }
    // decaying plucks
    for (name, m, tau) in [("A2", 45.0, 0.15), ("A2", 45.0, 0.3), ("A2", 45.0, 0.6), ("E1", 28.0, 0.3), ("A3", 57.0, 0.15)] {
        let mut p = part(m, 0.4, 0.5, 1.0);
        p.tau = tau;
        cases.push((format!("pluck {name} tau{tau}"), render(WIN, &[p]), "1"));
    }
    // chord and vibrato
    cases.push(("chord A2+E3+A3 a0.3".into(), render(WIN, &[part(45.0, 0.3, 0.5, 1.0), part(52.0, 0.3, 0.5, 1.0), part(57.0, 0.3, 0.5, 1.0)]), "3"));
    let mut v3 = part(57.0, 0.4, 0.5, 1.0);
    v3.vib = 30.0;
    cases.push(("vibrato A3 +-30c".into(), render(WIN, &[v3]), "1"));
    let mut v2 = part(45.0, 0.4, 0.5, 1.0);
    v2.vib = 50.0;
    cases.push(("vibrato A2 +-50c".into(), render(WIN, &[v2]), "1"));
    // repeated A2 eighths: 120 bpm (IOI 250 ms, dur 200 ms), 160 bpm (IOI 187 ms, dur 150 ms), 16ths at 160 (IOI 94 ms, dur 80 ms)
    for (name, ioi, dur, n) in [("8ths 120bpm", 0.25f64, 0.2f64, 4usize), ("8ths 160bpm", 0.1875, 0.15, 6), ("16ths 160bpm", 0.09375, 0.08, 8)] {
        let parts: Vec<Part> = (0..n).map(|k| part(45.0, 0.4, 0.3 + k as f64 * ioi, dur)).collect();
        cases.push((format!("repeat A2 {name} x{n}"), render(WIN, &parts), if n == 4 { "4" } else if n == 6 { "6" } else { "8" }));
    }
    // quiet passage: bleed at about -70 dBFS, one 500 ms A2 at 0.005 (about -49 dBFS peak)
    let bleed = white(WIN, 0.0005, 99);
    let qnote = render(WIN, &[part(45.0, 0.005, 1.0, 0.5)]);
    let quiet: Vec<f32> = bleed.iter().zip(qnote.iter()).map(|(a, b)| a + b).collect();
    cases.push(("quiet passage: bleed + 500ms A2 a0.005".into(), quiet, "1"));
    // loud click in silence
    let mut click = vec![0f32; WIN];
    for i in 0..220 {
        click[44100 + i] = 0.5 * (1.0 - i as f32 / 220.0);
    }
    cases.push(("click 5ms a0.5 in silence".into(), click, "0"));
    // noise and silence
    for amp in [0.4f32, 0.05, 0.005, 0.002, 0.001, 0.0005] {
        cases.push((format!("white noise a{amp}"), white(WIN, amp, 12345), "0"));
    }
    cases.push(("silence (zeros)".into(), vec![0f32; WIN], "0"));

    println!("| case | RMS | want | old (4959b42) | ungated (F18) | prod (gated) | prod notes |");
    println!("|---|---|---|---|---|---|---|");
    for (name, x, want) in &cases {
        let o = run(&mut r, x);
        println!("| {name} | {:.5} | {want} | {} {} | {} {} | {} {} | {} |", o.rms, o.old.len(), "", o.ungated.len(), "", o.gated.len(), "", fmt(&o.prod));
    }
}

/// Breaker (pure): a passage of six real A2 notes at 160 bpm eighths, each 10 frames at grid 0.33 with 6-frame
/// dips to 0.15 between them. ENERGY_TOL bridges the dips, so they become one long note whose mean is about
/// 0.27. The mean floor (velocity >= 38) drops it, and 4959b42 kept it. Expected: at least one note in the run.
#[test]
fn r4_pure_bridged_repeated_notes_keep_a_note() {
    let (mut note, mut onset) = grids0();
    for k in 0..6 {
        let s = 40 + k * 16;
        for f in s..s + 10 {
            note[f][30] = 0.33;
        }
        for f in s + 10..s + 16 {
            note[f][30] = 0.15;
        }
        onset[s][30] = 0.9;
    }
    let new = frames_to_notes(&note, &onset);
    let old = old_frames_to_notes(&note, &onset);
    println!("new {new:?}\nold {old:?}");
    assert!(!new.is_empty(), "six real notes in a bridged run give no notes (old kept {} note(s): {old:?})", old.len());
}

/// Breaker (ignored, model): the RMS gate is judged on the whole 2 s window, not on the note. A real 300 ms A2 at
/// 0.0035 (about -49 dBFS peak) in otherwise silent audio is the note the model hears at 0.4. It must survive.
#[test]
#[ignore]
fn r4_model_quiet_short_note_survives_rms_gate() {
    let mut r = rig();
    let loud = run(&mut r, &render(WIN, &[part(45.0, 0.4, 1.0, 0.3)]));
    let quiet = run(&mut r, &render(WIN, &[part(45.0, 0.0035, 1.0, 0.3)]));
    let seen = |ns: &[Note]| ns.iter().any(|n| (44..=46).contains(&n.pitch) && (0.9..1.4).contains(&n.t));
    println!("loud (a0.4) prod {}", fmt(&loud.prod));
    println!("quiet (a0.0035) window RMS {:.6}, ungated {}, prod {}", quiet.rms, fmt(&quiet.ungated), fmt(&quiet.prod));
    assert!(seen(&loud.prod), "precondition: model should see the loud note: {:?}", loud.prod);
    assert!(seen(&quiet.ungated), "precondition: model should see the quiet note without the gate: {:?}", quiet.ungated);
    assert!(seen(&quiet.prod), "RMS gate dropped a real 300 ms A2 at -49 dBFS peak (window RMS {:.6} < 1e-3): prod {:?}", quiet.rms, quiet.prod);
}



/// Breaker (ignored, model): a real 200 ms A2 at 0.4 (an eighth at about 150 bpm). 4959b42 kept it (about 104 ms
/// of note grid). The 12-frame minimum drops it. Expected: one A2 note near 0.5 s.
#[test]
#[ignore]
fn r4_model_200ms_a2_kept() {
    let mut r = rig();
    let o = run(&mut r, &render(WIN, &[part(45.0, 0.4, 0.5, 0.2)]));
    println!("old {} prod {}", fmt(&o.old), fmt(&o.prod));
    let seen = |ns: &[Note]| ns.iter().any(|n| (44..=46).contains(&n.pitch) && (0.4..0.8).contains(&n.t));
    assert!(seen(&o.prod), "200 ms A2 at 0.4 gives no note (old rules: {}; new ungated: {})", fmt(&o.old), fmt(&o.ungated));
}

/// Breaker (ignored, model): six real A2 eighths at 160 bpm (150 ms each, 187 ms apart) in one 1.3 s passage.
/// 4959b42 gave one merged A2 note. The mean floor drops it. Expected: at least one A2 note in the passage.
#[test]
#[ignore]
fn r4_model_fast_eighths_keep_a2() {
    let mut r = rig();
    let parts: Vec<Part> = (0..6).map(|k| part(45.0, 0.4, 0.3 + k as f64 * 0.1875, 0.15)).collect();
    let o = run(&mut r, &render(WIN, &parts));
    println!("old {} new(ungated) {} prod {}", fmt(&o.old), fmt(&o.ungated), fmt(&o.prod));
    let seen = |ns: &[Note]| ns.iter().any(|n| (44..=46).contains(&n.pitch) && (0.2..1.5).contains(&n.t));
    assert!(seen(&o.prod), "six real A2 eighths at 160 bpm give no A2 note (old rules: {})", fmt(&o.old));
}

/// Breaker (ignored, model): the same A2 phase sweep as transcriber_finds_a2, but counting. F19 says one A2
/// note per attack; any extra A2-band note is a spurious note that the `any` in that test would hide.
#[test]
#[ignore]
fn r4_model_a2_single_note_per_phase() {
    let mut r = rig();
    let mut bad = Vec::new();
    for shift in (0..512usize).step_by(64) {
        let attack = 44100 + shift;
        let x = render(WIN, &[part(45.0, 0.4, attack as f64 / RATE, 1.0)]);
        let ns = r.tr.notes(&x).unwrap();
        let a2: Vec<(u8, f64, f64)> = ns.iter().filter(|n| (44..=46).contains(&n.pitch)).map(|n| (n.pitch, n.t, n.dur)).collect();
        println!("shift {shift:3}: {} A2-band notes {a2:?}", a2.len());
        if a2.len() != 1 {
            bad.push((shift, a2.len()));
        }
    }
    assert!(bad.is_empty(), "A2 band must give exactly one note per attack phase (shift, count): {bad:?}");
}


/// Breaker (pure): a real soft note (grid 0.45 for 30 frames at pitch 50, onset 0.5 at frame 60) inside noise
/// with a 0.40 ceiling (seed 1). The walk bridges the noise frames, so the span runs to about frame 172 and its
/// mean drops to about 0.27 (velocity 35). The mean floor drops the note. 4959b42 kept it (112 frames, vel 35).
/// Expected: a note on pitch 71 at frame 60.
#[test]
fn r4_pure_soft_note_in_noise_kept() {
    let mut x = 7919u64;
    let (mut note, mut onset) = grids0();
    for f in 0..172 {
        for p in 0..88 {
            note[f][p] = lcg(&mut x) * 0.40;
            onset[f][p] = lcg(&mut x) * 0.45;
        }
    }
    for f in 0..172 {
        note[f][10] = 0.0;
    }
    onset[100][10] = 0.5;
    for f in 60..90 {
        note[f][50] = 0.45;
    }
    onset[60][50] = 0.5;
    let new = frames_to_notes(&note, &onset);
    let old = old_frames_to_notes(&note, &onset);
    println!("new {:?}\nold {:?}", new.iter().filter(|n| n.pitch == 71).collect::<Vec<_>>(), old.iter().filter(|n| n.pitch == 71).collect::<Vec<_>>());
    assert!(new.iter().any(|n| n.pitch == 71 && (n.t * FPS - 60.0).abs() <= 2.0), "soft note at frame 60 dropped inside noise (4959b42 kept it): old {:?}", old.iter().filter(|n| n.pitch == 71).collect::<Vec<_>>());
}
