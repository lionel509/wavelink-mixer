//! Note data for the Stems feature: the rolling timeline, the SMF writer, and the private fluidsynth player.
#![allow(dead_code, unused_imports, unused_variables)]
use crate::config::glib_home;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Note {
    pub t: f64,
    pub dur: f64,
    pub pitch: u8,
    pub vel: u8,
}

pub const STEMS: [&str; 3] = ["drums", "bass", "other"];
pub const COLORS: [&str; 3] = ["#ff9f43", "#54a0ff", "#c8d6e5"];
pub const MIDI_LAG: f64 = 1.5;
/// (fluidsynth channel, GM program) for drums, bass and other.
pub const PROGRAMS: [(u8, u8); 3] = [(9, 0), (1, 33), (2, 0)];

/// Ticks per quarter note in the SMF output.
const TPQ: u32 = 480;
/// 120 bpm (500000 us per quarter note) at 480 tpq is 960 ticks per second.
const TICKS_PER_SEC: f64 = 960.0;
/// Drum hits are written with a fixed length, as in `synth_events`.
const DRUM_DUR: f64 = 0.1;

/// Notes per stem, in insertion order.
pub struct Timeline {
    notes: [Vec<Note>; 3],
}

impl Timeline {
    pub fn new() -> Self {
        Timeline { notes: [Vec::new(), Vec::new(), Vec::new()] }
    }

    pub fn add(&mut self, stem: usize, notes: &[Note]) {
        self.notes[stem].extend_from_slice(notes);
    }

    /// Notes with t < to and t + dur >= from.
    pub fn window(&self, stem: usize, from: f64, to: f64) -> Vec<Note> {
        self.notes[stem].iter().filter(|n| n.t < to && n.t + n.dur >= from).cloned().collect()
    }

    /// Drops notes with t + dur < before.
    pub fn prune(&mut self, before: f64) {
        for v in self.notes.iter_mut() {
            v.retain(|n| n.t + n.dur >= before);
        }
    }

    pub fn drain_all(&mut self) -> [Vec<Note>; 3] {
        [std::mem::take(&mut self.notes[0]), std::mem::take(&mut self.notes[1]), std::mem::take(&mut self.notes[2])]
    }
}

/// MIDI key for a note: drum pitch 0 is the GM bass drum (38), everything else is the pitch itself.
fn key(stem: usize, pitch: u8) -> u8 {
    if stem == 0 && pitch == 0 {
        38
    } else {
        pitch.min(127)
    }
}

fn write_vlq(mut v: u32, out: &mut Vec<u8>) {
    let mut buf = [0u8; 5];
    let mut i = 4;
    buf[i] = (v & 0x7f) as u8;
    v >>= 7;
    while v > 0 {
        i -= 1;
        buf[i] = ((v & 0x7f) as u8) | 0x80;
        v >>= 7;
    }
    out.extend_from_slice(&buf[i..]);
}

/// Serialises one track. `events` are (tick, is_note_on, bytes); at equal ticks note-offs come first.
fn track_chunk(events: &mut Vec<(u64, u8, Vec<u8>)>) -> Vec<u8> {
    events.sort_by_key(|e| (e.0, e.1));
    let mut body = Vec::new();
    let mut prev = 0u64;
    for (tick, _, bytes) in events.iter() {
        write_vlq((*tick - prev) as u32, &mut body);
        body.extend_from_slice(bytes);
        prev = *tick;
    }
    body.extend_from_slice(&[0x00, 0xff, 0x2f, 0x00]);
    let mut out = Vec::with_capacity(body.len() + 8);
    out.extend_from_slice(b"MTrk");
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    out
}

/// One SMF track: MIDI channel (0-based), optional program change and track name.
#[derive(Clone, Debug, PartialEq)]
pub struct TrackSpec {
    pub ch: u8,
    pub prog: Option<u8>,
    pub name: Option<String>,
}

