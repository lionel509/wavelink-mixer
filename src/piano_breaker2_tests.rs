//! Breaker round 2 for #23 / PR #30 (head 89b857b): the envelope-capped cancel, unclaimed-partial
//! judging, the per-key re-strike detector, the new resamplers and decimators.
use super::*;

fn sine(f: f64, secs: f64, rate: u32, amp: f64) -> Vec<f32> {
    let r = rate as f64;
    (0..(secs * r) as usize)
        .map(|i| {
            let t = i as f64 / r;
            let env = (t / 0.01).min(1.0) * ((secs - t) / 0.01).clamp(0.0, 1.0);
            (amp * env * (std::f64::consts::TAU * f * t).sin()) as f32
        })
        .collect()
}

fn hz_midi(f: f64) -> f64 {
    69.0 + 12.0 * (f / 440.0).log2()
}

/// FluidSynth render with extra settings (`-o name=value`), into ~/.cache/wavelink-piano-breaker/.
fn fluid_opts(name: &str, notes: &[Note], prog: u8, rate: u32, opts: &[&str]) -> Vec<f32> {
    let dir = crate::config::glib_home().join(".cache/wavelink-piano-breaker");
    std::fs::create_dir_all(&dir).unwrap();
    let mid = dir.join(format!("{name}.mid"));
    let wav = dir.join(format!("{name}-{rate}.wav"));
    let spec = crate::notes::TrackSpec { ch: 0, prog: Some(prog), name: None };
    std::fs::write(&mid, crate::notes::smf_bytes(&[(spec, notes.to_vec())], Some(0.0))).unwrap();
    let font = crate::notes::synth_font().expect("FluidR3_GM.sf2 missing");
    let mut args: Vec<String> = ["-ni", "-q", "-g", "0.6", "-r", &rate.to_string()].iter().map(|s| s.to_string()).collect();
    for o in opts {
        args.push("-o".into());
        args.push(o.to_string());
    }
    args.extend(["-F".to_string(), wav.display().to_string(), font.display().to_string(), mid.display().to_string()]);
    let bin = crate::notes::synth_bin();
    let st = if std::path::Path::new(bin).exists() {
        std::process::Command::new(bin).args(&args).status()
    } else {
        std::process::Command::new("distrobox-host-exec").arg("/home/linuxbrew/.linuxbrew/bin/fluidsynth").args(&args).status()
    };
    assert!(st.map(|s| s.success()).unwrap_or(false), "fluidsynth failed for {name}");
    read_wav(&std::fs::read(&wav).unwrap()).unwrap().0
}

// ---------------------------------------------------------------------------------------------
// Resamplers and the internal long-window decimators
// ---------------------------------------------------------------------------------------------

/// GREEN probe. A pure tone must only ever light the key it is on (or its neighbour when it sits between keys):
/// nothing an octave away or further (aliases of the 2:1 / 4:1 decimators that now feed the 2048 and
/// 4096 windows, or of the file resamplers). Scans 27.5 Hz .. 10.5 kHz in quarter-semitone steps at
/// 22.05 kHz, and a subset at 32 / 44.1 / 48 / 96 kHz.
#[test]
fn brk2_pure_tone_scan_no_far_notes() {
    let mut bad = Vec::new();
    let mut scan = |rate: u32, step: usize| {
        let mut q = 0usize;
        while q <= 4 * 100 {
            let m = 21.0 + q as f64 / 4.0;
            let f = 440.0 * 2f64.powf((m - 69.0) / 12.0);
            q += step;
            if f > 0.45 * rate as f64 || f > 10500.0 {
                continue;
            }
            for amp in [0.05f64, 0.7] {
                let est = transcribe(&sine(f, 1.0, rate, amp), rate);
                for e in &est {
                    if (e.pitch as f64 - m).abs() > 1.01 {
                        bad.push(format!("{rate} Hz file, {f:.1} Hz (midi {m:.2}) amp {amp}: note {} at {:.2}", e.pitch, e.t));
                    }
                }
            }
        }
    };
    scan(22050, 1);
    for rate in [32000u32, 44100, 48000, 96000] {
        scan(rate, 5);
    }
    assert!(bad.is_empty(), "{} far notes:\n{}", bad.len(), bad.join("\n"));
}

