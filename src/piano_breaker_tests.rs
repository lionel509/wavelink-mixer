//! Breaker round 1 for #23 / PR #30: adversarial audio and edge cases for the piano tracker.
//! Red tests mark real defects; green ones are kept as regression probes. Each test says which.
use super::*;

const TOL: f64 = 0.05;

fn n(t: f64, dur: f64, pitch: u8, vel: u8) -> Note {
    Note { t, dur, pitch, vel }
}

fn names(v: &[Note]) -> String {
    v.iter().map(|n| format!("{}@{:.3}/{:.3}v{}", n.pitch, n.t, n.dur, n.vel)).collect::<Vec<_>>().join(" ")
}

fn lcg(s: &mut u64) -> f32 {
    *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    ((*s >> 33) as f32) / (1u64 << 31) as f32
}

/// The same piano-like synth as `synth_piano`, at any sample rate, with partials up to `fmax` Hz
/// (capped at 0.45 * rate), so a 48 kHz render carries real content above the analysis Nyquist.
fn synth_at(notes: &[Note], secs: f64, rate: u32, fmax: f64, seed: u64) -> Vec<f32> {
    synth_full(notes, secs, rate, fmax, seed, 0.003)
}

/// `synth_at` with a given attack time (a real hammer + soundboard build-up, or a sampled piano,
/// takes 10..30 ms to peak, not the 3 ms of `synth_piano`).
fn synth_full(notes: &[Note], secs: f64, rate: u32, fmax: f64, seed: u64, attack: f64) -> Vec<f32> {
    let r = rate as f64;
    let len = (secs * r) as usize;
    let mut out = vec![0f32; len];
    let mut s = seed;
    for nt in notes {
        let p = nt.pitch as f64;
        let f0 = 440.0 * 2f64.powf((p - 69.0) / 12.0);
        let b = 2e-4 * 2f64.powf((p - 60.0) / 24.0);
        let tau = (1.6 * 2f64.powf(-(p - 60.0) / 24.0)).clamp(0.25, 4.0);
        let amp = 0.25 * (nt.vel as f64 / 127.0).powi(2);
        let i0 = (nt.t * r) as usize;
        let end = ((nt.t + nt.dur + 0.15) * r) as usize;
        for h in 1..200usize {
            let hf = h as f64;
            let fh = hf * f0 * (1.0 + b * hf * hf).sqrt();
            if fh > fmax.min(0.45 * r) {
                break;
            }
            let mut a = amp / hf.powf(0.9) * (1.0 + 0.3 * (hf * 1.7 + p).sin());
            if h == 1 && p < 33.0 {
                a *= 0.4;
            }
            let tau_h = tau / (1.0 + 0.12 * (hf - 1.0));
            let ph = lcg(&mut s) as f64 * std::f64::consts::TAU;
            let w = std::f64::consts::TAU * fh / r;
            for i in i0..end.min(len) {
                let t = (i - i0) as f64 / r;
                let att = (t / attack).min(1.0);
                let rel = if t > nt.dur { (-(t - nt.dur) / 0.03).exp() } else { 1.0 };
                out[i] += (a * att * rel * (-t / tau_h).exp() * (w * (i - i0) as f64 + ph).sin()) as f32;
            }
        }
    }
    out
}

/// A sustained (organ / voice / bowed) harmonic tone at RATE: 12 partials at 1/h, pitch and
/// amplitude given per sample time, 20 ms attack and release.
fn held_tone(secs: f64, midi: impl Fn(f64) -> f64, amp: impl Fn(f64) -> f64) -> Vec<f32> {
    let r = RATE as f64;
    let len = (secs * r) as usize;
    let mut ph = [0f64; 12];
    (0..len)
        .map(|i| {
            let t = i as f64 / r;
            let f0 = 440.0 * 2f64.powf((midi(t) - 69.0) / 12.0);
            let env = (t / 0.02).min(1.0) * ((secs - t) / 0.02).clamp(0.0, 1.0) * amp(t);
            let mut v = 0f64;
            for (h, p) in ph.iter_mut().enumerate() {
                let hf = (h + 1) as f64;
                if hf * f0 > 0.45 * r {
                    break;
                }
                *p += std::f64::consts::TAU * hf * f0 / r;
                v += (*p).sin() / hf;
            }
            (0.15 * env * v) as f32
        })
        .collect()
}

/// A reference piece: melody, chords and a bass line over 3 registers.
fn piece() -> Vec<Note> {
    let mut gt = Vec::new();
    let mel = [72u8, 74, 76, 77, 79, 77, 76, 74, 72, 71, 72, 76];
    for (i, &p) in mel.iter().enumerate() {
        gt.push(n(0.3 + 0.35 * i as f64, 0.3, p, 70 + (i as u8 * 7) % 40));
    }
    for (i, &root) in [48u8, 53, 55, 48].iter().enumerate() {
        let t = 0.3 + 1.05 * i as f64;
        gt.push(n(t, 0.9, root - 12, 90));
        gt.push(n(t, 0.9, root + 4, 70));
        gt.push(n(t, 0.9, root + 7, 70));
    }
    gt.sort_by(|a, b| a.t.total_cmp(&b.t));
    gt
}

/// Per pitch, notes in time order must not overlap, and every event stream must pair up:
/// On, then Off at a time >= the On, never two Ons of one key without an Off between them.
fn check_events(evs: &[Ev]) -> Result<(), String> {
    check_events_opt(evs, true)
}

