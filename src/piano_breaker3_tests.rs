//! Breaker round 3 for #23 / PR #30 (head 464b0c4): what the new level / rise / balance gates miss.
use super::round2::fluid_opts;
use super::*;

fn recall_of(est: &[Note], gt: &[Note]) -> f64 {
    note_f1(est, gt, TOL).1
}

fn fs(name: &str, notes: &[Note]) -> Vec<f32> {
    let x = fluid_opts(name, notes, 0, 44100, &[]);
    to_rate(&x, 44100)
}

// ---------------------------------------------------------------------------------------------
// Soft after loud on the same key
// ---------------------------------------------------------------------------------------------

fn soft_after_loud(ringing: bool) -> Result<String, String> {
    let mut rep = Vec::new();
    let mut bad = Vec::new();
    for pitch in [40u8, 55, 67, 79] {
        for soft in [30u8, 40, 55] {
            let (gt, t2) = if ringing {
                (vec![n(0.3, 1.5, pitch, 120), n(0.9, 0.6, pitch, soft)], 0.9)
            } else {
                (vec![n(0.3, 0.4, pitch, 120), n(1.3, 0.5, pitch, soft)], 1.3)
            };
            let est = transcribe(&synth_piano(&gt, 2.5, 3), RATE);
            let found = est.iter().any(|e| e.pitch == pitch && (e.t - t2).abs() <= TOL);
            rep.push(format!("{pitch}/v{soft}:{}", found as u8));
            if !found {
                bad.push(format!("key {pitch} v120 -> v{soft}: {}", names(&est)));
            }
        }
    }
    if bad.is_empty() {
        Ok(rep.join(" "))
    } else {
        Err(format!("{}\n{}", rep.join(" "), bad.join("\n")))
    }
}

/// RED (R3-F1, regression). pp after ff on the same key, struck again 0.6 s after the ff note was
/// released (synthetic piano, amplitude ~ vel^2: v120 -> v30 is 24 dB). Found 0 of 4 at v30
/// (round 2: 4 of 4); v40 / v55 are found. Root cause: the new-note gate wants the key's own
/// pre-rise peak minus NEW_LVL_KEY = 20 dB, and that peak only decays 3 dB/s, so for ~1-2 s after a
/// loud note the same key can't start anything more than ~21-24 dB softer. A pp answer to an ff
/// note is ordinary piano writing.
#[test]
fn brk3_soft_after_loud_same_key_after_release() {
    let r = soft_after_loud(false);
    eprintln!("METRIC r3 soft after loud, after release: {r:?}");
    assert!(r.is_ok(), "{}", r.unwrap_err());
}

/// RED (R3-F2, also in round 2). The same, but the pp note re-strikes the key while the ff note
/// still rings (0.6 s in): found 0 of 12, at every key and soft velocity. The re-strike needs a
/// rise of x1.4..2 over the ringing note's energy, and a pp strike on a ringing ff string adds less.
#[test]
fn brk3_soft_restrike_of_ringing_loud_note() {
    let r = soft_after_loud(true);
    eprintln!("METRIC r3 soft re-strike of ringing loud note: {r:?}");
    assert!(r.is_ok(), "{}", r.unwrap_err());
}

/// RED (R3-F2, FluidSynth, same in round 2). The same on the FluidSynth piano (44.1 kHz render):
/// recall of the soft notes 0.30. Every re-strike of a ringing ff note is missed, and after release
/// G3 / G4 miss both velocities.
#[test]
#[ignore]
fn brk3_fluidsynth_soft_after_loud_same_key() {
    let mut gt = Vec::new();
    let mut t = 0.4;
    for pitch in [43u8, 55, 60, 67, 76] {
        for soft in [30u8, 45] {
            gt.push(n(t, 1.2, pitch, 120));
            gt.push(n(t + 0.6, 0.5, pitch, soft)); // re-strike while ringing
            t += 1.6;
            gt.push(n(t, 0.3, pitch, 120));
            gt.push(n(t + 1.0, 0.5, pitch, soft)); // after release
            t += 2.0;
        }
    }
    let est = transcribe(&fs("r3-soft-after-loud", &gt), RATE);
    let soft: Vec<Note> = gt.iter().filter(|g| g.vel < 100).cloned().collect();
    let r = recall_of(&est, &soft);
    let missed: Vec<String> = soft.iter().filter(|g| !est.iter().any(|e| e.pitch == g.pitch && (e.t - g.t).abs() <= TOL)).map(|g| format!("{}v{}@{:.2}", g.pitch, g.vel, g.t)).collect();
    eprintln!("METRIC r3 fluidsynth soft after loud: recall of the soft notes {r:.3}; missed {missed:?}");
    assert!(r >= 0.8, "soft notes after loud ones: recall {r:.3}, missed {missed:?}");
}

