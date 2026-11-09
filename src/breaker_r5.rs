//! Breaker round 5 (fix round 4: F23, block-wise silence gate). Test-only, never production code.
//!
//! `ungated` replays the front end of `Transcriber::notes` without `is_silent`, so each test can show that the
//! model finds the note before the gate is applied.
use crate::dsp::{self, BP_RATE, SEP_RATE};
use crate::models;
use crate::notes::Note;
use crate::sep::{frames_to_notes, pick_grids, Transcriber, BP_SAMPLES};
use ort::session::Session;
use ort::value::Tensor;
use std::f64::consts::PI;

const RATE: f64 = 44100.0;
const WIN: usize = 88200;

/// Sine at MIDI `midi`, amplitude `amp`, from `start` for `dur` seconds, no envelope, in a window of `total`.
fn sine(total: usize, midi: f64, amp: f32, start: f64, dur: f64) -> Vec<f32> {
    let f0 = 440.0 * 2f64.powf((midi - 69.0) / 12.0);
    let mut x = vec![0f32; total];
    let i0 = (start * RATE) as usize;
    let i1 = (((start + dur) * RATE) as usize).min(total);
    for i in i0..i1 {
        x[i] = ((2.0 * PI * f0 * i as f64 / RATE).sin() as f32) * amp;
    }
    x
}

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

/// Front end of Transcriber::notes without is_silent. Returns (note, onset) grids.
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

fn ungated(r: &mut Rig, x: &[f32]) -> Vec<Note> {
    let (g, o) = grids(&mut r.raw, x);
    frames_to_notes(&g, &o)
}

fn fmt(ns: &[Note]) -> String {
    if ns.is_empty() {
        return "-".into();
    }
    ns.iter().map(|n| format!("p{}@{:.2}/{:.0}ms/v{}", n.pitch, n.t, n.dur * 1000.0, n.vel)).collect::<Vec<_>>().join(" ")
}

/// A note of `pitch` whose onset lies in [t0, t1] and lasts at least `min_dur` seconds.
fn has(ns: &[Note], pitch: u8, t0: f64, t1: f64, min_dur: f64) -> bool {
    ns.iter().any(|n| n.pitch == pitch && n.t >= t0 && n.t <= t1 && n.dur >= min_dur)
}

/// Break (F23 must-hold): the model finds a clean 300 ms A2 at amplitude 0.0010 in silence (ungated:
/// p45 at 0.82 s, 348 ms, vel 69). The block gate drops it, because a sine's block RMS is 0.707 x peak
/// (0.00071 here, below SILENCE_RMS 1e-3). The gate's own quiet-note test pins 0.0035, not this edge.
#[test]
#[ignore]
fn r5_model_quiet_300ms_a2_survives_gate() {
    let mut r = rig();
    let x = sine(WIN, 45.0, 0.0010, 0.85, 0.30);
    let u = ungated(&mut r, &x);
    assert!(has(&u, 45, 0.7, 0.95, 0.139), "precondition: model should see the quiet note without the gate: {}", fmt(&u));
    let p = r.tr.notes(&x).unwrap();
    assert!(has(&p, 45, 0.7, 0.95, 0.139), "real quiet A2 dropped by the silence gate: prod {}", fmt(&p));
}

/// Break (F23 must-hold, minimum length): a 139 ms A2 at amplitude 0.0012 is the shortest sound that gives a
/// note of the minimum length (ungated: p45 at 0.77 s, 267 ms, vel 55). The gate drops it (block RMS 0.00086).
#[test]
#[ignore]
fn r5_model_quiet_139ms_a2_survives_gate() {
    let mut r = rig();
    let x = sine(WIN, 45.0, 0.0012, 0.85, 0.139);
    let u = ungated(&mut r, &x);
    assert!(has(&u, 45, 0.6, 0.95, 0.139), "precondition: model should see the quiet note without the gate: {}", fmt(&u));
    let p = r.tr.notes(&x).unwrap();
    assert!(has(&p, 45, 0.6, 0.95, 0.139), "minimum-length quiet A2 dropped by the silence gate: prod {}", fmt(&p));
}