fn check_events_opt(evs: &[Ev], overlaps: bool) -> Result<(), String> {
    let mut open: [Option<f64>; 128] = [None; 128];
    for e in evs {
        match *e {
            Ev::On { t, pitch, vel } => {
                if open[pitch as usize].is_some() {
                    return Err(format!("second On of {pitch} at {t:.3} without an Off"));
                }
                if vel == 0 || vel > 127 {
                    return Err(format!("velocity {vel}"));
                }
                open[pitch as usize] = Some(t);
            }
            Ev::Off { t, pitch } => match open[pitch as usize].take() {
                Some(t0) if t + 1e-9 < t0 => return Err(format!("Off of {pitch} at {t:.3} before its On at {t0:.3}")),
                Some(_) => {}
                None => return Err(format!("Off of {pitch} at {t:.3} without an On")),
            },
        }
    }
    if let Some(p) = open.iter().position(|o| o.is_some()) {
        return Err(format!("{p} left sounding"));
    }
    if !overlaps {
        return Ok(());
    }
    let notes = events_to_notes(evs);
    for p in 0..128u8 {
        let v: Vec<&Note> = notes.iter().filter(|x| x.pitch == p).collect();
        for w in v.windows(2) {
            if w[0].t + w[0].dur > w[1].t + 1e-9 {
                return Err(format!(
                    "same-pitch overlap on {p}: {:.3}..{:.3} and {:.3}..{:.3}",
                    w[0].t,
                    w[0].t + w[0].dur,
                    w[1].t,
                    w[1].t + w[1].dur
                ));
            }
        }
    }
    Ok(())
}

fn run_events(x: &[f32]) -> Vec<Ev> {
    let mut tr = PianoTracker::new();
    let mut ev = tr.push(x);
    ev.extend(tr.flush());
    ev
}

// ---------------------------------------------------------------------------------------------
// Streaming / chunking
// ---------------------------------------------------------------------------------------------

/// GREEN probe. Any chunking, including 1-sample pushes, empty pushes and exact hop/window
/// multiples, yields the same events bit for bit as one push.
#[test]
fn brk_push_sizes_are_bit_identical() {
    let gt = piece();
    let x = synth_piano(&gt, 5.0, 11);
    let whole = run_events(&x);
    for sizes in [vec![1usize], vec![255, 256, 257], vec![2048, 0, 4096, 1], vec![11025], vec![22051, 3]] {
        let mut tr = PianoTracker::new();
        let mut ev = Vec::new();
        let (mut i, mut k) = (0, 0);
        while i < x.len() {
            let j = (i + sizes[k % sizes.len()]).min(x.len());
            ev.extend(tr.push(&x[i..j]));
            i = j;
            k += 1;
        }
        ev.extend(tr.flush());
        assert_eq!(ev, whole, "chunk sizes {sizes:?}");
    }
}

/// GREEN probe. A note whose attack sits right on a 0.5 s chunk edge (just before, on, just after)
/// comes out exactly once, live (0.5 s pushes) as offline.
#[test]
fn brk_note_on_chunk_edge_not_split_or_doubled() {
    for dt in [-0.003f64, -0.0005, 0.0, 0.0005, 0.003, 0.0116] {
        for pitch in [33u8, 60, 96] {
            let gt = vec![n(1.0 + dt, 0.7, pitch, 90)];
            let x = synth_piano(&gt, 2.5, 3);
            let mut tr = PianoTracker::new();
            let mut ev = Vec::new();
            for c in x.chunks(RATE as usize / 2) {
                ev.extend(tr.push(c));
            }
            ev.extend(tr.flush());
            let est = events_to_notes(&ev);
            let mine: Vec<&Note> = est.iter().filter(|e| e.pitch == pitch).collect();
            assert_eq!(mine.len(), 1, "edge {dt:+} pitch {pitch}: {}", names(&est));
            assert!((mine[0].t - gt[0].t).abs() <= TOL, "edge {dt:+} pitch {pitch}: {}", names(&est));
        }
    }
}

/// RED (F5). The live path (44.1 kHz stereo in odd pieces -> Downmix -> tracker) must give the same
/// notes as `transcribe` of the same 44.1 kHz file. Measured: 28 vs 29 notes and different
/// durations. Root cause: `to_rate` drops the decimator's 16-sample group delay (DELAY_OUT), the live
/// `Downmix` does not, so the live grid sits 0.73 ms later. Dropping DELAY_OUT samples from the live
/// stream makes live == offline bit for bit (checked).
#[test]
fn brk_live_path_matches_offline_44k() {
    let gt = piece();
    let mono = synth_at(&gt, 5.0, 44100, 18000.0, 5);
    let off = transcribe(&mono, 44100);
    let stereo: Vec<f32> = mono.iter().flat_map(|&v| [v, v]).collect();
    let mut dm = Downmix::new();
    let mut tr = PianoTracker::new();
    let mut ev = Vec::new();
    let mut s = 7u64;
    let mut i = 0;
    while i < stereo.len() {
        let step = 2 * (1 + (lcg(&mut s) * 900.0) as usize);
        let j = (i + step).min(stereo.len());
        ev.extend(tr.push(&dm.push(&stereo[i..j])));
        i = j;
    }
    ev.extend(tr.flush());
    let live = events_to_notes(&ev);
    assert_eq!(live.len(), off.len(), "live {}\noffline {}", names(&live), names(&off));
    for (a, b) in live.iter().zip(&off) {
        assert_eq!(a.pitch, b.pitch, "live {}\noffline {}", names(&live), names(&off));
        assert!((a.t - b.t).abs() < 0.002, "{a:?} vs {b:?}");
    }
}

// ---------------------------------------------------------------------------------------------
// Stuck notes, overlaps, event pairing
// ---------------------------------------------------------------------------------------------

/// RED (F2). Staccato re-strikes of one key with short gaps: the tracker can start the second note
/// before the first one's Off (the onset search reaches back up to 16 frames, bounded only by the
/// previous *onset*, not the previous Off), so the note list holds overlapping same-pitch notes.
/// In an SMF (or any receiver that orders by time) the first note's Off then kills the second.
#[test]
fn brk_restrike_after_short_gap_no_same_pitch_overlap() {
    let mut bad = Vec::new();
    for pitch in [36u8, 48, 60, 72, 84] {
        for gap_ms in (10..=150).step_by(10) {
            for dur in [0.12f64, 0.25] {
                let g = gap_ms as f64 / 1000.0;
                let gt = vec![n(0.3, dur, pitch, 90), n(0.3 + dur + g, 0.3, pitch, 90), n(0.3 + 2.0 * dur + 2.0 * g + 0.3, 0.3, pitch, 60)];
                let x = synth_piano(&gt, 2.5, 13);
                let ev = run_events(&x);
                if let Err(e) = check_events(&ev) {
                    bad.push(format!("pitch {pitch} gap {gap_ms} ms dur {dur}: {e}"));
                }
            }
        }
    }
    assert!(bad.is_empty(), "{} of 150 cases: {}", bad.len(), bad.join("\n"));
}

