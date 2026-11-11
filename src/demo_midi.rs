//! Held-note demo: one pass over a WAV or a synthetic tone through the live window loop (carry-over, Save MIDI).
//! Run: `WAVELINK_DEMO_WAV=<path> WAVELINK_DEMO_LABEL=<name> WAVELINK_DEMO_OUT=<mid path> cargo test --release -p wavelink-mixer demo_held_notes -- --ignored --nocapture`
use crate::dsp::BP_RATE;
use crate::notes::{midi_bytes, Note, Timeline};
use crate::notes::take_for_save; // PR-ONLY
use crate::sep::{Transcriber, BP_SAMPLES, FPS, FRAME_THRESH, ONSET_THRESH};
use crate::stems::{in_window, window_notes, Shared, BP_WINDOW, CHUNK, RATE};
use crate::stems::{commit, release}; // PR-ONLY
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

/// Notes sorted by (t, pitch): the order the stats and the save comparison use.
fn by_time(mut v: Vec<Note>) -> Vec<Note> {
    v.sort_by(|a, b| a.t.total_cmp(&b.t).then(a.pitch.cmp(&b.pitch)));
    v
}

/// Index rule of the plan: round((n - 1) * q) into the sorted list; 0 for an empty list.
fn quantile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((sorted.len() - 1) as f64 * q).round() as usize]
}

fn vlq_at(b: &[u8], mut i: usize) -> (usize, usize) {
    let mut v = 0usize;
    loop {
        let c = b[i];
        i += 1;
        v = (v << 7) | (c & 0x7f) as usize;
        if c & 0x80 == 0 {
            return (v, i);
        }
    }
}