/// GREEN probe. Content between the analysis Nyquist and the file's Nyquist (cymbals, sibilance, synth air)
/// must not fold into notes, at every file rate the resamplers handle.
#[test]
fn brk2_band_edge_and_ultrasonic_tones_are_silent() {
    let mut bad = Vec::new();
    for rate in [32000u32, 44100, 48000, 96000] {
        let mut f = 10600.0;
        while f < 0.48 * rate as f64 && f < 20000.0 {
            let est = transcribe(&sine(f, 1.5, rate, 0.7), rate);
            if !est.is_empty() {
                bad.push(format!("{rate} Hz, {f:.0} Hz: {}", names(&est)));
            }
            f += 350.0;
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// GREEN probe. The reference piece at every file rate scores about like 22.05 kHz (0.870):
/// 8 k 0.792, 11.025 k 0.818 (both were ~0.5 in round 1), 16 k 0.898, 24 k 0.760, 32 k 0.826,
/// 44.1 k 0.857, 48..192 k 0.917. The spread is the known grid-phase sensitivity.
#[test]
fn brk2_rates_agree_including_odd_rates() {
    let gt = piece();
    let reff = note_f1(&transcribe(&synth_at(&gt, 5.0, 22050, 20000.0, 9), 22050), &gt, TOL).2;
    let mut bad = Vec::new();
    for rate in [8000u32, 11025, 16000, 24000, 32000, 44100, 48000, 88200, 96000, 192000] {
        let est = transcribe(&synth_at(&gt, 5.0, rate, 20000.0, 9), rate);
        let f = note_f1(&est, &gt, TOL).2;
        eprintln!("METRIC r2 rate {rate}: F1 {f:.3} (22050: {reff:.3})");
        if f < reff - 0.12 {
            bad.push(format!("{rate}: F1 {f:.3} vs {reff:.3}"));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("; "));
}

/// GREEN probe. The 44.1 kHz live path still matches offline bit for bit after the new halfband and delay skip,
/// for any chunking, including 1-frame (8-byte) pieces.
#[test]
fn brk2_live_downmix_tiny_chunks() {
    let gt = piece();
    let mono = synth_at(&gt, 5.0, 44100, 18000.0, 5);
    let off = transcribe(&mono, 44100);
    let stereo: Vec<f32> = mono.iter().flat_map(|&v| [v, v]).collect();
    for step in [2usize, 6, 882, 4410 * 2 + 2] {
        let mut dm = Downmix::new();
        let mut tr = PianoTracker::new();
        let mut ev = Vec::new();
        for c in stereo.chunks(step) {
            ev.extend(tr.push(&dm.push(c)));
        }
        ev.extend(tr.flush());
        let live = events_to_notes(&ev);
        assert_eq!(live.iter().map(|n| (n.pitch, (n.t * 1e4).round() as i64)).collect::<Vec<_>>(), off.iter().map(|n| (n.pitch, (n.t * 1e4).round() as i64)).collect::<Vec<_>>(), "step {step}");
    }
}

// ---------------------------------------------------------------------------------------------
// Envelope cap and unclaimed judging
// ---------------------------------------------------------------------------------------------

/// GREEN probe (metric only for the melody). A loud bass note held under a quiet melody whose notes sit exactly on its partials (C2 under
/// C5 D5 E5 G5 Bb5 C6 = partials 8 9 10 12 14 16): measures melody recall per melody velocity; and
/// the bass alone must not grow ghosts on those partials (the envelope cap leaves the excess of an
/// uneven partial series in the residual). Measured melody recall: 1.00 at v80 on C2/G1/G2,
/// 0.50 / 1.00 / 0.38 at v45 (round 1: 0.50 / 0.50 / 0.50, with ghosts). No bass-alone ghosts.
#[test]
fn brk2_loud_bass_quiet_melody_on_its_partials() {
    let mel = [72u8, 74, 76, 79, 82, 84, 82, 79];
    let mut report = Vec::new();
    let mut ghosts = Vec::new();
    for bass in [36u8, 31, 43] {
        let b = vec![n(0.3, 3.4, bass, 120)];
        let alone = transcribe(&synth_piano(&b, 4.0, 3), RATE);
        let g: Vec<&Note> = alone.iter().filter(|e| e.pitch != bass).collect();
        if !g.is_empty() {
            ghosts.push(format!("bass {bass} alone: {}", names(&alone)));
        }
        for vel in [45u8, 60, 80] {
            let mut gt = b.clone();
            for (i, &p) in mel.iter().enumerate() {
                gt.push(n(0.6 + 0.4 * i as f64, 0.35, p + bass - 36, vel));
            }
            let est = transcribe(&synth_piano(&gt, 4.0, 3), RATE);
            let mg: Vec<Note> = gt[1..].to_vec();
            let me: Vec<Note> = est.iter().filter(|e| e.pitch > bass + 12).cloned().collect();
            let (p, r, _) = note_f1(&me, &mg, TOL);
            report.push(format!("bass {bass} v120 / melody v{vel}: R {r:.2} P {p:.2}"));
        }
    }
    eprintln!("METRIC r2 loud bass, quiet melody: {}", report.join("; "));
    assert!(ghosts.is_empty(), "{}", ghosts.join("\n"));
}

/// GREEN probe. Triads and sevenths in every inversion and open/close position, C3..C6: the plan target F1 0.85.
#[test]
fn brk2_inverted_and_open_voicings() {
    let mut gt = Vec::new();
    let mut t = 0.3;
    let shapes: [&[u8]; 8] = [&[0, 4, 7], &[4, 7, 12], &[7, 12, 16], &[0, 7, 16], &[0, 4, 7, 11], &[4, 7, 11, 12], &[7, 11, 12, 16], &[0, 10, 16, 19]];
    for root in [48u8, 53, 57, 60, 62, 67] {
        for s in shapes {
            for &iv in s {
                gt.push(n(t, 0.5, root + iv, 85));
            }
            t += 0.7;
        }
    }
    let est = transcribe(&synth_piano(&gt, t + 0.5, 4), RATE);
    let (p, r, f) = note_f1(&est, &gt, TOL);
    eprintln!("METRIC r2 inversions/open: P {p:.3} R {r:.3} F1 {f:.3} ({} notes)", gt.len());
    assert!(f >= 0.85, "F1 {f:.3} (P {p:.3} R {r:.3})");
}

// ---------------------------------------------------------------------------------------------
// Re-strike detector vs tremolo / vibrato
// ---------------------------------------------------------------------------------------------

fn modulated(midi: f64, m: impl Fn(f64) -> f64, a: impl Fn(f64) -> f64) -> Result<(), String> {
    let est = transcribe(&held_tone(4.0, m, a), RATE);
    let on = est.iter().filter(|e| e.pitch as f64 == midi).count();
    if on != 1 || est.len() > 2 {
        return Err(format!("midi {midi}: {} notes ({on} on the key): {}", est.len(), names(&est)));
    }
    Ok(())
}

/// RED (R2-F4). Deep tremolo on a held low tone (amplitude 0.2..1 at 7 Hz, 0.1..1 at 3 Hz: an amp
/// tremolo, a Leslie on slow): every swell is re-struck, from the start (D3: 28 notes at 7 Hz, 13 at
/// 3 Hz; round 1: 17 and 9, starting only after ~1.7 s). The 4-frame RESTRIKE_FAST path (x2.0 with
/// 3 of 4 partials lifted, out of a dip) needs no flux and is met by any swell deeper than 6 dB
/// within 46 ms. E4 / D5 pass. Round 1's gentler 0.5..1 at 5 Hz case stays green.
#[test]
fn brk2_deep_tremolo_stays_one_note() {
    use std::f64::consts::TAU;
    let mut bad = Vec::new();
    for midi in [50.0f64, 62.0, 74.0] {
        for (depth, hz) in [(0.4f64, 7.0f64), (0.45, 3.0)] {
            if let Err(e) = modulated(midi, move |_| midi, move |t| (1.0 - depth) + depth * (TAU * hz * t).sin()) {
                bad.push(format!("tremolo {:.1}..1 at {hz} Hz, {e}", 1.0 - 2.0 * depth));
            }
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// RED (R2-F4). Wide (operatic) vibrato, ±100 cents at 6 Hz: D3 gives 19 notes (the key once, then
/// 51 flickering and 61, the octave of the low extreme, re-struck every cycle), E4 gives 95 notes
/// (61/62/63 chopped at the vibrato rate), D5 3 notes none on the key. Round 1 gave one note on
/// each. Vibrato with its natural amplitude wobble (±40 c + 30 % AM) and round 1's ±30..50 c pass.
#[test]
fn brk2_wide_vibrato_stays_one_note() {
    use std::f64::consts::TAU;
    let mut bad = Vec::new();
    for midi in [50.0f64, 62.0, 74.0] {
        if let Err(e) = modulated(midi, move |t| midi + (TAU * 6.0 * t).sin(), |_| 1.0) {
            bad.push(format!("vibrato ±100c, {e}"));
        }
        if let Err(e) = modulated(midi, move |t| midi + 0.4 * (TAU * 5.5 * t).sin(), |t| 0.85 + 0.15 * (TAU * 5.5 * t + 1.0).sin()) {
            bad.push(format!("vibrato ±40c + AM, {e}"));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

// ---------------------------------------------------------------------------------------------
// FluidSynth (ignored): other instruments, reverb tails, pedal
// ---------------------------------------------------------------------------------------------

/// RED (R2-F2, FluidSynth). Monophonic lines on non-piano GM instruments. Precision, round 1 -> 2:
/// trumpet 0.909 -> 0.588 (ghosts +19 = h3: 79 for 60, 81 for 62...), trombone 0.353 -> 0.158 (38
/// notes for 10: +12 / +19 / +28), alto sax 0.692 -> 0.474, clarinet 0.357 -> 0.250 (+28 = h5),
/// piano 1.000 -> 0.909; basses 1.000, tuba / cello unchanged (late onsets). Root cause, checked
/// with PIANO_TUNE: disabling the envelope cap (ENV_K=100) brings trumpet back to 1.000 and trombone
/// to 0.333; the cap (1.5 ref / h, ref from h1/h2) leaves the excess of a series whose h3..h5 are
/// above h1 in the residual. UNCLAIMED_MIN=1 alone changes little.
#[test]
#[ignore]
fn brk2_fluidsynth_mono_lines_other_instruments() {
    let line = |lo: u8| -> Vec<Note> { [0u8, 2, 4, 5, 7, 9, 11, 12, 7, 4].iter().enumerate().map(|(i, &d)| n(0.4 + 0.6 * i as f64, 0.5, lo + d, 95)).collect() };
    let mut bad = Vec::new();
    for (prog, name, lo) in [(32u8, "acoustic bass", 33u8), (33, "finger bass", 33), (57, "trombone", 41), (58, "tuba", 33), (42, "cello", 41), (56, "trumpet", 60), (65, "alto sax", 56), (71, "clarinet", 55), (0, "piano", 36)] {
        let gt = line(lo);
        let est = transcribe(&fluid_opts(&format!("mono-{prog}"), &gt, prog, 22050, &[]), 22050);
        let (p, r, f) = note_f1(&est, &gt, TOL);
        let extra: Vec<String> = est.iter().filter(|e| !gt.iter().any(|g| g.pitch == e.pitch && (e.t - g.t).abs() <= TOL)).map(|e| format!("{}@{:.2}", e.pitch, e.t)).collect();
        eprintln!("METRIC r2 mono {name} (prog {prog}): P {p:.3} R {r:.3} F1 {f:.3}, {} est for 10; extra {extra:?}", est.len());
        if p < 0.7 {
            bad.push(format!("{name}: P {p:.3} ({} est for 10)", est.len()));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("; "));
}

/// RED (R2-F1, FluidSynth). Staccato notes in a big reverb (room 0.9, level 1.0): the tail must not
/// come back as new notes. Precision, round 1 -> 2: piano 1.000 -> 0.400, violin 0.833 -> 0.385,
/// strings 0.333 -> 0.143 (round 1 lost the strings to late onsets). The extra notes start 0.4..3 s
/// after the note ended, at velocity 16..58. Root cause, traced on piano C4 (f150..f185): the key is
/// Idle in the reverb tail, 37 dB under its strike; its gate energy creeps up x1.5..1.7 over 6
/// frames (comb-filter beating), which passes `recent_rise` (ONSET_RISE 1.5, no level or flux
/// condition, no limit on how slow the rise is), and the whitened salience of the clean tail peak
/// reaches 15 > ON_ABS. PIANO_TUNE ONSET_RISE=2.5 brings piano to 0.750 and violin to 0.833.
#[test]
#[ignore]
fn brk2_fluidsynth_reverb_tail_no_restrikes() {
    let gt: Vec<Note> = [60u8, 64, 67, 72, 48, 55].iter().enumerate().map(|(i, &p)| n(0.5 + 1.2 * i as f64, 0.15, p, 100)).collect();
    let mut bad = Vec::new();
    for (prog, name) in [(0u8, "piano"), (48, "strings"), (40, "violin")] {
        let est = transcribe(&fluid_opts(&format!("reverb-{prog}"), &gt, prog, 22050, &["synth.reverb.room-size=0.9", "synth.reverb.level=1.0", "synth.reverb.width=100"]), 22050);
        let (p, r, f) = note_f1(&est, &gt, TOL);
        eprintln!("METRIC r2 reverb {name}: P {p:.3} R {r:.3} F1 {f:.3}: {}", names(&est));
        if p < 0.8 {
            bad.push(format!("{name}: P {p:.3}: {}", names(&est)));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// RED (R2-F3, FluidSynth; also in round 1 at P 0.500). Sustain pedal (every note held 2.5 s, as a
/// pedalled arpeggio sounds): P 0.462 R 0.600. A new note on a partial of a held one (C3 over a held
/// C2, G3 over G2) re-strikes the held key and the new key is missed: 36 re-struck at 0.859 when 48
/// was played, 43 at 1.335 for 55. The harmonic-rivalry rule in `track` only compares keys that are
/// already sounding (`rs` is filled for KeyState::On only), so an Idle key starting on a held key's
/// partial is never the rival that wins.
#[test]
#[ignore]
fn brk2_fluidsynth_pedalled_arpeggio() {
    let arp = [36u8, 43, 48, 52, 55, 60, 64, 67, 72, 76];
    let mut gt = Vec::new();
    for rep in 0..2 {
        for (i, &p) in arp.iter().enumerate() {
            gt.push(n(0.4 + 2.6 * rep as f64 + 0.22 * i as f64, 2.5 - 0.22 * i as f64, p + 5 * rep as u8, 85));
        }
    }
    let est = transcribe(&fluid_opts("pedal-arp", &gt, 0, 22050, &[]), 22050);
    let (p, r, f) = note_f1(&est, &gt, TOL);
    eprintln!("METRIC r2 pedalled arpeggio: P {p:.3} R {r:.3} F1 {f:.3}, {} est for {}: {}", est.len(), gt.len(), names(&est));
    assert!(p >= 0.8, "P {p:.3}: {}", names(&est));
}

/// A held harmonic tone with a given partial spectrum (amplitudes for h = 1..), 15 ms attack.
fn spectral_tone(notes: &[(f64, f64, f64)], spec: &[f64], secs: f64) -> Vec<f32> {
    let r = RATE as f64;
    let mut out = vec![0f32; (secs * r) as usize];
    for &(t0, dur, midi) in notes {
        let f0 = 440.0 * 2f64.powf((midi - 69.0) / 12.0);
        let (i0, i1) = ((t0 * r) as usize, (((t0 + dur + 0.03) * r) as usize).min(out.len()));
        for (h, &a) in spec.iter().enumerate() {
            let fh = (h + 1) as f64 * f0;
            if fh > 0.45 * r {
                break;
            }
            for i in i0..i1 {
                let t = (i - i0) as f64 / r;
                let env = (t / 0.015).min(1.0) * if t > dur { (-(t - dur) / 0.01).exp() } else { 1.0 };
                out[i] += (0.05 * a * env * (std::f64::consts::TAU * fh * t).sin()) as f32;
            }
        }
    }
    out
}

/// GREEN probe (R2-F2 does not reproduce synthetically). A brass-like line (partials 2..5 stronger than the fundamental, as a
/// trumpet or trombone has): the envelope cap on the cancel (reference = max(a1, 1.4 a2), cap
/// 1.5 ref / h) leaves the excess of h3..h5 in the residual, and it comes out as twelfth / 17th ghosts.
#[test]
fn brk2_brass_like_spectrum_no_harmonic_ghosts() {
    let spec = [0.45, 0.8, 1.0, 0.95, 0.8, 0.6, 0.45, 0.33, 0.24, 0.17, 0.12, 0.08];
    let mut bad = Vec::new();
    for lo in [41.0f64, 52.0, 60.0] {
        let notes: Vec<(f64, f64, f64)> = [0.0, 2.0, 4.0, 5.0, 7.0, 9.0, 7.0, 4.0].iter().enumerate().map(|(i, &d)| (0.3 + 0.6 * i as f64, 0.5, lo + d)).collect();
        let gt: Vec<Note> = notes.iter().map(|&(t, d, m)| n(t, d, m as u8, 90)).collect();
        let est = transcribe(&spectral_tone(&notes, &spec, 5.5), RATE);
        let (p, r, _) = note_f1(&est, &gt, TOL);
        let ghosts: Vec<String> = est.iter().filter(|e| !gt.iter().any(|g| g.pitch == e.pitch && e.t >= g.t - TOL && e.t <= g.t + g.dur)).map(|e| format!("{}@{:.2}", e.pitch, e.t)).collect();
        eprintln!("METRIC r2 brass-like from {lo}: P {p:.3} R {r:.3}, ghosts {ghosts:?}");
        if !ghosts.is_empty() {
            bad.push(format!("line from {lo}: ghosts {ghosts:?}"));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// GREEN probe (R2-F1 does not reproduce with this synthetic tail; round 1 failed it). A short piano note followed by a reverb-like tail of the same pitch
/// 35 dB down whose level beats slowly (Freeverb's comb filters do this): the tail must not be
/// re-born as a new note. FluidSynth version: brk2_fluidsynth_reverb_tail_no_restrikes.
#[test]
fn brk2_reverb_like_tail_is_not_a_new_note() {
    let mut bad = Vec::new();
    for pitch in [48u8, 60, 67] {
        let mut x = synth_piano(&[n(0.3, 0.15, pitch, 100)], 3.5, 2);
        let m = pitch as f64;
        let tail = held_tone(3.0, |_| m, |t| (-(t / 1.5)).exp() * (0.6 + 0.4 * (std::f64::consts::TAU * 1.7 * t).sin()));
        for (i, v) in tail.iter().enumerate() {
            let j = i + (0.4 * RATE as f64) as usize;
            if j < x.len() {
                x[j] += 0.025 * v;
            }
        }
        let est = transcribe(&x, RATE);
        if est.len() != 1 {
            bad.push(format!("pitch {pitch}: {}", names(&est)));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}