/// GREEN probe. Event pairing holds on adversarial inputs: every On gets exactly one Off at or after
/// it, nothing is left sounding after flush. (Same-pitch overlaps are F2's red test; the dense
/// random case below hits them too, e.g. 56 at 5.817..5.921 and 5.828..6.037.)
#[test]
fn brk_events_pair_up_on_adversarial_input() {
    let mut cases: Vec<(&str, Vec<f32>)> = Vec::new();
    cases.push(("piece", synth_piano(&piece(), 5.0, 1)));
    let mut s = 99u64;
    let mut dense = Vec::new();
    for i in 0..120 {
        dense.push(n(0.05 * i as f64, 0.05 + 0.4 * lcg(&mut s) as f64, 30 + (lcg(&mut s) * 70.0) as u8, 40 + (lcg(&mut s) * 80.0) as u8));
    }
    cases.push(("dense random", synth_piano(&dense, 7.0, 2)));
    cases.push(("vibrato", held_tone(3.0, |t| 69.0 + 0.6 * (std::f64::consts::TAU * 5.5 * t).sin(), |_| 1.0)));
    cases.push(("glide", held_tone(3.0, |t| 48.0 + 8.0 * t, |_| 1.0)));
    for (name, x) in cases {
        let ev = run_events(&x);
        check_events_opt(&ev, false).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}

// ---------------------------------------------------------------------------------------------
// Flicker on sustained, non-piano sounds (the song input is "everything pitched")
// ---------------------------------------------------------------------------------------------

/// GREEN probe. A steady organ-like tone held for 8 s is one note, not a chain of re-strikes.
#[test]
fn brk_held_tone_is_one_note() {
    for midi in [40.0f64, 57.0, 69.0, 81.0] {
        let x = held_tone(8.0, |_| midi, |_| 1.0);
        let est = transcribe(&x, RATE);
        let mine: Vec<&Note> = est.iter().filter(|e| e.pitch as f64 == midi).collect();
        assert_eq!(mine.len(), 1, "midi {midi}: {}", names(&est));
        assert!(mine[0].dur > 7.5, "midi {midi}: {}", names(&est));
        assert!(est.len() == 1, "midi {midi}: extra notes {}", names(&est));
    }
}

/// GREEN probe. A sung / bowed note with ordinary vibrato (±30 or ±50 cents at 5.5 Hz) must stay one
/// note, with no neighbour key flickering in.
#[test]
fn brk_vibrato_note_does_not_flicker() {
    let mut fails = Vec::new();
    for (midi, cents) in [(57.0f64, 30.0f64), (69.0, 30.0), (69.0, 50.0), (76.0, 40.0)] {
        let x = held_tone(3.0, |t| midi + cents / 100.0 * (std::f64::consts::TAU * 5.5 * t).sin(), |_| 1.0);
        let est = transcribe(&x, RATE);
        let on_key = est.iter().filter(|e| e.pitch as f64 == midi).count();
        if on_key != 1 || est.len() > 1 {
            fails.push(format!("midi {midi} ±{cents}c: {} notes ({on_key} on the key): {}", est.len(), names(&est)));
        }
    }
    assert!(fails.is_empty(), "{}", fails.join("\n"));
}

/// RED (F4). A held tone with tremolo (amplitude 0.5..1 at 5 Hz, e.g. a Leslie organ or a
/// tremolo guitar, a string swell) must not re-strike on every swell. Measured: after ~2 s every
/// swell is a new note (9..11 notes for 1). Root cause: the re-strike gate is relative only. During a
/// steady tone `gf_mean` decays toward the tone's tiny frame-to-frame flux, so a gentle swell reads
/// gflux >= GF_ONSET = 2, and a 0.5 -> 1.0 swell lifts all 4 partials by more than RESTRIKE = 1.4.
#[test]
fn brk_tremolo_does_not_restrike() {
    let mut fails = Vec::new();
    for midi in [48.0f64, 64.0, 76.0] {
        let x = held_tone(4.0, |_| midi, |t| 0.75 + 0.25 * (std::f64::consts::TAU * 5.0 * t).sin());
        let est = transcribe(&x, RATE);
        if est.len() != 1 {
            fails.push(format!("midi {midi}: {} notes: {}", est.len(), names(&est)));
        }
    }
    assert!(fails.is_empty(), "{}", fails.join("\n"));
}

// ---------------------------------------------------------------------------------------------
// Level, DC, clipping
// ---------------------------------------------------------------------------------------------

/// RED (F7). Level sweep on the reference piece: +10 / 0 / -20 / -40 dB hold F1 0.692..0.682, but
/// -50 dB drops to 0.600 (16 notes for 24; -40 dB already finds 20 instead of 28). The absolute
/// floors (ABS_FLOOR = 1e-5, the 1e-5 in the flux log) make a quiet source lose notes although every
/// other threshold is relative.
#[test]
fn brk_level_invariance() {
    let gt = piece();
    let x = synth_piano(&gt, 5.0, 4);
    let base = note_f1(&transcribe(&x, RATE), &gt, TOL).2;
    let mut fails = Vec::new();
    for db in [10.0f32, 0.0, -20.0, -40.0, -50.0] {
        let g = 10f32.powf(db / 20.0);
        let y: Vec<f32> = x.iter().map(|v| v * g).collect();
        let est = transcribe(&y, RATE);
        let f = note_f1(&est, &gt, TOL).2;
        eprintln!("METRIC level {db:+} dB: F1 {f:.3} ({} notes, base {base:.3})", est.len());
        if f < base - 0.05 {
            fails.push(format!("{db:+} dB: F1 {f:.3} vs {base:.3} at 0 dB ({} notes)", est.len()));
        }
    }
    assert!(fails.is_empty(), "{}", fails.join("\n"));
}

/// RED (F6). A -60 dBFS DC offset (0.001) under the music changes the transcription (3 of
/// 28 notes differ: the bass re-strikes at 0.65 / 1.01 s vanish). A lone DC step and a click train
/// are correctly not notes. Root cause: the DC step at the first sample seeds the global-flux running
/// mean `gf_mean` at 154 (vs 0.001 without DC; steady state ~38), still 50 after 1 s, so the
/// flux onsets of 2.05 / 2.23 at 0.64 s read 1.17 / 1.29 and the GF_ONSET = 2.0 gate stays shut.
#[test]
fn brk_dc_offset_and_step() {
    let gt = piece();
    let x = synth_piano(&gt, 5.0, 4);
    let clean = transcribe(&x, RATE);
    let y: Vec<f32> = x.iter().map(|v| v + 0.001).collect();
    let dc = transcribe(&y, RATE);
    assert_eq!(dc.iter().map(|n| n.pitch).collect::<Vec<_>>(), clean.iter().map(|n| n.pitch).collect::<Vec<_>>(), "dc {}\nclean {}", names(&dc), names(&clean));
    let step: Vec<f32> = (0..3 * RATE as usize).map(|i| if i > RATE as usize { 0.5 } else { 0.0 }).collect();
    let e = transcribe(&step, RATE);
    assert!(e.is_empty(), "DC step: {}", names(&e));
    let clicks: Vec<f32> = (0..3 * RATE as usize).map(|i| if i % 2205 == 0 { 0.9 } else { 0.0 }).collect();
    let e = transcribe(&clicks, RATE);
    assert!(e.is_empty(), "click train at 10 Hz: {}", names(&e));
}

/// GREEN probe. Hard clipping 6 dB over the peak (a hot master into ±1) adds odd harmonics; it
/// must not invent keys, and must not lose much. Measured P 0.750 R 0.625 vs clean P 0.643 R 0.750.
#[test]
fn brk_clipped_input_no_ghosts() {
    let gt = piece();
    let x = synth_piano(&gt, 5.0, 4);
    let peak = x.iter().fold(0f32, |a, v| a.max(v.abs()));
    let clean = note_f1(&transcribe(&x, RATE), &gt, TOL);
    let y: Vec<f32> = x.iter().map(|v| (v * 2.0 / peak).clamp(-1.0, 1.0)).collect();
    let est = transcribe(&y, RATE);
    let (p, r, f) = note_f1(&est, &gt, TOL);
    eprintln!("METRIC clipped +6 dB: P {p:.3} R {r:.3} F1 {f:.3} (clean P {:.3} R {:.3})", clean.0, clean.1);
    assert!(p >= clean.0 - 0.1, "clipping ghosts: P {p:.3} vs clean {:.3}: {}", clean.0, names(&est));
    assert!(r >= clean.1 - 0.2, "clipping losses: R {r:.3} vs clean {:.3}: {}", clean.1, names(&est));
}

// ---------------------------------------------------------------------------------------------
// Rates, resampling, empty / short / non-finite input
// ---------------------------------------------------------------------------------------------

/// GREEN probe. The same music at the common file rates gives about the same F1 as at 22 050 Hz
/// (measured 0.731..0.826 vs 0.784; the spread is the grid-phase sensitivity, see
/// brk_grid_phase_sensitivity; the FluidSynth fixtures agree within 0.02 at 44.1 / 48 kHz).
#[test]
fn brk_rates_agree() {
    let gt = piece();
    let mut fails = Vec::new();
    let reff = {
        let x = synth_at(&gt, 5.0, 22050, 20000.0, 9);
        note_f1(&transcribe(&x, 22050), &gt, TOL).2
    };
    for rate in [16000u32, 32000, 44100, 48000, 88200, 96000] {
        let x = synth_at(&gt, 5.0, rate, 20000.0, 9);
        let est = transcribe(&x, rate);
        let f = note_f1(&est, &gt, TOL).2;
        eprintln!("METRIC rate {rate}: F1 {f:.3} (22050: {reff:.3})");
        if f < reff - 0.08 {
            fails.push(format!("{rate} Hz: F1 {f:.3} vs {reff:.3}: {}", names(&est)));
        }
    }
    assert!(fails.is_empty(), "{}", fails.join("\n"));
}

/// GREEN probe. Content above the analysis Nyquist in a 48 kHz file (a 19.96 kHz tone, which would
/// alias onto C7 = 2093 Hz) must not become a note.
#[test]
fn brk_no_alias_notes_from_ultrasonic_content() {
    for (rate, f) in [(48000u32, 22050.0 - 2093.0), (44100, 22050.0 - 1046.5), (96000, 22050.0 + 2093.0)] {
        let x: Vec<f32> = (0..(3 * rate) as usize).map(|i| (0.3 * (std::f64::consts::TAU * f * i as f64 / rate as f64).sin()) as f32).collect();
        let e = transcribe(&x, rate);
        assert!(e.is_empty(), "{rate} Hz file, {f:.0} Hz tone: {}", names(&e));
    }
}

/// GREEN probe. No panic and no notes on empty, tiny and silent inputs at any rate; flush and
/// all_off are idempotent.
#[test]
fn brk_empty_and_short_inputs() {
    for rate in [8000u32, 22050, 44100, 48000, 96000, 192000] {
        for len in [0usize, 1, 2, 3, 255, 256, 2047, 2048, 2049, 4097] {
            let x = vec![0f32; len];
            assert!(transcribe(&x, rate).is_empty(), "rate {rate} len {len}");
        }
    }
    let mut tr = PianoTracker::new();
    assert!(tr.push(&[]).is_empty());
    assert!(tr.flush().is_empty());
    assert!(tr.flush().is_empty());
    assert!(tr.all_off(0.0).is_empty());
    // a 70 ms note right at the very end of the stream still comes out (flush pads with zeros)
    let x = synth_piano(&[n(0.2, 0.3, 60, 90)], 0.29, 1);
    let e = transcribe(&x, RATE);
    assert_eq!(e.iter().filter(|n| n.pitch == 60).count(), 1, "{}", names(&e));
}

/// GREEN probe. NaN / inf samples (a broken upstream plugin) must not panic, must not leave notes
/// stuck, and the tracker must recover once they have left the longest window.
#[test]
fn brk_non_finite_input_recovers() {
    let gt = vec![n(1.5, 0.5, 60, 90), n(2.5, 0.5, 67, 90)];
    let mut x = synth_piano(&gt, 3.5, 2);
    for (i, v) in x.iter_mut().enumerate().skip(RATE as usize / 2).take(500) {
        *v = if i % 3 == 0 { f32::NAN } else if i % 3 == 1 { f32::INFINITY } else { f32::NEG_INFINITY };
    }
    let ev = run_events(&x);
    check_events(&ev).unwrap();
    let est = events_to_notes(&ev);
    let f = note_f1(&est, &gt, TOL).2;
    assert!(f >= 0.99, "after a NaN burst: {}", names(&est));
}

// ---------------------------------------------------------------------------------------------
// Velocity
// ---------------------------------------------------------------------------------------------

/// GREEN probe. Alternating soft (vel 40) and loud (vel 110) strikes of the same key: every loud
/// note must come out louder than the soft notes next to it.
#[test]
fn brk_velocity_orders_soft_and_loud() {
    let mut gt = Vec::new();
    for i in 0..16 {
        gt.push(n(0.3 + 0.4 * i as f64, 0.3, 60, if i % 2 == 0 { 40 } else { 110 }));
    }
    let est = transcribe(&synth_piano(&gt, 7.0, 3), RATE);
    assert_eq!(est.len(), gt.len(), "{}", names(&est));
    for w in est.windows(2) {
        let (soft, loud) = if w[0].vel < w[1].vel { (w[0].vel, w[1].vel) } else { (w[1].vel, w[0].vel) };
        assert!(loud >= soft + 10, "{}", names(&est));
    }
}

/// Equal-velocity notes across the keyboard should come out at roughly equal velocity. Probe: an
/// ascending chromatic scale, all vel 80, prints the spread.
#[test]
fn brk_velocity_flat_across_registers() {
    let gt: Vec<Note> = (24u8..=103).map(|p| n(0.3 + 0.25 * (p - 24) as f64, 0.2, p, 80)).collect();
    let est = transcribe(&synth_piano(&gt, 0.3 + 0.25 * 81.0, 3), RATE);
    let v: Vec<u8> = est.iter().map(|n| n.vel).collect();
    let (lo, hi) = (*v.iter().min().unwrap(), *v.iter().max().unwrap());
    eprintln!("METRIC velocity spread over C1..G7 at vel 80: {lo}..{hi} ({} notes) {v:?}", est.len());
    assert!(hi - lo <= 50, "equal-velocity scale reads {lo}..{hi}: {v:?}");
}

// ---------------------------------------------------------------------------------------------
// Long runs, CPU on pathological input
// ---------------------------------------------------------------------------------------------

/// GREEN probe. 15 minutes of a note every 0.5 s, fed in 10 ms pushes: onsets do not drift, the
/// input buffer stays bounded, every note is found.
#[test]
fn brk_long_run_no_drift_bounded_memory() {
    let tile = synth_piano(&[n(0.05, 0.3, 64, 90)], 0.5, 1);
    let reps = 1800;
    let mut tr = PianoTracker::new();
    let mut ev = Vec::new();
    let mut max_buf = 0;
    for _ in 0..reps {
        for c in tile.chunks(220) {
            ev.extend(tr.push(c));
        }
        max_buf = max_buf.max(tr.buf.len());
    }
    ev.extend(tr.flush());
    let est = events_to_notes(&ev);
    assert_eq!(est.len(), reps, "first {}", names(&est[..est.len().min(5)]));
    // onsets are on the 11.6 ms frame grid, and 0.5 s is 43.07 hops, so compare notes at the same
    // grid phase: 256 tiles are exactly 11025 hops
    let err = |i: usize| est[i].t - (0.05 + 0.5 * i as f64);
    let mut worst = 0f64;
    for i in 0..256 {
        worst = worst.max((err(i + 256 * 6) - err(i)).abs());
    }
    let (lo, hi) = (0..reps).map(err).fold((f64::MAX, f64::MIN), |(a, b), e| (a.min(e), b.max(e)));
    eprintln!("METRIC long run: onset error range {:.2}..{:.2} ms, drift over 12.8 min {:.4} ms, max buffer {max_buf} samples", lo * 1e3, hi * 1e3, worst * 1e3);
    assert!(worst < 1e-6, "drift {:.3} ms", worst * 1e3);
    assert!(max_buf < 4 * SIZES[0] + 220, "buffer grew to {max_buf}");
}

/// CPU on pathological input, ms per 0.5 s (plan budget 5 ms), best of 3 runs, release only.
/// Inputs: white noise at full scale, all 88 keys struck together every 0.5 s, 30 random notes/s,
/// a 50 Hz click train, a dense sweep cluster.
#[test]
fn brk_cpu_pathological_inputs() {
    let secs = 6.0;
    let len = (secs * RATE as f64) as usize;
    let mut s = 5u64;
    let mut cases: Vec<(&str, Vec<f32>)> = Vec::new();
    cases.push(("white 0 dBFS", (0..len).map(|_| 2.0 * lcg(&mut s) - 1.0).collect()));
    let mut all88 = Vec::new();
    for i in 0..12 {
        for p in 21u8..=108 {
            all88.push(n(0.5 * i as f64, 0.45, p, 60));
        }
    }
    cases.push(("88-key cluster", synth_piano(&all88, secs, 1)));
    let mut rnd = Vec::new();
    for i in 0..(30.0 * secs) as usize {
        rnd.push(n(i as f64 / 30.0, 0.3, 21 + (lcg(&mut s) * 87.0) as u8, 90));
    }
    cases.push(("30 notes/s", synth_piano(&rnd, secs, 2)));
    cases.push(("click 50 Hz", (0..len).map(|i| if i % 441 == 0 { 1.0 } else { 0.0 }).collect()));
    let mut fails = Vec::new();
    for (name, x) in &cases {
        let mut best = f64::MAX;
        for _ in 0..3 {
            let mut tr = PianoTracker::new();
            let t = std::time::Instant::now();
            for c in x.chunks(RATE as usize / 2) {
                std::hint::black_box(tr.push(c));
            }
            best = best.min(t.elapsed().as_secs_f64() * 1000.0 / (secs / 0.5));
        }
        eprintln!("METRIC cpu {name}: {best:.2} ms per 0.5 s");
        if !cfg!(debug_assertions) && best > 5.0 {
            fails.push(format!("{name}: {best:.2} ms per 0.5 s"));
        }
    }
    assert!(fails.is_empty(), "{}", fails.join("; "));
}

// ---------------------------------------------------------------------------------------------
// Lanes, WAV reader
// ---------------------------------------------------------------------------------------------

/// GREEN probe. Lane edges exactly as the plan: 47 Bass, 48 Mid, 72 Mid, 73 High, and the SMF
/// split puts each on its own track / channel.
#[test]
fn brk_lane_boundaries() {
    assert_eq!([lane(0), lane(21), lane(47)], [Lane::Bass; 3]);
    assert_eq!([lane(48), lane(60), lane(72)], [Lane::Mid; 3]);
    assert_eq!([lane(73), lane(108), lane(127)], [Lane::High; 3]);
    let notes: Vec<Note> = [47u8, 48, 72, 73].iter().enumerate().map(|(i, &p)| n(0.1 * i as f64, 0.5, p, 90)).collect();
    let parsed = crate::notes::parse_smf(&crate::notes::piano_midi(&notes, true, Some(0.0))).unwrap();
    let got: Vec<(u8, usize, u8)> = parsed.iter().map(|p| (p.note.pitch, p.track, p.ch)).collect();
    assert_eq!(got, vec![(47, 1, 0), (48, 2, 1), (72, 2, 1), (73, 3, 2)]);
}

/// RED (F8). A WAV whose fmt chunk says 0 bits per sample panics `read_wav` (chunks_exact(0)), which
/// aborts `wavelink-mixer piano --wav` and kills the replay worker, instead of returning an error.
#[test]
fn brk_read_wav_zero_bits_is_an_error_not_a_panic() {
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&36u32.to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes()); // PCM
    b.extend_from_slice(&1u16.to_le_bytes()); // mono
    b.extend_from_slice(&22050u32.to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes());
    b.extend_from_slice(&0u16.to_le_bytes());
    b.extend_from_slice(&0u16.to_le_bytes()); // 0 bits
    b.extend_from_slice(b"data");
    b.extend_from_slice(&4u32.to_le_bytes());
    b.extend_from_slice(&[1, 2, 3, 4]);
    let r = std::panic::catch_unwind(|| read_wav(&b));
    assert!(matches!(r, Ok(Err(_))), "read_wav panicked or accepted a 0-bit WAV");
}