/// (abs_tick, status & 0xf0, key) of every note on/off in one MTrk chunk, in file order (0 = tempo map,
/// 1 = drums, 2 = bass, 3 = other). Reads only the files `midi_bytes` writes. Same parse as the notes.rs test helper.
fn key_events(bytes: &[u8], track: usize) -> Vec<(u64, u8, u8)> {
    let mut pos = 14usize;
    for idx in 0..=track {
        assert_eq!(&bytes[pos..pos + 4], b"MTrk");
        let len = u32::from_be_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = &bytes[pos + 8..pos + 8 + len];
        pos += 8 + len;
        if idx != track {
            continue;
        }
        let mut out = Vec::new();
        let (mut i, mut tick) = (0usize, 0u64);
        while i < body.len() {
            let (delta, next) = vlq_at(body, i);
            i = next;
            tick += delta as u64;
            let st = body[i];
            if st == 0xff {
                let (plen, next) = vlq_at(body, i + 2);
                i = next + plen;
            } else if st & 0xf0 == 0xc0 || st & 0xf0 == 0xd0 {
                i += 2;
            } else {
                if st & 0xf0 == 0x80 || st & 0xf0 == 0x90 {
                    out.push((tick, st & 0xf0, body[i + 1]));
                }
                i += 3;
            }
        }
        return out;
    }
    panic!("no track {track}");
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
    let label = std::env::var("WAVELINK_DEMO_LABEL").unwrap_or_else(|_| "unset".to_string());
    println!("source: {source}");
    println!("platform: {}", std::env::consts::OS);
    println!("label: {label}");

    // One pass of the live window loop, stem 1 only. `reference` gets the same notes through plain add/extend.
    let shared = Shared::new();
    let mut reference = Timeline::new();
    let mut open: Vec<Note> = Vec::new();
    let mut strikes: Vec<(f64, u8)> = Vec::new();
    let mut edges: Vec<f64> = Vec::new();
    let total_s = sig.len() as f64 / RATE as f64;
    let mut mid: [Vec<Note>; 3] = Default::default();
    let mut held_at_mid = 0usize;
    let mut mid_t = 0.0f64;
    let mut mid_done = false;
    let mut windows = 0usize;
    let mut played = CHUNK;
    while played <= sig.len() {
        let end_t = played as f64 / RATE as f64;
        let have = played.min(BP_WINDOW);
        let mut win = vec![0.0f32; BP_WINDOW - have];
        win.extend_from_slice(&sig[played - have..played]);
        let mut g = tr.grids(&win).expect("grids");
        let ws = end_t - BP_SAMPLES as f64 / BP_RATE as f64;
        // Raw strikes of this window's keep slice: onset peaks over the threshold with a note frame under them.
        if let Some(gr) = g.as_ref() {
            edges.push(ws + gr.note.len() as f64 / FPS);
            for f in 0..gr.note.len() {
                let t = ws + f as f64 / FPS;
                if !in_window(t, end_t) {
                    continue;
                }
                for p in 0..gr.onset[f].len() {
                    let v = gr.onset[f][p];
                    let before = if f > 0 { gr.onset[f - 1][p] } else { f32::NEG_INFINITY };
                    let after = if f + 1 < gr.onset.len() { gr.onset[f + 1][p] } else { f32::NEG_INFINITY };
                    if v >= ONSET_THRESH && gr.note[f][p] >= FRAME_THRESH && (v >= before || v >= after) {
                        strikes.push((t, p as u8 + 21));
                    }
                }
            }
        }
        let (new, ext) = window_notes(g.as_mut(), end_t, &mut open);
        commit(&shared, 1, &new, &ext, &open); // PR-ONLY
        // MAIN: { let mut tl = shared.timeline.lock().unwrap(); tl.add(1, &new); tl.extend(1, &ext); }
        reference.add(1, &new);
        reference.extend(1, &ext);
        windows += 1;
        if !mid_done && end_t >= total_s / 2.0 {
            let (mid_taken, held_now) = take_for_save(&mut shared.timeline.lock().unwrap(), true); // PR-ONLY
            // MAIN: let (mid_taken, held_now) = (shared.timeline.lock().unwrap().drain_all(), open.len());
            mid = mid_taken;
            held_at_mid = held_now;
            mid_t = end_t;
            mid_done = true;
        }
        played += CHUNK;
    }
    release(&shared); // PR-ONLY
    let (fin, _) = take_for_save(&mut shared.timeline.lock().unwrap(), true); // PR-ONLY
    // MAIN: let fin = shared.timeline.lock().unwrap().drain_all();
    let single = reference.drain_all();

    strikes.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.total_cmp(&b.0)));
    strikes.dedup_by(|a, b| a.1 == b.1 && (a.0 - b.0).abs() < 0.5 / FPS);

    let bass = by_time(single[1].clone());
    let mut durs: Vec<f64> = bass.iter().map(|n| n.dur).collect();
    durs.sort_by(f64::total_cmp);
    println!("windows: {windows}");
    println!("notes per track: drums {}, bass {}, other {}", single[0].len(), single[1].len(), single[2].len());
    println!("note length p10/p50/p90 (s): {:.3} / {:.3} / {:.3}", quantile(&durs, 0.1), quantile(&durs, 0.5), quantile(&durs, 0.9));
    println!("longest note (s): {:.3}", durs.last().copied().unwrap_or(0.0));

    let mut by_key = bass.clone();
    by_key.sort_by(|a, b| a.pitch.cmp(&b.pitch).then(a.t.total_cmp(&b.t)));
    let overlaps = by_key.windows(2).filter(|w| w[0].pitch == w[1].pitch && w[1].t < w[0].t + w[0].dur - 1e-9).count();
    println!("same-key overlaps in timeline: {overlaps}");

    let mut on = [false; 128];
    let mut stacked = 0usize;
    for (_, st, k) in key_events(&midi_bytes(&single), 2) {
        let k = (k & 0x7f) as usize;
        if st == 0x90 {
            if on[k] {
                stacked += 1;
            }
            on[k] = true;
        } else {
            if !on[k] {
                stacked += 1;
            }
            on[k] = false;
        }
    }
    println!("same-key stacked events in file: {stacked}");

    let hits = |n: &Note, e: f64| (n.t + n.dur - e).abs() <= 1.5 / FPS;
    let at_edge = bass.iter().filter(|n| edges.iter().any(|&e| hits(n, e))).count();
    let at_last = edges.last().map_or(0, |&e| bass.iter().filter(|n| hits(n, e)).count());
    println!(
        "notes ending at a window edge: {at_edge} of {} (chance about {:.0}), {at_last} at the last window",
        bass.len(),
        bass.len() as f64 * 3.0 / 43.0
    );

    let reach = 13.0 / FPS;
    let mut swallowed: Vec<String> = Vec::new();
    let mut swallowed_n = 0usize;
    for &(t, p) in &strikes {
        let inside = bass.iter().find(|n| n.pitch == p && t - n.t >= reach && n.t + n.dur - t >= reach);
        let starts = bass.iter().any(|n| n.pitch == p && (n.t - t).abs() <= 2.0 / FPS);
        if let (Some(n), false) = (inside, starts) {
            swallowed_n += 1;
            if swallowed.len() < 10 {
                swallowed.push(format!("  pitch {p} strike {t:.3} inside note {:.3}..{:.3}", n.t, n.t + n.dur));
            }
        }
    }
    println!("re-strikes swallowed: {swallowed_n} of {} strikes", strikes.len());
    for s in &swallowed {
        println!("{s}");
    }

    let mut joined = mid[1].clone();
    joined.extend(fin[1].iter().cloned());
    let joined = by_time(joined);
    println!("mid-song save at {mid_t:.1} s: {} finished, {held_at_mid} held", mid[1].len());
    println!("mid + final == single final save: {}", if joined == bass { "YES" } else { "NO" });
    let mut ndiff = 0usize;
    let mut diffs: Vec<String> = Vec::new();
    for i in 0..joined.len().max(bass.len()) {
        if joined.get(i) != bass.get(i) {
            ndiff += 1;
            if diffs.len() < 10 {
                diffs.push(format!("  #{i}: mid+final {:?} vs single {:?}", joined.get(i), bass.get(i)));
            }
        }
    }
    println!("notes that differ: {ndiff}");
    for d in &diffs {
        println!("{d}");
    }

    if let Ok(out) = std::env::var("WAVELINK_DEMO_OUT") {
        std::fs::write(&out, midi_bytes(&single)).unwrap_or_else(|e| panic!("write {out}: {e}"));
        println!("wrote: {out}");
    }
}
