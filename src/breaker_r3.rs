//! Breaker round 3 (F10 to F12 fixes). Test-only, never production code.
use crate::sep::{frames_to_notes, infer_onsets, pick_grids, Transcriber, FPS};
use crate::stemcfg::remute_target;

fn grids() -> (Vec<Vec<f32>>, Vec<Vec<f32>>) {
    (vec![vec![0f32; 88]; 172], vec![vec![0f32; 88]; 172])
}

/// Small deterministic LCG in [0, 1).
fn lcg(x: &mut u64) -> f32 {
    *x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    ((*x >> 33) as f32) / ((1u64 << 31) as f32)
}

// ---------- F10 ----------

/// Probe: a node the engine un-mutes (same id, mute=false) while replaced must be re-muted, and the
/// helper must say nothing when nothing needs doing. Expected to pass.
#[test]
fn r3_remute_state_rule_edges() {
    assert_eq!(remute_target(true, Some((9, false)), Some(9)), Some(9));
    assert_eq!(remute_target(true, Some((9, false)), Some(3)), Some(9));
    assert_eq!(remute_target(true, Some((9, true)), Some(9)), None);
    assert_eq!(remute_target(false, Some((9, false)), Some(9)), None);
    assert_eq!(remute_target(false, None, None), None);
}

// ---------- F12 ----------

/// Probe: upstream n_diff=2 on a single sharp attack. diff = min(note[f]-note[f-1], note[f]-note[f-2]),
/// clamped at 0, frames 0 and 1 forced to 0, scaled to the global onset max. Expected to pass.
#[test]
fn r3_infer_onsets_matches_upstream_formula() {
    let (mut note, mut onset) = grids();
    for f in 10..40 {
        note[f][5] = 0.8;
    }
    onset[50][7] = 0.6; // global onset max, on another pitch
    let inf = infer_onsets(&note, &onset);
    assert!((inf[10][5] - 0.6).abs() < 1e-6, "attack frame {}", inf[10][5]);
    assert_eq!(inf[11][5], 0.0);
    assert_eq!(inf[0][5], 0.0);
    // a rise held for only one of the two spans is not an onset (min over n = 1, 2)
    let (mut n2, o2) = grids();
    n2[10][5] = 0.5;
    n2[9][5] = 0.4;
    let inf2 = infer_onsets(&n2, &o2);
    assert_eq!(inf2[10][5], 0.0, "no onset peak anywhere, so omax = 0");
}

/// Probe: a rise that starts at frame 0 or 1 cannot be inferred (upstream forces frame_diff[:2] = 0).
/// A note sounding from frame 0 with no onset peak is therefore not a note. Documented, not a break:
/// transcribe() drops notes at the window start anyway.
#[test]
fn r3_frame0_rise_is_not_inferred() {
    let (mut note, mut onset) = grids();
    for f in 0..30 {
        note[f][20] = 0.9;
    }
    onset[60][40] = 0.9;
    let ns = frames_to_notes(&note, &onset);
    println!("frame0 notes {ns:?}");
    assert!(ns.iter().all(|n| n.pitch != 41), "{ns:?}");
}

/// Probe: a vibrato on a held note (0.8 +/- 0.2 every 6 frames), attack peak 0.9 at frame 20. The
/// held note never falls under FRAME_THRESH, so it must stay one note.
#[test]
fn r3_vibrato_on_held_note_stays_one_note() {
    let (mut note, mut onset) = grids();
    for f in 20..120 {
        note[f][30] = if (f / 3) % 2 == 0 { 0.8 } else { 0.6 };
    }
    onset[20][30] = 0.9;
    let ns = frames_to_notes(&note, &onset);
    println!("vibrato notes {ns:?}");
    assert_eq!(ns.iter().filter(|n| n.pitch == 51).count(), 1, "{ns:?}");
}

/// Probe: a dip under FRAME_THRESH for 12 frames (longer than ENERGY_TOL) followed by a re-attack
/// must give two notes. A dip of 3 frames must give one. Expected to pass.
#[test]
fn r3_repeated_pitch_split_by_gap_length() {
    let (mut note, mut onset) = grids();
    for f in 20..50 {
        note[f][30] = 0.8;
    }
    for f in 62..120 {
        note[f][30] = 0.8;
    }
    onset[20][30] = 0.9;
    onset[62][30] = 0.9;
    let ns = frames_to_notes(&note, &onset);
    println!("gap12 notes {ns:?}");
    assert_eq!(ns.iter().filter(|n| n.pitch == 51).count(), 2, "{ns:?}");
    let (mut note, mut onset) = grids();
    for f in 20..50 {
        note[f][30] = 0.8;
    }
    for f in 53..120 {
        note[f][30] = 0.8;
    }
    onset[20][30] = 0.9;
    onset[53][30] = 0.9;
    let ns = frames_to_notes(&note, &onset);
    println!("gap3 notes {ns:?}");
    assert_eq!(ns.iter().filter(|n| n.pitch == 51).count(), 1, "{ns:?}");
}