// ---------------------------------------------------------------------------------------------
// SMF writer / parser
// ---------------------------------------------------------------------------------------------

/// RED (F2, writer side). Two overlapping notes of the same pitch on one track (the tracker produces
/// them, see brk_restrike_after_short_gap_no_same_pitch_overlap; take_notes can too after a mid-run
/// save). `smf_bytes` writes On A, On B, Off A, Off B, so every player (and our own parser) ends B at
/// A's Off. A writer should cut A at B's start so B keeps its length.
#[test]
fn brk_smf_overlapping_same_pitch_keeps_second_note() {
    let notes = vec![n(0.0, 1.0, 60, 90), n(0.5, 1.0, 60, 80)];
    let p = crate::notes::parse_smf(&crate::notes::piano_midi(&notes, false, Some(0.0))).unwrap();
    let b = p.iter().find(|x| (x.note.t - 0.5).abs() < 0.002).expect("second note missing");
    assert!((b.note.dur - 1.0).abs() < 0.002, "second note cut to {:.3} s by the first note's Off: {p:?}", b.note.dur);
}

/// GREEN probe. The parser honours running status and note-on velocity 0 as note-off, and a
/// writer round trip keeps notes at the lane edges, zero-length notes and long notes.
#[test]
fn brk_smf_running_status_and_roundtrip() {
    let mut f = Vec::new();
    f.extend_from_slice(b"MThd");
    f.extend_from_slice(&6u32.to_be_bytes());
    f.extend_from_slice(&0u16.to_be_bytes());
    f.extend_from_slice(&1u16.to_be_bytes());
    f.extend_from_slice(&480u16.to_be_bytes());
    // 0: on 60, 0: (running) on 64, 480: (running) 60 vel 0, 0: (running) 64 vel 0, end
    let body: Vec<u8> = vec![0x00, 0x90, 60, 100, 0x00, 64, 90, 0x83, 0x60, 60, 0, 0x00, 64, 0, 0x00, 0xff, 0x2f, 0x00];
    f.extend_from_slice(b"MTrk");
    f.extend_from_slice(&(body.len() as u32).to_be_bytes());
    f.extend_from_slice(&body);
    let p = crate::notes::parse_smf(&f).unwrap();
    assert_eq!(p.len(), 2, "{p:?}");
    assert!(p.iter().all(|x| (x.note.dur - 0.5).abs() < 1e-9), "{p:?}");
    let notes = vec![n(0.0, 0.0, 21, 1), n(0.0, 600.0, 108, 127), n(1.0, 0.25, 47, 64), n(1.0, 0.25, 73, 64)];
    let q = crate::notes::parse_smf(&crate::notes::piano_midi(&notes, false, Some(0.0))).unwrap();
    assert_eq!(q.len(), 4);
    let long = q.iter().find(|x| x.note.pitch == 108).unwrap();
    assert!((long.note.dur - 600.0).abs() < 0.002);
    let zero = q.iter().find(|x| x.note.pitch == 21).unwrap();
    assert!(zero.note.dur > 0.0 && zero.note.dur < 0.002);
}