// ---------------------------------------------------------------------------------------------
// Quiet melody under a loud chord (the loudest-key -30 dB gate)
// ---------------------------------------------------------------------------------------------

/// RED (R3-F3, also in round 2). A held loud chord (C2 C3 E3 G3 at vel 120) under a quiet
/// right-hand melody: melody recall 0.00 / 0.25 / 0.38 at v25 / v35 / v50 (round 2: 0.00 / 0.25 /
/// 0.25); FluidSynth (next test) 0.00 / 0.00 / 0.40 at v30 / v45 / v60. NOT the new -30 dB
/// loudest-key gate: NEW_LVL_GLOBAL=80 changes nothing. A contributor is the whitening floor 50 dB
/// under the frame peak (DYN_RANGE = 3e-3): DYN_RANGE=3e-4 lifts synthetic v50 to 0.88 and
/// FluidSynth v45 / v60 to 0.30 / 0.50.
#[test]
fn brk3_quiet_melody_under_loud_chord() {
    let mel = [69u8, 71, 73, 75, 78, 80, 82, 85];
    let mut rep = Vec::new();
    let mut bad = Vec::new();
    for vel in [25u8, 35, 50] {
        let mut gt = vec![n(0.3, 3.6, 36, 120), n(0.3, 3.6, 48, 120), n(0.3, 3.6, 52, 120), n(0.3, 3.6, 55, 120)];
        for (i, &p) in mel.iter().enumerate() {
            gt.push(n(0.7 + 0.4 * i as f64, 0.35, p, vel));
        }
        let est = transcribe(&synth_piano(&gt, 4.2, 5), RATE);
        let r = recall_of(&est, &gt[4..]);
        rep.push(format!("v{vel}: R {r:.2}"));
        if vel >= 35 && r < 0.75 {
            bad.push(format!("melody v{vel} under a v120 chord: R {r:.2}: {}", names(&est)));
        }
    }
    eprintln!("METRIC r3 quiet melody under loud chord (synthetic): {}", rep.join("; "));
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
#[ignore]
fn brk3_fluidsynth_quiet_melody_under_loud_chord() {
    let mel = [69u8, 71, 73, 75, 78, 80, 82, 85, 83, 81];
    let mut rep = Vec::new();
    let mut bad = Vec::new();
    for vel in [30u8, 45, 60] {
        let mut gt = Vec::new();
        for bar in 0..2 {
            let t = 0.4 + 2.2 * bar as f64;
            for p in [36u8, 48, 52, 55] {
                gt.push(n(t, 2.1, p, 120));
            }
        }
        for (i, &p) in mel.iter().enumerate() {
            gt.push(n(0.6 + 0.4 * i as f64, 0.35, p, vel));
        }
        let est = transcribe(&fs(&format!("r3-quiet-mel-{vel}"), &gt), RATE);
        let mg: Vec<Note> = gt.iter().filter(|g| g.pitch >= 69).cloned().collect();
        let r = recall_of(&est, &mg);
        rep.push(format!("v{vel}: R {r:.2}"));
        if vel >= 45 && r < 0.75 {
            bad.push(format!("v{vel}: R {r:.2}"));
        }
    }
    eprintln!("METRIC r3 fluidsynth quiet melody under loud chord: {}", rep.join("; "));
    assert!(bad.is_empty(), "{}", bad.join("; "));
}

// ---------------------------------------------------------------------------------------------
// Re-strikes the new gates may now miss
// ---------------------------------------------------------------------------------------------

/// RED (R3-F5, Low). Repeated notes in the bass at 8/s (the PR's 90 ms holds): E1 0.40, A1 0.70,
/// E2 0.80 (round 2: 0.40 / 0.70 / 0.90); C2, A2, C#3 and every 4/s case are 1.00. E2 is inside the
/// PR's "16/16 at every pitch E2..C7" claim (its own test uses a different seed).
#[test]
fn brk3_bass_repeats_below_d3() {
    let mut rep = Vec::new();
    let mut bad = Vec::new();
    for pitch in [28u8, 33, 36, 40, 45, 49] {
        for (att, hz, hold) in [(0.003f64, 8.0f64, 0.09f64), (0.003, 4.0, 0.2), (0.05, 4.0, 0.2)] {
            let gt: Vec<Note> = (0..10).map(|i| n(0.4 + i as f64 / hz, hold, pitch, 95)).collect();
            let est = transcribe(&synth_full(&gt, 0.4 + 10.0 / hz + 0.6, RATE, 20000.0, 3, att), RATE);
            let r = recall_of(&est, &gt);
            rep.push(format!("{pitch} {hz}/s att {:.0}ms: {r:.2}", att * 1e3));
            if r < 0.9 {
                bad.push(format!("key {pitch} {hz}/s hold {hold} attack {:.0} ms: R {r:.2}", att * 1e3));
            }
        }
    }
    eprintln!("METRIC r3 bass repeats: {}", rep.join("; "));
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// RED (R3-F4, also in round 2). A five-note chord (C3 G3 C4 E4 G4) repeated on the FluidSynth piano
/// with 30..60 ms gaps, i.e. the repeated-chord accompaniment of most pop piano: recall 0.33 (4/s,
/// 35 ms), 0.53 (4/s, 60 ms), 0.47 (6/s), 0.53 (2/s, 30 ms); round 2 0.35 / 0.53 / 0.50 / 0.50.
/// Single-key repeats with the same gaps are 12/12, so it is the chord: the octave / twelfth
/// rivalry and the shared partials suppress most chord tones' re-strikes.
#[test]
#[ignore]
fn brk3_fluidsynth_repeated_dense_chords() {
    let mut rep = Vec::new();
    let mut bad = Vec::new();
    for (hz, gap) in [(4.0f64, 0.035f64), (4.0, 0.06), (6.0, 0.035), (2.0, 0.03)] {
        let mut gt = Vec::new();
        for i in 0..8 {
            for p in [48u8, 55, 60, 64, 67] {
                gt.push(n(0.4 + i as f64 / hz, 1.0 / hz - gap, p, 90));
            }
        }
        let est = transcribe(&fs(&format!("r3-chord-rep-{hz}-{}", (gap * 1e3) as u32), &gt), RATE);
        let (p, r, _) = note_f1(&est, &gt, TOL);
        rep.push(format!("{hz}/s gap {:.0} ms: R {r:.2} P {p:.2}", gap * 1e3));
        if r < 0.8 {
            bad.push(format!("{hz}/s gap {:.0} ms: R {r:.2}", gap * 1e3));
        }
    }
    eprintln!("METRIC r3 repeated 5-note chords: {}", rep.join("; "));
    assert!(bad.is_empty(), "{}", bad.join("; "));
}

/// GREEN probe. Rock / pop bass: a low key re-struck in eighths while a short fifth or octave above is struck
/// with it each time (power chord). The new rivalry rule cancels a held key's re-strike whenever a
/// key a fifth/octave/twelfth above is *starting*, which is every time here.
#[test]
fn brk3_bass_restrike_with_short_fifth_on_top() {
    let mut bad = Vec::new();
    let mut rep = Vec::new();
    for (bass, top) in [(36u8, 43u8), (40, 52), (45, 52), (43, 55)] {
        let mut gt = Vec::new();
        for i in 0..8 {
            let t = 0.4 + 0.25 * i as f64;
            gt.push(n(t, 0.22, bass, 100));
            gt.push(n(t, 0.08, top, 85));
        }
        let est = transcribe(&synth_piano(&gt, 2.8, 4), RATE);
        let bg: Vec<Note> = gt.iter().filter(|g| g.pitch == bass).cloned().collect();
        let r = recall_of(&est, &bg);
        rep.push(format!("{bass}+{top}: bass R {r:.2}"));
        if r < 0.9 {
            bad.push(format!("bass {bass} with {top} on top: bass recall {r:.2}: {}", names(&est)));
        }
    }
    eprintln!("METRIC r3 bass + short fifth/octave: {}", rep.join("; "));
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// GREEN probe. Slow-attack piano (soft pedal, felt piano, sampled pp layer): attacks of 80..200 ms,
/// repeats with a 40 ms gap. Alone: 1.00 everywhere (round 2: 0.25..1.00, much better now). Over a
/// note held a sixth below (metric only): 0.00..1.00, worst C3 with a 200 ms attack (0.00).
#[test]
fn brk3_slow_attack_piano() {
    let mut bad = Vec::new();
    let mut rep = Vec::new();
    for att in [0.08f64, 0.12, 0.2] {
        for pitch in [48u8, 60, 72] {
            let gt: Vec<Note> = (0..4).map(|i| n(0.4 + 0.5 * i as f64, 0.46, pitch, 90)).collect();
            let est = transcribe(&synth_full(&gt, 2.8, RATE, 20000.0, 2, att), RATE);
            let r = recall_of(&est, &gt);
            // over a held note a major sixth below
            let mut g2 = gt.clone();
            g2.push(n(0.2, 2.4, pitch - 9, 80));
            let est2 = transcribe(&synth_full(&g2, 2.8, RATE, 20000.0, 2, att), RATE);
            let r2 = recall_of(&est2, &gt);
            rep.push(format!("{pitch} att {:.0}: {r:.2}/{r2:.2}", att * 1e3));
            if r < 0.75 {
                bad.push(format!("key {pitch} attack {:.0} ms: R {r:.2} alone [{}], {r2:.2} over a held note [{}]", att * 1e3, names(&est), names(&est2)));
            }
        }
    }
    eprintln!("METRIC r3 slow attack: {}", rep.join("; "));
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

// ---------------------------------------------------------------------------------------------
// Onset walk-back
// ---------------------------------------------------------------------------------------------

/// GREEN probe. Onset bias: isolated notes with a 3 ms attack over the keyboard. The walk-back to 20 % of the
/// level reached must not stamp onsets early (the 1024 window already sees an attack 23 ms before
/// it happens).
#[test]
fn brk3_onset_not_stamped_early() {
    let mut errs = Vec::new();
    for pitch in (24u8..=100).step_by(4) {
        let gt = vec![n(0.5, 0.5, pitch, 90)];
        let est = transcribe(&synth_piano(&gt, 1.5, 2), RATE);
        if let Some(e) = est.iter().find(|e| e.pitch == pitch) {
            errs.push((pitch, (e.t - 0.5) * 1e3));
        }
    }
    let mean = errs.iter().map(|e| e.1).sum::<f64>() / errs.len() as f64;
    let min = errs.iter().map(|e| e.1).fold(f64::MAX, f64::min);
    eprintln!("METRIC r3 onset error ms: mean {mean:.1}, earliest {min:.1}: {errs:?}");
    assert!(min > -25.0 && mean.abs() < 15.0, "onsets early: mean {mean:.1} ms, earliest {min:.1} ms");
}

/// GREEN probe. FluidSynth onsets: mean +5.5 ms, earliest -1.2 ms (round 2: +13.7 / +6.7). The walk-back
/// does not stamp early.
#[test]
#[ignore]
fn brk3_fluidsynth_onset_bias() {
    let gt: Vec<Note> = (36u8..=96).step_by(3).enumerate().map(|(i, p)| n(0.5 + 0.6 * i as f64, 0.4, p, 90)).collect();
    let est = transcribe(&fs("r3-onsets", &gt), RATE);
    let mut errs = Vec::new();
    for g in &gt {
        if let Some(e) = est.iter().filter(|e| e.pitch == g.pitch).min_by(|a, b| (a.t - g.t).abs().total_cmp(&(b.t - g.t).abs())) {
            errs.push((e.t - g.t) * 1e3);
        }
    }
    let mean = errs.iter().sum::<f64>() / errs.len() as f64;
    let min = errs.iter().copied().fold(f64::MAX, f64::min);
    eprintln!("METRIC r3 fluidsynth onset error ms: mean {mean:.1}, earliest {min:.1}, n {}", errs.len());
    assert!(min > -25.0 && mean.abs() < 15.0, "mean {mean:.1} earliest {min:.1}");
}

/// GREEN probe (evidence against the KNOWN GAP reason on brk_fluidsynth_legato_repeats). On the same
/// FluidSynth C4 8/s renders, the full-band 10 ms RMS jump at a strike is indeed small (5 ms gap:
/// -0.4..0.8 dB, 10 ms: -0.6..0.6), but the high band (2nd difference, rising 12 dB/oct) jumps
/// +4.7..5.9 dB at every strike, against -1.0..-0.4 dB half-way between strikes and -0.7..1.1 dB on
/// a single held C4 probed on the same grid. A tremolo or swell moves both bands alike, so "high-band
/// jump minus full-band jump" separates the strikes with a >3.5 dB margin here. (Caveat: part of it may
/// be the sample start in FluidSynth; a real hammer also makes a broadband transient.)
#[test]
#[ignore]
fn brk3_fluidsynth_legato_hf_cue_exists() {
    for gap in [0.005f64, 0.010, 0.035, -1.0] {
        // gap -1: one held C4 (no re-strikes), probed at the same 8/s grid: the control
        let gt = if gap < 0.0 { let mut v = repeats(60, 8.0, 0.0, 12, 0.5); v.truncate(1); v[0].dur = 1.6; let mut probe = repeats(60, 8.0, 0.0, 12, 0.5); probe[0] = v[0].clone(); probe } else { repeats(60, 8.0, gap, 12, 0.5) };
        let render: Vec<Note> = if gap < 0.0 { vec![gt[0].clone()] } else { gt.clone() };
        let x = fluid_opts(&format!("r3-legato-{}", (gap * 1e3) as i32), &render, 0, 22050, &[]);
        let hp: Vec<f32> = (0..x.len()).map(|i| if i >= 2 { x[i] - 2.0 * x[i - 1] + x[i - 2] } else { 0.0 }).collect();
        let rms = |s: &[f32], c: usize| -> f64 { let a = c.saturating_sub(110); let b = (c + 110).min(s.len()); (s[a..b].iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / (b - a) as f64).sqrt().max(1e-12) };
        let mut full = Vec::new();
        let mut high = Vec::new();
        let ctrl = std::env::var("CTRL").is_ok();
        for g in &gt[1..] {
            let c = ((g.t + if ctrl { 0.0625 } else { 0.0 }) * 22050.0) as usize;
            let pre = (1..=4).map(|k| rms(&x, c - k * 220)).fold(f64::MAX, f64::min);
            let post = (0..=4).map(|k| rms(&x, c + k * 220)).fold(0f64, f64::max);
            full.push(20.0 * (post / pre).log10());
            let pre = (1..=4).map(|k| rms(&hp, c - k * 220)).fold(f64::MAX, f64::min);
            let post = (0..=4).map(|k| rms(&hp, c + k * 220)).fold(0f64, f64::max);
            high.push(20.0 * (post / pre).log10());
        }
        let f = |v: &Vec<f64>| format!("min {:.1} median {:.1} max {:.1}", v.iter().cloned().fold(f64::MAX, f64::min), { let mut s = v.clone(); s.sort_by(|a, b| a.total_cmp(b)); s[s.len() / 2] }, v.iter().cloned().fold(f64::MIN, f64::max));
        eprintln!("METRIC r3 legato gap {:.0} ms (CTRL={ctrl}): full-band jump dB {}; high-band jump dB {}", gap * 1e3, f(&full), f(&high));
        let cue: Vec<f64> = high.iter().zip(&full).map(|(h, fb)| h - fb).collect();
        let lo = cue.iter().cloned().fold(f64::MAX, f64::min);
        let hi = cue.iter().cloned().fold(f64::MIN, f64::max);
        if gap > 0.0 && gap < 0.02 && !ctrl {
            assert!(lo > 3.0, "strike cue {lo:.1} dB");
        } else if gap < 0.0 || ctrl {
            assert!(hi < 1.5, "control cue {hi:.1} dB");
        }
    }
}
