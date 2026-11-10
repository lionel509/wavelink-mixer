//! Held-note demo: before (main's per-window keep) vs after (carry-over), on a WAV or a synthetic tone.
//! Run: `WAVELINK_DEMO_WAV=<path> cargo test --release -p wavelink-mixer demo_held_notes -- --ignored --nocapture`
use crate::dsp::BP_RATE;
use crate::notes::{Note, Timeline};
use crate::sep::{Transcriber, BP_SAMPLES};
use crate::stems::{in_window, window_notes, BP_WINDOW, CHUNK, RATE};
use std::f64::consts::PI;

/// Mono samples of a 16-bit PCM WAV at 44100 Hz (channels averaged).
fn read_wav(path: &str) -> Vec<f32> {
    let b = std::fs::read(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    assert!(b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WAVE", "{path}: not a RIFF/WAVE file");
    let mut fmt: Option<(u16, usize, u32, u16)> = None;
    let mut data: Option<&[u8]> = None;
    let mut i = 12;
    while i + 8 <= b.len() {
        let id = &b[i..i + 4];
        let len = u32::from_le_bytes([b[i + 4], b[i + 5], b[i + 6], b[i + 7]]) as usize;
        let body = &b[i + 8..(i + 8 + len).min(b.len())];
        if id == b"fmt " && body.len() >= 16 {
            let tag = u16::from_le_bytes([body[0], body[1]]);
            let ch = u16::from_le_bytes([body[2], body[3]]) as usize;
            let rate = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
            let bits = u16::from_le_bytes([body[14], body[15]]);
            fmt = Some((tag, ch, rate, bits));
        }
        if id == b"data" {
            data = Some(body);
        }
        i += 8 + len + (len & 1);
    }
    let (tag, ch, rate, bits) = fmt.expect("fmt chunk");
    assert!((tag == 1 || tag == 0xFFFE) && bits == 16 && (ch == 1 || ch == 2), "{path}: need 16-bit PCM mono or stereo, got tag {tag}, {ch} ch, {bits} bit");
    assert_eq!(rate, RATE as u32, "{path}: need 44100 Hz, got {rate}");
    data.expect("data chunk")
        .chunks_exact(2 * ch)
        .map(|f| {
            let s: f32 = (0..ch).map(|c| i16::from_le_bytes([f[2 * c], f[2 * c + 1]]) as f32 / 32768.0).sum();
            s / ch as f32
        })
        .collect()
}

/// 0.5 s silence, a 4.0 s 220 Hz tone with a 10 ms attack, 2.0 s silence.
fn synth_tone() -> Vec<f32> {
    let r = RATE as f64;
    let total = (6.5 * r) as usize;
    let on = (0.5 * r) as usize;
    let off = (4.5 * r) as usize;
    let att = (0.010 * r) as usize;
    (0..total)
        .map(|i| {
            if i < on || i >= off {
                return 0.0;
            }
            let t = i as f64 / r;
            let env = if i - on < att { (i - on) as f64 / att as f64 } else { 1.0 };
            let f = 220.0;
            let s = 0.4 * (2.0 * PI * f * t).sin() + 0.2 * (4.0 * PI * f * t).sin() + 0.1 * (6.0 * PI * f * t).sin();
            (env * s) as f32
        })
        .collect()
}

/// Feeds the signal one CHUNK at a time; `carry` picks the window_notes path, otherwise main's keep is replayed.
fn run(tr: &mut Transcriber, sig: &[f32], carry: bool) -> Vec<Note> {
    let mut tl = Timeline::new();
    let mut kept_before: Vec<Note> = Vec::new();
    let mut open: Vec<Note> = Vec::new();
    let mut played = CHUNK;
    while played <= sig.len() {
        let end_t = played as f64 / RATE as f64;
        let have = played.min(BP_WINDOW);
        let mut win = vec![0.0f32; BP_WINDOW - have];
        win.extend_from_slice(&sig[played - have..played]);
        if carry {
            let mut g = tr.grids(&win).expect("grids");
            let (new, ext) = window_notes(g.as_mut(), end_t, &mut open);
            tl.add(1, &new);
            tl.extend(1, &ext);
        } else {
            for n in tr.notes(&win).expect("notes") {
                let t = end_t - BP_SAMPLES as f64 / BP_RATE as f64 + n.t;
                if in_window(t, end_t) {
                    kept_before.push(Note { t, ..n });
                }
            }
        }
        played += CHUNK;
    }
    if carry {
        tl.window(1, 0.0, 1e9)
    } else {
        kept_before.sort_by(|a, b| a.t.total_cmp(&b.t));
        kept_before
    }
}

fn report(label: &str, notes: &[Note]) {
    println!("{label}");
    println!("notes: {}", notes.len());
    for n in notes {
        println!("  pitch {} t {:.3} dur {:.3} vel {}", n.pitch, n.t, n.dur, n.vel);
    }
    let longest = notes.iter().map(|n| n.dur).fold(0.0, f64::max);
    println!("longest: {:.3} s", longest);
    println!("notes >= 1.0 s: {}", notes.iter().filter(|n| n.dur >= 1.0).count());
}

#[test]
#[ignore]
fn demo_held_notes() {
    let m = crate::models::ensure(&["basic-pitch-nmp.onnx"], &|s: String| println!("{s}")).expect("model");
    let mut tr = Transcriber::load(&m).expect("transcriber");
    let (sig, source) = match std::env::var("WAVELINK_DEMO_WAV") {
        Ok(p) => (read_wav(&p), p),
        Err(_) => (synth_tone(), "synthetic 220 Hz 4.0 s".to_string()),
    };
    println!("source: {source}");
    println!("platform: {}", std::env::consts::OS);
    let before = run(&mut tr, &sig, false);
    report("before (main's per-window keep, replayed)", &before);
    let after = run(&mut tr, &sig, true);
    report("after (carry-over)", &after);
}