/// GREEN probe (context for F6). `gf_mean` is seeded by the first frame, so a stream that starts
/// mid-sound starts with a too-high flux mean. With strong, detached repeats the effect is small
/// (10 vs 11 found on C3, equal elsewhere); pinned so it cannot get worse.
#[test]
fn brk_restrikes_found_when_stream_starts_mid_sound() {
    let mut fails = Vec::new();
    for pitch in [48u8, 60, 72] {
        // a held chord already sounding at sample 0, and a key repeated 6 times per second over it
        let mut gt = vec![n(0.0, 2.6, pitch - 12, 80), n(0.0, 2.6, pitch + 4, 70)];
        for i in 0..14 {
            gt.push(n(0.1 + i as f64 / 6.0, 0.1, pitch, 95));
        }
        let x = synth_piano(&gt, 2.8, 5);
        let mut pad = vec![0f32; RATE as usize / 2];
        pad.extend_from_slice(&x);
        let count = |est: &[Note], off: f64| est.iter().filter(|e| e.pitch == pitch && e.t >= off + 0.05 && e.t < off + 2.4).count();
        let at0 = count(&transcribe(&x, RATE), 0.0);
        let padded = count(&transcribe(&pad, RATE), 0.5);
        eprintln!("METRIC mid-sound start, key {pitch}: {at0} repeats found from t=0, {padded} after 0.5 s silence (13 played)");
        if at0 + 2 < padded {
            fails.push(format!("key {pitch}: {at0} repeats when the stream starts mid-sound vs {padded} with leading silence"));
        }
    }
    assert!(fails.is_empty(), "{}", fails.join("\n"));
}

