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

/// SMF 1, 480 tpq. Track 0 is the tempo map; then drums (ch 9, pitch 0 -> key 38), bass (ch 1, prog 33), other (ch 2, prog 0).
pub fn midi_bytes(tracks: &[Vec<Note>; 3]) -> Vec<u8> {
    let t_min = tracks.iter().flatten().map(|n| n.t).fold(f64::INFINITY, f64::min);
    let t_min = if t_min.is_finite() { t_min } else { 0.0 };
    let tick = |t: f64| ((t - t_min) * TICKS_PER_SEC).round().max(0.0) as u64;

    let mut out = Vec::new();
    out.extend_from_slice(b"MThd");
    out.extend_from_slice(&6u32.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes()); // format 1
    out.extend_from_slice(&4u16.to_be_bytes()); // track count: tempo + 3 stems
    out.extend_from_slice(&(TPQ as u16).to_be_bytes());

    let mut tempo = vec![(0u64, 0u8, vec![0xff, 0x51, 0x03, 0x07, 0xa1, 0x20])];
    out.extend(track_chunk(&mut tempo));

    for (stem, notes) in tracks.iter().enumerate() {
        let (ch, prog) = PROGRAMS[stem];
        let mut ev = Vec::new();
        if stem != 0 {
            ev.push((0u64, 0u8, vec![0xc0 | ch, prog]));
        }
        for n in notes {
            let k = key(stem, n.pitch);
            let dur = if stem == 0 { DRUM_DUR } else { n.dur };
            let vel = n.vel.clamp(1, 127);
            ev.push((tick(n.t), 1, vec![0x90 | ch, k, vel]));
            ev.push((tick(n.t + dur), 0, vec![0x80 | ch, k, 0]));
        }
        out.extend(track_chunk(&mut ev));
    }
    out
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
}