/// SMF 1, 480 tpq, 120 bpm. Track 0 is the tempo map, then one track per entry. Times are measured
/// from `origin`, or from the earliest note when `origin` is None. Notes are written as given
/// (key = pitch, length = dur, at least one tick).
pub fn smf_bytes(tracks: &[(TrackSpec, Vec<Note>)], origin: Option<f64>) -> Vec<u8> {
    let t_min = tracks.iter().flat_map(|t| t.1.iter()).map(|n| n.t).fold(f64::INFINITY, f64::min);
    let t0 = origin.unwrap_or(if t_min.is_finite() { t_min } else { 0.0 });
    let tick = |t: f64| ((t - t0) * TICKS_PER_SEC).round().max(0.0) as u64;

    let mut out = Vec::new();
    out.extend_from_slice(b"MThd");
    out.extend_from_slice(&6u32.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes()); // format 1
    out.extend_from_slice(&(tracks.len() as u16 + 1).to_be_bytes()); // tempo + tracks
    out.extend_from_slice(&(TPQ as u16).to_be_bytes());

    let mut tempo = vec![(0u64, 0u8, vec![0xff, 0x51, 0x03, 0x07, 0xa1, 0x20])];
    out.extend(track_chunk(&mut tempo));

    for (spec, notes) in tracks {
        let ch = spec.ch & 0x0f;
        let mut ev = Vec::new();
        if let Some(name) = &spec.name {
            let mut b = vec![0xff, 0x03];
            write_vlq(name.len() as u32, &mut b);
            b.extend_from_slice(name.as_bytes());
            ev.push((0u64, 0u8, b));
        }
        if let Some(prog) = spec.prog {
            ev.push((0u64, 0u8, vec![0xc0 | ch, prog & 0x7f]));
        }
        // a note of a key ends no later than the next start of that key on this track, so overlapping
        // same-key notes never let the first Off cut the second note short
        let mut order: Vec<&Note> = notes.iter().collect();
        order.sort_by(|a, b| a.t.total_cmp(&b.t));
        for (i, n) in order.iter().enumerate() {
            let k = n.pitch.min(127);
            let vel = n.vel.clamp(1, 127);
            let on = tick(n.t);
            let mut off = tick(n.t + n.dur).max(on + 1);
            if let Some(next) = order[i + 1..].iter().find(|m| m.pitch.min(127) == k) {
                off = off.min(tick(next.t).max(on + 1));
            }
            ev.push((on, 1, vec![0x90 | ch, k, vel]));
            ev.push((off, 0, vec![0x80 | ch, k, 0]));
        }
        out.extend(track_chunk(&mut ev));
    }
    out
}

/// SMF 1, 480 tpq. Track 0 is the tempo map; then drums (ch 9, pitch 0 -> key 38), bass (ch 1, prog 33), other (ch 2, prog 0).
pub fn midi_bytes(tracks: &[Vec<Note>; 3]) -> Vec<u8> {
    let specs: Vec<(TrackSpec, Vec<Note>)> = tracks
        .iter()
        .enumerate()
        .map(|(stem, notes)| {
            let (ch, prog) = PROGRAMS[stem];
            let notes = notes
                .iter()
                .map(|n| Note { t: n.t, dur: if stem == 0 { DRUM_DUR } else { n.dur }, pitch: key(stem, n.pitch), vel: n.vel })
                .collect();
            (TrackSpec { ch, prog: if stem == 0 { None } else { Some(prog) }, name: None }, notes)
        })
        .collect();
    smf_bytes(&specs, None)
}

/// Piano export: Acoustic Grand (program 0). `split` writes one track per lane (Bass / Mid / High on
/// MIDI channels 1 / 2 / 3), otherwise one merged track on channel 1. `origin` as in `smf_bytes`.
pub fn piano_midi(notes: &[Note], split: bool, origin: Option<f64>) -> Vec<u8> {
    if split {
        let lanes = crate::piano::split_lanes(notes);
        let tracks: Vec<(TrackSpec, Vec<Note>)> = crate::piano::LANES
            .iter()
            .zip(lanes)
            .map(|(l, v)| (TrackSpec { ch: l.index() as u8, prog: Some(0), name: Some(format!("Piano {}", l.name())) }, v))
            .collect();
        smf_bytes(&tracks, origin)
    } else {
        smf_bytes(&[(TrackSpec { ch: 0, prog: Some(0), name: Some("Piano".into()) }, notes.to_vec())], origin)
    }
}

/// A parsed note: track index (0 = the first chunk), channel, program in effect, and the note.
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedNote {
    pub track: usize,
    pub ch: u8,
    pub prog: u8,
    pub note: Note,
}