/// GREEN probe (context for F5). The tracker is sensitive to the sub-hop grid phase: shifting the
/// piece by 0..256 samples moves F1 between 0.679 and 0.760 (note count 25..29). This is why the
/// uncompensated 0.73 ms live decimator delay (F5) changes notes. Pinned loosely so it can't get worse.
#[test]
fn brk_grid_phase_sensitivity() {
    let gt = piece();
    let x = synth_piano(&gt, 5.0, 4);
    let mut fs = Vec::new();
    for sh in (0..256).step_by(16) {
        let mut y = vec![0f32; sh];
        y.extend_from_slice(&x);
        let g2: Vec<Note> = gt.iter().map(|g| Note { t: g.t + sh as f64 / RATE as f64, ..g.clone() }).collect();
        fs.push(note_f1(&transcribe(&y, RATE), &g2, TOL).2);
    }
    let (lo, hi) = fs.iter().fold((1f64, 0f64), |(a, b), &f| (a.min(f), b.max(f)));
    eprintln!("METRIC grid phase: F1 {lo:.3}..{hi:.3} over 16 sub-hop shifts");
    assert!(hi - lo <= 0.12, "F1 moves {lo:.3}..{hi:.3} with a sub-millisecond start offset: {fs:?}");
}


// ---------------------------------------------------------------------------------------------
// FluidSynth adversarial fixtures (offline render to a file, never to a device), #[ignore]
// ---------------------------------------------------------------------------------------------