/// Probe (false notes on noise): note grid is uniform noise in [0, 0.35), onset grid is uniform noise in
/// [0, 0.45) with one real onset peak of 0.6 on pitch 10. No pitch here has a real attack, so the only
/// note allowed is the one at pitch 10 (none, since its note grid is set to 0). Over 20 seeds, count
/// every note that appears. Any note is a spurious MIDI note.
#[test]
fn r3_noise_produces_no_notes() {
    let mut total = 0usize;
    let mut worst = Vec::new();
    for seed in 1..=20u64 {
        let mut x = seed * 7919;
        let (mut note, mut onset) = grids();
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
    println!("noise: seeds with notes {worst:?}, total {total}");
    assert_eq!(total, 0, "spurious notes on noise: {worst:?}");
}

/// Probe: the grids are 172 frames; a rise at the last frames must not panic and must not invent notes
/// shorter than MIN_NOTE_FRAMES. Expected to pass.
#[test]
fn r3_last_frame_rise_is_safe() {
    let (mut note, mut onset) = grids();
    for f in 168..172 {
        note[f][60] = 0.9;
    }
    onset[171][60] = 0.9;
    onset[170][60] = 0.9;
    let ns = frames_to_notes(&note, &onset);
    assert!(ns.iter().all(|n| n.pitch != 81 || n.dur >= 5.0 / FPS), "{ns:?}");
    let _ = infer_onsets(&note[..1].to_vec(), &onset[..1].to_vec());
}

/// Probe: determinism of candidate order. Two equal-valued onsets on two pitches at the same frame must
/// give the same notes on every call. Expected to pass.
#[test]
fn r3_candidate_order_is_deterministic() {
    let (mut note, mut onset) = grids();
    for f in 40..90 {
        note[f][12] = 0.7;
        note[f][50] = 0.7;
    }
    onset[40][12] = 0.8;
    onset[40][50] = 0.8;
    let first = frames_to_notes(&note, &onset);
    for _ in 0..20 {
        assert_eq!(frames_to_notes(&note, &onset), first);
    }
}

/// Probe: pick_grids keeps the shape rule's neighbours intact (no `:0` fallback, exact names only).
/// Expected to pass.
#[test]
fn r3_pick_grids_exact_names_only() {
    let n: Vec<String> = ["StatefulPartitionedCall:10", "StatefulPartitionedCall:1", "StatefulPartitionedCall:20", "StatefulPartitionedCall:2"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(pick_grids(&n), Ok((1, 3)));
    let n: Vec<String> = ["StatefulPartitionedCall:12", "StatefulPartitionedCall:22"].iter().map(|s| s.to_string()).collect();
    assert!(pick_grids(&n).is_err());
}

// ---------- model (ignored, CPU) ----------

fn a2_input(attack: usize, amp: f32, total: usize) -> Vec<f32> {
    (0..total)
        .map(|i| {
            if i < attack {
                0.0
            } else {
                ((2.0 * std::f64::consts::PI * 110.0 * i as f64 / 44100.0).sin() as f32) * amp
            }
        })
        .collect()
}

/// Breaker (ignored, CPU, model): the transcriber_finds_a2 gate (|t - 0.988| < 0.06, where 0.988 is the
/// attack in model-window time: input attack 1.0 s minus the 256/22050 s window offset) must hold for
/// attacks at other sub-frame phases, not only for the suite's exact 1.0 s attack.
#[test]
#[ignore]
fn r3_model_a2_gate_holds_across_attack_phases() {
    let m = crate::models::ensure(&["basic-pitch-nmp.onnx"], &|s: String| println!("{s}")).unwrap();
    let mut tr = Transcriber::load(&m).unwrap();
    let offset = 256.0 / 22050.0;
    let mut bad = Vec::new();
    for shift in (0..=448).step_by(64) {
        let attack = 44100 + shift;
        let x = a2_input(attack, 0.4, 88200);
        let ns = tr.notes(&x).unwrap();
        let expect = attack as f64 / 44100.0 - offset;
        let hit = ns.iter().filter(|n| (44..=46).contains(&n.pitch)).map(|n| n.t).find(|t| (t - expect).abs() < 0.06);
        println!("shift {shift:3} expect {expect:.4} hit {hit:?}");
        if hit.is_none() {
            let nearest = ns.iter().filter(|n| (44..=46).contains(&n.pitch)).map(|n| n.t - expect).fold(None, |a: Option<f64>, d| match a { Some(x) if x.abs() <= d.abs() => Some(x), _ => Some(d) });
            bad.push((shift, nearest));
        }
    }
    assert!(bad.is_empty(), "gate missed at attack phases (shift samples, note-minus-expect s): {bad:?}");
}

/// Probe (ignored): soft-attack recall. Same A2 tone at amplitudes 0.4, 0.2, 0.1. Print the note count;
/// the fix only claims recall for attacks whose onset grid crosses 0.5 somewhere in the window.
#[test]
#[ignore]
fn r3_model_amplitude_recall() {
    let m = crate::models::ensure(&["basic-pitch-nmp.onnx"], &|s: String| println!("{s}")).unwrap();
    let mut tr = Transcriber::load(&m).unwrap();
    for amp in [0.4f32, 0.2, 0.1] {
        let x = a2_input(44100, amp, 88200);
        let ns = tr.notes(&x).unwrap();
        println!("amp {amp}: notes {ns:?}");
    }
}

/// Breaker (ignored, CPU, model): white noise must not become notes. Pre-fix the real model's onset grid
/// only makes notes at peaks >= 0.5. Also prints the steady tone (one note expected, correct pitch).
#[test]
#[ignore]
fn r3_model_white_noise_no_notes() {
    let m = crate::models::ensure(&["basic-pitch-nmp.onnx"], &|s: String| println!("{s}")).unwrap();
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
    assert!(bad.is_empty(), "notes on white noise (amp, count): {bad:?}");
}

/// Probe: an onset peak inside a sustained note of the same pitch (a re-articulation the note grid does
/// not show). Pre-fix and post-fix give one note: the held note consumes the frames. Report only.
#[test]
fn r3_onset_peak_inside_held_note() {
    let (mut note, mut onset) = grids();
    for f in 20..120 {
        note[f][30] = 0.8;
    }
    onset[20][30] = 0.9;
    onset[60][30] = 0.9;
    let ns = frames_to_notes(&note, &onset);
    println!("held+peak notes {ns:?}");
    assert_eq!(ns.iter().filter(|n| n.pitch == 51).count(), 2, "{ns:?}");
}