fn read_vlq(b: &[u8], i: &mut usize) -> Result<u32, String> {
    let mut v = 0u32;
    for _ in 0..4 {
        let c = *b.get(*i).ok_or("truncated varlen")?;
        *i += 1;
        v = (v << 7) | (c & 0x7f) as u32;
        if c & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err("varlen too long".into())
}

/// A small SMF (format 0/1, PPQ timing) reader: notes in seconds, honouring tempo changes from any
/// track, running status, and note-on with velocity 0 as note-off. Enough to round-trip our writer
/// and to score a reference file.
pub fn parse_smf(b: &[u8]) -> Result<Vec<ParsedNote>, String> {
    if b.len() < 14 || &b[..4] != b"MThd" {
        return Err("not a MIDI file".into());
    }
    let hlen = u32::from_be_bytes(b[4..8].try_into().unwrap()) as usize;
    let ntrk = u16::from_be_bytes([b[10], b[11]]) as usize;
    let div = u16::from_be_bytes([b[12], b[13]]);
    if div & 0x8000 != 0 || div == 0 {
        return Err("SMPTE timing not supported".into());
    }
    let tpq = div as f64;
    // pass 1: collect raw events (abs tick, track, bytes)
    let mut pos = 8 + hlen;
    let mut tempos: Vec<(u64, u32)> = Vec::new();
    let mut evs: Vec<(u64, usize, u8, u8, u8)> = Vec::new(); // tick, track, status, d1, d2
    for trk in 0..ntrk {
        if pos + 8 > b.len() || &b[pos..pos + 4] != b"MTrk" {
            return Err(format!("track {trk}: bad chunk"));
        }
        let len = u32::from_be_bytes(b[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = b.get(pos + 8..pos + 8 + len).ok_or("truncated track")?;
        pos += 8 + len;
        let (mut i, mut tick, mut status) = (0usize, 0u64, 0u8);
        while i < body.len() {
            tick += read_vlq(body, &mut i)? as u64;
            let first = *body.get(i).ok_or("truncated event")?;
            if first == 0xff {
                let ty = *body.get(i + 1).ok_or("truncated meta")?;
                i += 2;
                let l = read_vlq(body, &mut i)? as usize;
                let d = body.get(i..i + l).ok_or("truncated meta data")?;
                if ty == 0x51 && l == 3 {
                    tempos.push((tick, u32::from_be_bytes([0, d[0], d[1], d[2]])));
                }
                i += l;
                if ty == 0x2f {
                    break;
                }
                continue;
            }
            if first == 0xf0 || first == 0xf7 {
                i += 1;
                let l = read_vlq(body, &mut i)? as usize;
                i += l;
                continue;
            }
            if first & 0x80 != 0 {
                status = first;
                i += 1;
            } else if status == 0 {
                return Err("running status without a status".into());
            }
            let n = if matches!(status & 0xf0, 0xc0 | 0xd0) { 1 } else { 2 };
            let d = body.get(i..i + n).ok_or("truncated channel event")?;
            evs.push((tick, trk, status, d[0], if n == 2 { d[1] } else { 0 }));
            i += n;
        }
    }
    tempos.sort_by_key(|t| t.0);
    let secs = |tick: u64| -> f64 {
        let (mut t, mut last, mut us) = (0f64, 0u64, 500_000u32);
        for &(tt, v) in &tempos {
            if tt >= tick {
                break;
            }
            t += (tt - last) as f64 * us as f64 / 1e6 / tpq;
            last = tt;
            us = v;
        }
        t + (tick - last) as f64 * us as f64 / 1e6 / tpq
    };
    evs.sort_by_key(|e| (e.0, e.1));
    let mut prog = [0u8; 16];
    let mut open: std::collections::HashMap<(usize, u8, u8), (f64, u8, u8)> = Default::default();
    let mut out = Vec::new();
    for (tick, trk, st, d1, d2) in evs {
        let ch = st & 0x0f;
        match st & 0xf0 {
            0xc0 => prog[ch as usize] = d1,
            0x90 if d2 > 0 => {
                let t = secs(tick);
                if let Some((t0, v, p)) = open.insert((trk, ch, d1), (t, d2, prog[ch as usize])) {
                    out.push(ParsedNote { track: trk, ch, prog: p, note: Note { t: t0, dur: t - t0, pitch: d1, vel: v } });
                }
            }
            0x80 | 0x90 => {
                if let Some((t0, v, p)) = open.remove(&(trk, ch, d1)) {
                    out.push(ParsedNote { track: trk, ch, prog: p, note: Note { t: t0, dur: secs(tick) - t0, pitch: d1, vel: v } });
                }
            }
            _ => {}
        }
    }
    out.sort_by(|a, b| a.note.t.total_cmp(&b.note.t).then(a.note.pitch.cmp(&b.note.pitch)));
    Ok(out)
}

/// Per note: (t + lag, "noteon C K V") and (t + lag + d, "noteoff C K"). Drums: K = 38 if pitch == 0, d = 0.1.
pub fn synth_events(stem: usize, notes: &[Note], lag: f64) -> Vec<(f64, String)> {
    let (c, _) = PROGRAMS[stem];
    let mut out = Vec::with_capacity(notes.len() * 2);
    for n in notes {
        let k = key(stem, n.pitch);
        let d = if stem == 0 { DRUM_DUR } else { n.dur };
        let vel = n.vel.clamp(1, 127);
        out.push((n.t + lag, format!("noteon {c} {k} {vel}")));
        out.push((n.t + lag + d, format!("noteoff {c} {k}")));
    }
    out
}

/// Removes and returns the lines with time <= now, in ascending time order (stable).
pub fn due(pending: &mut Vec<(f64, String)>, now: f64) -> Vec<String> {
    let mut ready = Vec::new();
    let mut keep = Vec::new();
    for e in pending.drain(..) {
        if e.0 <= now {
            ready.push(e);
        } else {
            keep.push(e);
        }
    }
    *pending = keep;
    ready.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    ready.into_iter().map(|e| e.1).collect()
}

pub fn synth_bin() -> &'static str {
    for p in ["/home/linuxbrew/.linuxbrew/bin/fluidsynth", "/usr/bin/fluidsynth"] {
        if Path::new(p).exists() {
            return p;
        }
    }
    "fluidsynth"
}

/// glib_home()/.local/share/wavelink/soundfonts/FluidR3_GM.sf2, if it exists.
pub fn synth_font() -> Option<PathBuf> {
    let p = glib_home().join(".local/share/wavelink/soundfonts/FluidR3_GM.sf2");
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

/// ["-a", "pipewire", "-n", "-r", "48000", "-g", "0.6"]; the font is appended by spawn.
pub fn synth_args() -> Vec<String> {
    ["-a", "pipewire", "-n", "-r", "48000", "-g", "0.6"].iter().map(|s| s.to_string()).collect()
}

/// PipeWire props routing the synth into `node`.
pub fn synth_props(node: &str) -> String {
    format!(
        "{{ target.object={node} node.name=stems_synth node.description=\"Stems MIDI\" application.name=Stems node.dont-fallback=true }}"
    )
}

/// The private fluidsynth child. Its stdin carries the shell lines written by `pump`.
pub struct Synth {
    child: Child,
    stdin: ChildStdin,
    pending: Vec<(f64, String)>,
}

impl Synth {
    /// PIPEWIRE_PROPS from synth_props; stdin piped; then "prog 1 33\nprog 2 0\n". No font -> Err("soundfont missing").
    pub fn spawn(out_node: &str) -> Result<Synth, String> {
        let font = synth_font().ok_or_else(|| "soundfont missing".to_string())?;
        let mut cmd = Command::new(synth_bin());
        cmd.args(synth_args())
            .arg(&font)
            .env("PIPEWIRE_PROPS", synth_props(out_node))
            .stdin(Stdio::piped())
            .stdout(Stdio::null());
        crate::pw::die_with_parent(&mut cmd);
        let mut child = cmd.spawn().map_err(|e| format!("fluidsynth: {e}"))?;
        let mut stdin = child.stdin.take().ok_or_else(|| "fluidsynth: no stdin".to_string())?;
        let wrote = stdin.write_all(b"prog 1 33\nprog 2 0\n");
        let wrote = wrote.and_then(|_| stdin.flush());
        if let Err(e) = wrote {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("fluidsynth: {e}"));
        }
        Ok(Synth { child, stdin, pending: Vec::new() })
    }

    pub fn queue(&mut self, ev: Vec<(f64, String)>) {
        self.pending.extend(ev);
    }

    /// Writes the due lines and flushes; returns the count.
    pub fn pump(&mut self, now: f64) -> usize {
        let lines = due(&mut self.pending, now);
        let mut n = 0;
        for l in &lines {
            if writeln!(self.stdin, "{l}").is_err() {
                break;
            }
            n += 1;
        }
        let _ = self.stdin.flush();
        n
    }
}

impl Drop for Synth {
    /// Writes "reset\n", kills and waits.
    fn drop(&mut self) {
        let _ = writeln!(self.stdin, "reset");
        let _ = self.stdin.flush();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(t: f64, dur: f64, pitch: u8, vel: u8) -> Note {
        Note { t, dur, pitch, vel }
    }

    fn vlq(b: &[u8], mut i: usize) -> (usize, usize) {
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

    #[test]
    fn timeline_window_prune_drain() {
        let mut tl = Timeline::new();
        tl.add(1, &[n(1.0, 0.1, 40, 100), n(2.0, 0.1, 40, 100), n(3.0, 0.1, 40, 100)]);
        assert_eq!(tl.window(1, 1.5, 3.5).len(), 2);
        tl.prune(2.5);
        assert_eq!(tl.window(1, 0.0, 10.0).len(), 1);
        let d = tl.drain_all();
        assert!(d[0].is_empty());
        assert_eq!(d[1].len(), 1);
        assert_eq!(d[1][0].t, 3.0);
        assert!(d[2].is_empty());
        assert!(tl.drain_all().iter().all(|v| v.is_empty()));
    }

    #[test]
    fn midi_bytes_structure() {
        let tracks = [vec![n(0.0, 0.5, 0, 100), n(0.5, 0.5, 0, 100)], vec![n(0.0, 1.0, 40, 90)], vec![]];
        let b = midi_bytes(&tracks);
        assert_eq!(&b[..14], &[0x4d, 0x54, 0x68, 0x64, 0, 0, 0, 6, 0, 1, 0, 4, 0x01, 0xe0][..]);
        let (mut pos, mut chunks, mut total) = (14usize, 0usize, 0usize);
        let (mut ons, mut offs, mut drum_on) = (0usize, 0usize, false);
        while pos < b.len() {
            assert_eq!(&b[pos..pos + 4], b"MTrk");
            let len = u32::from_be_bytes(b[pos + 4..pos + 8].try_into().unwrap()) as usize;
            chunks += 1;
            total += len;
            let body = &b[pos + 8..pos + 8 + len];
            let (mut q, mut status) = (0usize, 0u8);
            while q < body.len() {
                q = vlq(body, q).1; // delta time
                let first = body[q];
                if first == 0xff {
                    let (ln, next) = vlq(body, q + 2);
                    q = next + ln;
                    continue;
                }
                let st = if first & 0x80 != 0 {
                    status = first;
                    q += 1;
                    first
                } else {
                    status // running status
                };
                match st & 0xf0 {
                    0x90 => {
                        let (key, vel) = (body[q], body[q + 1]);
                        q += 2;
                        if vel > 0 {
                            ons += 1;
                            if st == 0x99 && key == 38 {
                                drum_on = true;
                            }
                        } else {
                            offs += 1;
                        }
                    }
                    0x80 => {
                        q += 2;
                        offs += 1;
                    }
                    0xc0 | 0xd0 => q += 1,
                    _ => q += 2,
                }
            }
            pos += 8 + len;
        }
        assert_eq!(chunks, 4);
        assert_eq!(total, b.len() - 14 - 4 * 8);
        assert_eq!((ons, offs), (3, 3));
        assert!(drum_on, "drum pitch 0 must be written as key 38 on channel 9");
    }

    #[test]
    fn synth_events_channels_and_offs() {
        let ev = synth_events(0, &[n(1.0, 0.5, 0, 100)], 1.5);
        assert_eq!(ev.len(), 2, "{ev:?}");
        assert!((ev[0].0 - 2.5).abs() < 1e-9);
        assert_eq!(ev[0].1, "noteon 9 38 100");
        assert!((ev[1].0 - 2.6).abs() < 1e-9);
        assert_eq!(ev[1].1, "noteoff 9 38");

        let ev = synth_events(1, &[n(1.0, 0.5, 40, 100)], 1.5);
        assert_eq!(ev.len(), 2, "{ev:?}");
        assert!((ev[0].0 - 2.5).abs() < 1e-9);
        assert_eq!(ev[0].1, "noteon 1 40 100");
        assert!((ev[1].0 - 3.0).abs() < 1e-9);
        assert_eq!(ev[1].1, "noteoff 1 40");

        let ev = synth_events(2, &[n(1.0, 0.5, 40, 100)], 1.5);
        assert_eq!(ev[0].1, "noteon 2 40 100");
    }

    #[test]
    fn due_pops_in_order() {
        let mut p = vec![(3.0, "c".to_string()), (1.0, "a".to_string()), (2.0, "b".to_string())];
        assert_eq!(due(&mut p, 2.0), vec!["a".to_string(), "b".to_string()]);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].1, "c");
    }

    #[test]
    fn synth_never_listens_to_keyboard() {
        let a = synth_args();
        for want in ["-n", "-a", "pipewire"] {
            assert!(a.iter().any(|x| x == want), "synth_args lacks {want}: {a:?}");
        }
        assert!(!a.iter().any(|x| x.contains("autoconnect") || x.contains("alsa_seq")), "{a:?}");
        let p = synth_props("ch_stems");
        assert!(p.contains("target.object=ch_stems"), "{p}");
        assert!(p.contains("node.name=stems_synth"), "{p}");
    }

    #[test]
    fn smf_roundtrips_through_parser() {
        let notes = vec![n(0.25, 0.5, 40, 90), n(0.25, 1.0, 60, 100), n(1.0, 0.25, 76, 64), n(1.5, 0.125, 60, 30)];
        let b = piano_midi(&notes, true, Some(0.0));
        let p = parse_smf(&b).unwrap();
        assert_eq!(p.len(), notes.len());
        for (got, want) in p.iter().zip(&notes) {
            assert!((got.note.t - want.t).abs() < 1.5 / 960.0, "{got:?} vs {want:?}");
            assert!((got.note.dur - want.dur).abs() < 1.5 / 960.0, "{got:?} vs {want:?}");
            assert_eq!((got.note.pitch, got.note.vel, got.prog), (want.pitch, want.vel, 0));
            let lane = crate::piano::lane(want.pitch).index();
            assert_eq!((got.track, got.ch as usize), (lane + 1, lane), "lane track/channel for {want:?}");
        }
        let merged = parse_smf(&piano_midi(&notes, false, Some(0.0))).unwrap();
        assert!(merged.iter().all(|p| p.track == 1 && p.ch == 0));
        assert_eq!(merged.len(), 4);
        // origin None measures from the first note, as the stems export does
        let shifted = parse_smf(&piano_midi(&notes, false, None)).unwrap();
        assert!((shifted[0].note.t).abs() < 1e-9);
        // the stems writer keeps its layout: 4 chunks, drums on channel 9 as key 38
        let st = parse_smf(&midi_bytes(&[vec![n(0.0, 0.5, 0, 100)], vec![n(0.0, 1.0, 40, 90)], vec![]])).unwrap();
        assert_eq!(st.len(), 2);
        assert!(st.iter().any(|p| p.ch == 9 && p.note.pitch == 38 && (p.note.dur - 0.1).abs() < 0.002));
        assert!(st.iter().any(|p| p.ch == 1 && p.prog == 33 && p.note.pitch == 40));
    }

    #[test]
    fn parse_smf_rejects_garbage() {
        assert!(parse_smf(b"RIFF....").is_err());
        assert!(parse_smf(&[]).is_err());
        let mut b = piano_midi(&[n(0.0, 0.5, 60, 90)], false, Some(0.0));
        b.truncate(b.len() - 3);
        assert!(parse_smf(&b).is_err());
    }

    /// A zero-length note still writes its note-on before its note-off, and back-to-back notes of the
    /// same pitch keep off-before-on at the shared tick.
    #[test]
    fn midi_off_follows_on_and_back_to_back_same_pitch_holds() {
        let zero = vec![n(0.0, 0.0, 50, 100)];
        let bytes = midi_bytes(&[vec![], zero, vec![]]);
        let on = bytes.windows(3).position(|w| (w[0] & 0xf0) == 0x90 && w[1] == 50 && w[2] > 0).unwrap();
        let off = bytes.windows(3).position(|w| (w[0] & 0xf0) == 0x80 && w[1] == 50).unwrap();
        assert!(on < off, "zero-length note: off at byte {off} before on at byte {on}");

        let pair = vec![n(1.0, 0.5, 50, 100), n(1.5, 0.5, 50, 100)];
        let bytes = midi_bytes(&[vec![], pair, vec![]]);
        let ons: Vec<usize> = bytes.windows(3).enumerate().filter(|(_, w)| (w[0] & 0xf0) == 0x90 && w[1] == 50 && w[2] > 0).map(|(i, _)| i).collect();
        let offs: Vec<usize> = bytes.windows(3).enumerate().filter(|(_, w)| (w[0] & 0xf0) == 0x80 && w[1] == 50).map(|(i, _)| i).collect();
        assert_eq!((ons.len(), offs.len()), (2, 2));
        assert!(offs[0] < ons[1], "the first note's off must come before the second note's on at the shared tick");
    }
}