/// Renders `notes` with GM `prog` through FluidSynth (FluidR3_GM, offline -F) at `rate`, into
/// ~/.cache/wavelink-piano-breaker/, and returns the mono PCM.
fn fluid(name: &str, notes: &[Note], prog: u8, rate: u32) -> Vec<f32> {
    let dir = crate::config::glib_home().join(".cache/wavelink-piano-breaker");
    std::fs::create_dir_all(&dir).unwrap();
    let mid = dir.join(format!("{name}.mid"));
    let wav = dir.join(format!("{name}-{rate}.wav"));
    let spec = crate::notes::TrackSpec { ch: 0, prog: Some(prog), name: None };
    std::fs::write(&mid, crate::notes::smf_bytes(&[(spec, notes.to_vec())], Some(0.0))).unwrap();
    let font = crate::notes::synth_font().expect("FluidR3_GM.sf2 missing");
    let bin = crate::notes::synth_bin();
    let args: Vec<String> = ["-ni", "-q", "-g", "0.6", "-r", &rate.to_string(), "-F"]
        .iter()
        .map(|s| s.to_string())
        .chain([wav.display().to_string(), font.display().to_string(), mid.display().to_string()])
        .collect();
    let st = if std::path::Path::new(bin).exists() {
        std::process::Command::new(bin).args(&args).status()
    } else {
        std::process::Command::new("distrobox-host-exec").arg("/home/linuxbrew/.linuxbrew/bin/fluidsynth").args(&args).status()
    };
    assert!(st.map(|s| s.success()).unwrap_or(false), "fluidsynth failed for {name}");
    let (pcm, r) = read_wav(&std::fs::read(&wav).unwrap()).unwrap();
    assert_eq!(r, rate);
    pcm
}

fn repeats(pitch: u8, rate_hz: f64, gap: f64, count: usize, t0: f64) -> Vec<Note> {
    (0..count).map(|i| n(t0 + i as f64 / rate_hz, 1.0 / rate_hz - gap, pitch, 90)).collect()
}

/// RED (F1, FluidSynth). Real-piano repeated notes played legato (the next strike 5..20 ms after the
/// previous note-off, as a MIDI piano part writes them) are mostly merged into one long note. The
/// plan's "fast repeated notes (8/s): every re-strike is detected" is only met with the PR's 90 ms
/// holds (a ~35 ms gap). Measured per case in the METRIC lines.
#[test]
#[ignore]
fn brk_fluidsynth_legato_repeats() {
    let mut fails = Vec::new();
    for (pitch, hz, gap) in [(60u8, 8.0f64, 0.005f64), (60, 4.0, 0.010), (72, 6.0, 0.020), (48, 4.0, 0.010), (60, 8.0, 0.035)] {
        let gt = repeats(pitch, hz, gap, 12, 0.5);
        let name = format!("legato-{pitch}-{hz}-{}", (gap * 1000.0) as u32);
        let est = transcribe(&fluid(&name, &gt, 0, 22050), 22050);
        let (p, r, f) = note_f1(&est, &gt, TOL);
        eprintln!("METRIC legato repeats key {pitch} {hz}/s gap {:.0} ms: P {p:.3} R {r:.3} F1 {f:.3} ({} est)", gap * 1e3, est.len());
        if r < 0.9 {
            fails.push(format!("key {pitch} {hz}/s gap {:.0} ms: R {r:.3}", gap * 1e3));
        }
    }
    assert!(fails.is_empty(), "{}", fails.join("; "));
}

/// RED (F2, FluidSynth). Same-pitch notes with short gaps on a real piano: no overlapping notes for
/// the same key in the output (the SMF writer then cuts the second one).
#[test]
#[ignore]
fn brk_fluidsynth_no_same_pitch_overlap() {
    let mut bad = Vec::new();
    for pitch in [36u8, 43, 48, 55, 60, 67, 72, 84] {
        let mut gt = Vec::new();
        let mut t = 0.4;
        for gap_ms in [10u32, 20, 30, 50, 80, 120] {
            gt.push(n(t, 0.15, pitch, 90));
            t += 0.15 + gap_ms as f64 / 1000.0;
            gt.push(n(t, 0.25, pitch, 75));
            t += 0.6;
        }
        let est = transcribe(&fluid(&format!("gaps-{pitch}"), &gt, 0, 22050), 22050);
        let mut ev = Vec::new();
        for e in &est {
            ev.push(Ev::On { t: e.t, pitch: e.pitch, vel: e.vel });
            ev.push(Ev::Off { t: e.t + e.dur, pitch: e.pitch });
        }
        for p in 0..128u8 {
            let v: Vec<&Note> = est.iter().filter(|x| x.pitch == p).collect();
            for w in v.windows(2) {
                if w[0].t + w[0].dur > w[1].t + 1e-9 {
                    bad.push(format!("key {pitch}: {p} {:.3}..{:.3} overlaps {:.3}..{:.3}", w[0].t, w[0].t + w[0].dur, w[1].t, w[1].t + w[1].dur));
                }
            }
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// The PR's own fixtures (scales / chords / piece) rendered at 44.1 and 48 kHz, as the by-ear script
/// and real files are, must score like the 22.05 kHz renders the PR measured. Probe.
#[test]
#[ignore]
fn brk_fluidsynth_fixture_rates() {
    let mut fails = Vec::new();
    for (name, gt) in super::tests::fixture_pieces() {
        let mut fs = Vec::new();
        for rate in [22050u32, 44100, 48000] {
            let est = transcribe(&fluid(&format!("fix-{name}"), &gt, 0, rate), rate);
            fs.push(note_f1(&est, &gt, TOL).2);
        }
        eprintln!("METRIC fixture {name}: F1 22.05k {:.3} / 44.1k {:.3} / 48k {:.3}", fs[0], fs[1], fs[2]);
        if fs[1] < fs[0] - 0.05 || fs[2] < fs[0] - 0.05 {
            fails.push(format!("{name}: {fs:?}"));
        }
    }
    assert!(fails.is_empty(), "{}", fails.join("; "));
}

/// Sustained non-piano instruments (the input is "everything pitched"): long notes must not be
/// chopped into many short ones. 8 notes of 0.9 s each; counts output notes on the right keys and
/// everything else. Probe, with GM programs: 19 church organ, 40 violin, 48 strings, 52 choir, 73 flute.
#[test]
#[ignore]
fn brk_fluidsynth_sustained_instruments_flicker() {
    let mel = [60u8, 62, 64, 65, 67, 69, 71, 72];
    let gt: Vec<Note> = mel.iter().enumerate().map(|(i, &p)| n(0.5 + 1.0 * i as f64, 0.9, p, 90)).collect();
    let mut fails = Vec::new();
    for prog in [19u8, 40, 48, 52, 73] {
        let est = transcribe(&fluid(&format!("sustain-{prog}"), &gt, prog, 22050), 22050);
        let on_key = est.iter().filter(|e| gt.iter().any(|g| g.pitch == e.pitch && e.t >= g.t - TOL && e.t < g.t + g.dur)).count();
        let (p, r, f) = note_f1(&est, &gt, TOL);
        eprintln!("METRIC sustained prog {prog}: {} notes for 8 ({on_key} on the played keys), P {p:.3} R {r:.3} F1 {f:.3}: {}", est.len(), names(&est));
        if on_key > 12 {
            fails.push(format!("prog {prog}: {on_key} notes on 8 held keys"));
        }
    }
    assert!(fails.is_empty(), "{}", fails.join("; "));
}

/// RED (F1, pure Rust, runs in CI). The same as brk_fluidsynth_legato_repeats without FluidSynth:
/// the PR's own repeat pattern (8/s, keys held 90 ms) but with a 50 ms linear attack ramp instead of
/// the synth's 3 ms (it models the FluidSynth piano, whose partial energy peaks ~5 frames after the
/// flux onset; 20 ms still passes, 35 ms already loses half the C4 repeats).
/// Root cause (traced frame by frame on the FluidSynth render of C4 8/s): a re-strike needs the
/// short-window energy at frame f to exceed 1.4x its recent minimum AND a global-flux onset in
/// frames f-2..f. The flux onset fires at the attack (gflux 6.75 / 4.01 at f54 / f55), but the energy
/// reaches 1.42x only at f59, 5 frames later, after the 3-frame coincidence window has closed; it
/// never fires again. With a 3 ms attack both happen in the same frame, which is why the PR's test
/// passes. Measured R 0.75 / 0.25 / 0.33 on C3 / C4 / C5 (FluidSynth C4: 1 note for 12 strikes).
#[test]
fn brk_repeats_with_realistic_attack() {
    let mut fails = Vec::new();
    for pitch in [48u8, 60, 72] {
        let gt: Vec<Note> = (0..12).map(|i| n(0.5 + i as f64 / 8.0, 0.09, pitch, 90)).collect();
        let x = synth_full(&gt, 2.5, RATE, 20000.0, 3, 0.050);
        let est = transcribe(&x, RATE);
        let (_, r, _) = note_f1(&est, &gt, TOL);
        eprintln!("METRIC repeats 8/s, 50 ms attack, key {pitch}: R {r:.3} ({} est)", est.len());
        if r < 0.9 {
            fails.push(format!("key {pitch}: R {r:.3}: {}", names(&est)));
        }
    }
    assert!(fails.is_empty(), "{}", fails.join("\n"));
}

#[path = "piano_breaker2_tests.rs"]
mod round2;
