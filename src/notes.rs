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

/// A note that can still change. `cap` is the earliest later onset of its pitch ever accepted (`f64::INFINITY`
/// when none). `cool` is set when the note was not listed at the last `set_open`.
struct Live {
    n: Note,
    cap: f64,
    cool: bool,
}

/// Notes per stem. INVARIANT: a note can change only while it is live (from its `add` until the second `set_open`
/// that does not list it, or `close_all`); while it is live it ends no later than the next onset of its pitch the
/// timeline has accepted (a strike always wins over an earlier live note); once it leaves live it is final, is never
/// read or changed again, and no later note of its pitch may start before its end.
pub struct Timeline {
    live: [Vec<Live>; 3],
    done: [Vec<Note>; 3],
    /// Per stem and pitch, the end of the last final note of that pitch.
    floor: [[f64; 256]; 3],
}

/// The only write to a note's `dur`: the note ends at `want`, or at its `cap`.
fn fit(m: &mut Live, want: f64) {
    m.n.dur = want.min(m.cap - m.n.t);
}

/// The only way a note becomes final.
fn retire(done: &mut Vec<Note>, floor: &mut [f64; 256], n: Note) {
    let p = n.pitch as usize;
    floor[p] = floor[p].max(n.t + n.dur);
    done.push(n);
}

impl Timeline {
    pub fn new() -> Self {
        Timeline {
            live: [Vec::new(), Vec::new(), Vec::new()],
            done: [Vec::new(), Vec::new(), Vec::new()],
            floor: [[f64::NEG_INFINITY; 256]; 3],
        }
    }

    /// Adds notes. Of two notes of one pitch that overlap, the earlier one ends where the later one starts (an earlier
    /// window closed it past a strike it did not see, #71), and two notes with the same onset collapse into the
    /// longer one. A note that starts inside a final note of its pitch keeps only the part after that note's end.
    pub fn add(&mut self, stem: usize, notes: &[Note]) {
        for new in notes {
            let p = new.pitch as usize;
            let mut t = new.t;
            let mut want = new.dur;
            let floor = self.floor[stem][p];
            if t < floor {
                let end = t + want;
                if end <= floor {
                    continue;
                }
                want = end - floor;
                t = floor;
            }
            let mut cap = f64::INFINITY;
            let mut same: Option<usize> = None;
            let live = &mut self.live[stem];
            for (i, m) in live.iter_mut().enumerate() {
                if m.n.pitch != new.pitch {
                    continue;
                }
                if m.n.t < t {
                    m.cap = m.cap.min(t);
                    let d = m.n.dur;
                    fit(m, d);
                } else if m.n.t > t {
                    cap = cap.min(m.n.t);
                } else {
                    same = Some(i);
                }
            }
            match same {
                Some(i) => {
                    let d = live[i].n.dur.max(want);
                    fit(&mut live[i], d);
                }
                None => {
                    let mut m = Live { n: Note { t, dur: 0.0, pitch: new.pitch, vel: new.vel }, cap, cool: false };
                    fit(&mut m, want);
                    live.push(m);
                }
            }
        }
    }

    /// Notes with t < to and t + dur >= from, in time order.
    pub fn window(&self, stem: usize, from: f64, to: f64) -> Vec<Note> {
        let mut v: Vec<Note> = self.done[stem]
            .iter()
            .cloned()
            .chain(self.live[stem].iter().map(|m| m.n.clone()))
            .filter(|n| n.t < to && n.t + n.dur >= from)
            .collect();
        v.sort_by(|a, b| a.t.total_cmp(&b.t));
        v
    }

    /// Drops notes with t + dur < before.
    pub fn prune(&mut self, before: f64) {
        for v in self.done.iter_mut() {
            v.retain(|n| n.t + n.dur >= before);
        }
        for v in self.live.iter_mut() {
            v.retain(|m| m.n.t + m.n.dur >= before);
        }
    }

    /// Takes every note, final or not, after closing all of them.
    pub fn drain_all(&mut self) -> [Vec<Note>; 3] {
        self.close_all();
        self.drain_closed()
    }

    /// The window tick: marks the notes a later window may still change, by (t, pitch) per stem. A live note not
    /// listed cools for one window; a cooling note not listed again is retired.
    pub fn set_open(&mut self, stem: usize, open: &[Note]) {
        let Timeline { live, done, floor } = self;
        live[stem].retain_mut(|m| {
            let listed = open.iter().any(|o| o.t == m.n.t && o.pitch == m.n.pitch);
            if listed {
                m.cool = false;
                true
            } else if !m.cool {
                m.cool = true;
                true
            } else {
                retire(&mut done[stem], &mut floor[stem], m.n.clone());
                false
            }
        });
    }

    /// Every note is closed (a stopped session): all live notes become final.
    pub fn close_all(&mut self) {
        let Timeline { live, done, floor } = self;
        for stem in 0..3 {
            for m in live[stem].drain(..) {
                retire(&mut done[stem], &mut floor[stem], m.n);
            }
        }
    }

    /// Removes and returns the final notes, per stem, in time order.
    pub fn drain_closed(&mut self) -> [Vec<Note>; 3] {
        let mut out: [Vec<Note>; 3] = Default::default();
        for stem in 0..3 {
            let mut v = std::mem::take(&mut self.done[stem]);
            v.sort_by(|a, b| a.t.total_cmp(&b.t));
            out[stem] = v;
        }
        out
    }

    /// Clones of the live (held) notes, per stem.
    pub fn held(&self) -> [Vec<Note>; 3] {
        let mut out: [Vec<Note>; 3] = Default::default();
        for stem in 0..3 {
            out[stem] = self.live[stem].iter().map(|m| m.n.clone()).collect();
        }
        out
    }

    /// Lengthens live notes that a carry-over window extended: `ext` is (previous end, note with its new dur).
    /// A note that is already final is not changed; a live note stops at a later strike already accepted.
    pub fn extend(&mut self, stem: usize, ext: &[(f64, Note)]) {
        for (_, u) in ext {
            if let Some(m) = self.live[stem].iter_mut().find(|m| m.n.t == u.t && m.n.pitch == u.pitch) {
                fit(m, u.dur);
            }
        }
    }

    /// A failed Save hands its notes back unchanged; they were final when taken.
    pub fn put_back(&mut self, tracks: &[Vec<Note>; 3]) {
        for i in 0..3 {
            self.done[i].extend(tracks[i].iter().cloned());
        }
    }
}

/// What Save writes: the closed notes, and how many notes are still held. A stopped session closes every note
/// first.
pub fn take_for_save(tl: &mut Timeline, running: bool) -> ([Vec<Note>; 3], usize) {
    if !running {
        tl.close_all();
    }
    let held = tl.held().iter().map(|v| v.len()).sum();
    (tl.drain_closed(), held)
}

/// The status line after a Save.
pub fn save_status(path: &str, n: usize, held: usize) -> String {
    match (n, held) {
        (0, 0) => "no notes yet".to_string(),
        (0, h) => format!("no finished notes yet ({h} held)"),
        (n, 0) => format!("saved {path} ({n} notes)"),
        (n, h) => format!("saved {path} ({n} notes, {h} held for the next save)"),
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
        let mut order: Vec<&Note> = notes.iter().collect();
        order.sort_by(|a, b| a.t.total_cmp(&b.t));
        // One entry per (key, on tick): same-key notes that start on one tick collapse to the longer, the louder.
        // (on tick, off tick, key, vel) in on order.
        let mut hits: Vec<(u64, u64, u8, u8)> = Vec::with_capacity(order.len());
        let mut last: [Option<usize>; 128] = [None; 128];
        for n in order.iter() {
            let k = key(stem, n.pitch);
            let dur = if stem == 0 { DRUM_DUR } else { n.dur };
            let on = tick(n.t);
            // at least one tick after the note-on, so a zero-length note still stops after it starts
            let off = tick(n.t + dur).max(on + 1);
            let vel = n.vel.clamp(1, 127);
            match last[k as usize] {
                Some(i) if hits[i].0 == on => {
                    hits[i].1 = hits[i].1.max(off);
                    hits[i].3 = hits[i].3.max(vel);
                }
                _ => {
                    last[k as usize] = Some(hits.len());
                    hits.push((on, off, k, vel));
                }
            }
        }
        // A note of a key ends no later than the next start of that key on this track (ported from the piano branch).
        let mut next_on: [Option<u64>; 128] = [None; 128];
        for i in (0..hits.len()).rev() {
            let (on, off, k, _) = hits[i];
            let mut off = off;
            if let Some(nx) = next_on[k as usize] {
                off = off.min(nx.max(on + 1));
            }
            hits[i].1 = off;
            next_on[k as usize] = Some(on);
        }
        for (on, off, k, vel) in hits {
            ev.push((on, 1, vec![0x90 | ch, k, vel]));
            ev.push((off, 0, vec![0x80 | ch, k, 0]));
        }
        out.extend(track_chunk(&mut ev));
    }
    out
}

/// Per note: (t + lag, "noteon C K V") and (t + lag + d, "noteoff C K"). Drums: K = 38 if pitch == 0, d = 0.1.
pub fn synth_events(stem: usize, notes: &[Note], lag: f64) -> Vec<(f64, String)> {
    let (c, _) = PROGRAMS[stem];
    let mut order: Vec<&Note> = notes.iter().collect();
    order.sort_by(|a, b| a.t.total_cmp(&b.t));
    // Same rule as midi_bytes: a note ends no later than the next start of its key.
    let mut offs = vec![0f64; order.len()];
    let mut next_on: [Option<f64>; 128] = [None; 128];
    for i in (0..order.len()).rev() {
        let n = order[i];
        let k = key(stem, n.pitch) as usize;
        let d = if stem == 0 { DRUM_DUR } else { n.dur };
        let mut off = n.t + d;
        if let Some(nx) = next_on[k] {
            off = off.min(nx.max(n.t));
        }
        offs[i] = off;
        next_on[k] = Some(n.t);
    }
    let mut out = Vec::with_capacity(notes.len() * 2);
    for (i, n) in order.iter().enumerate() {
        let k = key(stem, n.pitch);
        let vel = n.vel.clamp(1, 127);
        out.push((n.t + lag, format!("noteon {c} {k} {vel}")));
        out.push((offs[i] + lag, format!("noteoff {c} {k}")));
    }
    out
}

/// Moves pending noteoffs of carried notes to their new end. `ext` is (previous end, note with its new dur).
/// An off that already played is not in `pending` and is left alone.
pub fn retime_offs(pending: &mut Vec<(f64, String)>, stem: usize, ext: &[(f64, Note)], lag: f64) {
    let (c, _) = PROGRAMS[stem];
    for (old_end, u) in ext {
        let k = key(stem, u.pitch);
        let line = format!("noteoff {c} {k}");
        if let Some(e) = pending.iter_mut().find(|e| e.1 == line && (e.0 - (old_end + lag)).abs() < 1e-6) {
            e.0 = u.t + u.dur + lag;
        }
    }
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

    /// Moves the pending offs of carried notes; see retime_offs.
    pub fn retime(&mut self, stem: usize, ext: &[(f64, Note)], lag: f64) {
        retime_offs(&mut self.pending, stem, ext, lag);
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

    #[test]
    fn add_clips_earlier_same_pitch_note_at_new_strike() {
        let mut tl = Timeline::new();
        tl.add(1, &[n(1.0, 0.5, 43, 90), n(1.0, 0.9, 50, 90)]);
        tl.add(1, &[n(1.3, 0.2, 43, 90)]);
        let w = tl.window(1, 0.0, 9.0);
        assert!((w[0].dur - 0.3).abs() < 1e-9, "{}", w[0].dur);
        assert!((w[1].dur - 0.9).abs() < 1e-9);
        assert_eq!(w.len(), 3);
    }

    #[test]
    fn add_clips_long_note_at_restrike_past_four_seconds() {
        let mut tl = Timeline::new();
        tl.add(1, &[n(0.0, 5.0, 60, 90)]);
        tl.add(1, &[n(4.9, 0.3, 60, 90)]);
        let w = tl.window(1, 0.0, 10.0);
        assert!((w[0].dur - 4.9).abs() < 1e-9, "{}", w[0].dur);
    }

    #[test]
    fn add_same_onset_same_pitch_keeps_only_the_longer() {
        let mut tl = Timeline::new();
        tl.add(1, &[n(1.0, 0.5, 60, 90)]);
        tl.add(1, &[n(1.0, 0.2, 60, 90)]);
        let w = tl.window(1, 0.0, 9.0);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].dur, 0.5);
        let mut rev = Timeline::new();
        rev.add(1, &[n(1.0, 0.2, 60, 90), n(1.0, 0.5, 60, 90)]);
        let w = rev.window(1, 0.0, 9.0);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].dur, 0.5);
    }

    #[test]
    fn add_clips_within_one_batch() {
        let mut tl = Timeline::new();
        tl.add(1, &[n(1.0, 1.0, 60, 90), n(1.5, 0.2, 60, 90)]);
        let w = tl.window(1, 0.0, 9.0);
        assert_eq!(w.len(), 2);
        assert!((w[0].dur - 0.5).abs() < 1e-9, "{}", w[0].dur);
    }

    #[test]
    fn add_same_onset_merge_is_clipped_by_a_later_strike() {
        let mut tl = Timeline::new();
        tl.add(1, &[n(1.0, 0.2, 60, 90), n(1.3, 0.4, 60, 90)]);
        tl.add(1, &[n(1.0, 0.5, 60, 90)]);
        let w = tl.window(1, 0.0, 9.0);
        assert_eq!(w.len(), 2);
        assert!((w[0].dur - 0.3).abs() < 1e-9, "overlap: {:?}", w);
        assert!(w[0].t + w[0].dur <= w[1].t + 1e-9, "overlap: {:?}", w);
    }

    #[test]
    fn extend_stops_at_a_strike_already_in_the_timeline() {
        let mut tl = Timeline::new();
        tl.add(1, &[n(0.0, 0.5, 60, 90)]);
        tl.add(1, &[n(1.3, 0.4, 60, 90)]);
        tl.extend(1, &[(0.5, n(0.0, 3.0, 60, 90))]);
        let w = tl.window(1, 0.0, 9.0);
        assert_eq!(w.len(), 2);
        assert!((w[0].dur - 1.3).abs() < 1e-9, "overlap after ext: {:?}", w);
        assert!(w[0].t + w[0].dur <= w[1].t + 1e-9, "overlap after ext: {:?}", w);
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

    /// (abs_tick, status & 0xf0, key) of every note on/off in one MTrk chunk (0 = tempo map, 1 = drums, 2 = bass, 3 = other).
    fn track_events(bytes: &[u8], track: usize) -> Vec<(u64, u8, u8)> {
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
                let (delta, next) = vlq(body, i);
                i = next;
                tick += delta as u64;
                let st = body[i];
                if st == 0xff {
                    let (plen, next) = vlq(body, i + 2);
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

    fn lcg(s: &mut u64) -> f64 {
        *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((*s >> 33) as f64) / (1u64 << 31) as f64
    }

    #[test]
    fn midi_same_pitch_overlap_clips_first_at_second_onset() {
        let tracks = [vec![], vec![n(1.0, 1.0, 50, 100), n(1.5, 1.0, 50, 100)], vec![]];
        let b = midi_bytes(&tracks);
        let ev: Vec<(u64, u8, u8)> = track_events(&b, 2).into_iter().filter(|e| e.2 == 50).collect();
        assert_eq!(ev, vec![(0, 0x90, 50), (480, 0x80, 50), (480, 0x90, 50), (1440, 0x80, 50)]);
    }

    #[test]
    fn midi_never_stacks_same_key() {
        let mut s = 7u64;
        let mut notes = Vec::new();
        for _ in 0..30 {
            let k = 50 + (lcg(&mut s) * 4.0) as u8;
            let t = lcg(&mut s) * 5.0;
            let dur = 0.05 + lcg(&mut s) * 1.45;
            notes.push(n(t, dur, k, 100));
        }
        let b = midi_bytes(&[vec![], notes, vec![]]);
        let ev = track_events(&b, 2);
        assert_eq!(ev.len(), 60, "one on and one off per note");
        for k in 50..54u8 {
            let seq: Vec<(u64, u8)> = ev.iter().filter(|e| e.2 == k).map(|e| (e.0, e.1)).collect();
            for pair in seq.chunks(2) {
                assert_eq!(pair.len(), 2, "key {k} has an unmatched event");
                assert_eq!(pair[0].1, 0x90, "key {k} starts with an on");
                assert_eq!(pair[1].1, 0x80, "key {k}: an on is followed by its off, never another on");
                assert!(pair[1].0 >= pair[0].0 + 1, "key {k}: off must come after its on");
            }
            assert_eq!(seq.len() % 2, 0);
        }
    }

    #[test]
    fn synth_same_pitch_overlap_clips_first() {
        let mut ev = synth_events(1, &[n(1.0, 1.0, 50, 100), n(1.5, 1.0, 50, 100)], 0.0);
        let off_a = ev.iter().filter(|e| e.1 == "noteoff 1 50").map(|e| e.0).fold(f64::INFINITY, f64::min);
        assert!((off_a - 1.5).abs() < 1e-9, "first off must land at the second on: {off_a}");
        let got = due(&mut ev, 1e9);
        assert_eq!(got, vec!["noteon 1 50 100", "noteoff 1 50", "noteon 1 50 100", "noteoff 1 50"]);
    }

    #[test]
    fn retime_offs_moves_pending_off() {
        let mut pending = synth_events(1, &[n(10.0, 0.9, 60, 100)], 1.5);
        retime_offs(&mut pending, 1, &[(10.9, n(10.0, 2.4, 60, 100))], 1.5);
        let off = pending.iter().find(|e| e.1 == "noteoff 1 60").unwrap().0;
        let on = pending.iter().find(|e| e.1 == "noteon 1 60 100").unwrap().0;
        assert!((off - 13.9).abs() < 1e-9, "off moves to new end + lag: {off}");
        assert!((on - 11.5).abs() < 1e-9, "on is untouched: {on}");
        // the old off has already played: nothing to move, nothing added
        let before = pending.clone();
        retime_offs(&mut pending, 1, &[(99.0, n(10.0, 5.0, 60, 100))], 1.5);
        assert_eq!(pending, before);
    }

    #[test]
    fn timeline_extend_lengthens_matching_note() {
        let mut tl = Timeline::new();
        tl.add(1, &[n(10.0, 0.9, 60, 100), n(10.0, 0.9, 62, 100)]);
        tl.extend(1, &[(10.9, n(10.0, 2.4, 60, 100)), (10.9, n(10.0, 2.4, 64, 100))]);
        let got = tl.window(1, 0.0, 100.0);
        assert_eq!(got.len(), 2, "no note is created for pitch 64");
        let p60 = got.iter().find(|m| m.pitch == 60).unwrap();
        let p62 = got.iter().find(|m| m.pitch == 62).unwrap();
        assert_eq!(p60.dur, 2.4);
        assert_eq!(p62.dur, 0.9);
    }

    #[test]
    fn timeline_drain_closed_keeps_open_notes() {
        let mut tl = Timeline::new();
        tl.add(1, &[n(1.0, 0.5, 50, 90), n(2.0, 0.5, 52, 90)]);
        tl.set_open(1, &[n(2.0, 0.5, 52, 90)]);
        assert!(tl.drain_closed()[1].is_empty(), "a note that closed in the last window waits one window");
        assert_eq!(tl.held()[1], vec![n(1.0, 0.5, 50, 90), n(2.0, 0.5, 52, 90)]);
        tl.set_open(1, &[n(2.0, 0.5, 52, 90)]);
        let closed = tl.drain_closed();
        assert_eq!(closed[1], vec![n(1.0, 0.5, 50, 90)]);
        assert_eq!(tl.held()[1], vec![n(2.0, 0.5, 52, 90)]);
        assert_eq!(tl.drain_all()[1], vec![n(2.0, 0.5, 52, 90)], "the held note is still in the timeline");
    }

    #[test]
    fn take_for_save_when_stopped_takes_everything() {
        let (a, b) = (n(1.0, 0.5, 50, 90), n(2.0, 0.5, 52, 90));
        let mut tl = Timeline::new();
        tl.add(1, &[a.clone(), b.clone()]);
        tl.set_open(1, &[b.clone()]);
        tl.set_open(1, &[b.clone()]);
        let (tracks, held) = take_for_save(&mut tl, true);
        assert_eq!(held, 1);
        assert_eq!(tracks[1], vec![a.clone()], "a held note is not written while running");
        let mut stopped = Timeline::new();
        stopped.add(1, &[a.clone(), b.clone()]);
        stopped.set_open(1, &[b.clone()]);
        let (tracks, held) = take_for_save(&mut stopped, false);
        assert_eq!(held, 0, "a stopped session holds nothing");
        assert_eq!(tracks[1], vec![a, b]);
    }

    fn same_key_overlaps(v: &[Note]) -> Vec<(Note, Note)> {
        let mut s = v.to_vec();
        s.sort_by(|a, b| a.t.total_cmp(&b.t).then(a.pitch.cmp(&b.pitch)));
        let mut out = Vec::new();
        for i in 0..s.len() {
            for j in i + 1..s.len() {
                if s[i].pitch == s[j].pitch && s[j].t < s[i].t + s[i].dur - 1e-9 {
                    out.push((s[i].clone(), s[j].clone()));
                }
            }
        }
        out
    }

    fn by_start(v: &mut Vec<Note>) {
        v.sort_by(|a, b| a.t.total_cmp(&b.t).then(a.pitch.cmp(&b.pitch)).then(a.dur.total_cmp(&b.dur)));
    }

    #[test]
    fn breaker_r6_extend_after_restrike_was_drained_overlaps_it() {
        let mut tl = Timeline::new();
        tl.add(1, &[n(0.0, 0.5, 40, 90)]);
        tl.add(1, &[n(0.125, 0.3, 40, 90)]);
        tl.extend(1, &[(0.5, n(0.0, 2.0, 40, 90))]);
        tl.set_open(1, &[n(0.0, 2.0, 40, 90)]);
        let drained = tl.drain_closed();
        tl.extend(1, &[(2.0, n(0.0, 2.5, 40, 90))]);
        let (fin, _) = take_for_save(&mut tl, false);
        let mut all = drained[1].clone();
        all.extend(fin[1].iter().cloned());
        assert!(same_key_overlaps(&all).is_empty(), "mid-save + final overlap: {:?}", all);
    }

    fn r6_seven_calls(mid: bool) -> Vec<Note> {
        let mut tl = Timeline::new();
        tl.add(1, &[n(0.0, 0.5, 40, 90)]);
        tl.add(1, &[n(0.125, 0.3, 40, 90)]);
        tl.extend(1, &[(0.5, n(0.0, 2.0, 40, 90))]);
        tl.set_open(1, &[n(0.0, 2.0, 40, 90)]);
        let mut out = Vec::new();
        if mid {
            out.extend(tl.drain_closed()[1].iter().cloned());
        }
        tl.extend(1, &[(2.0, n(0.0, 2.5, 40, 90))]);
        out.extend(take_for_save(&mut tl, false).0[1].iter().cloned());
        by_start(&mut out);
        out
    }

    #[test]
    fn breaker_r6_mid_save_plus_final_equals_single_after_restrike() {
        let mid = r6_seven_calls(true);
        let single = r6_seven_calls(false);
        assert_eq!(mid, single, "mid + final differ from one final save");
        assert_eq!(single, vec![n(0.0, 0.125, 40, 90), n(0.125, 0.3, 40, 90)]);
    }

    #[test]
    fn extend_respects_a_restrike_that_was_already_drained() {
        let mut tl = Timeline::new();
        tl.add(1, &[n(0.0, 0.5, 40, 90)]);
        tl.add(1, &[n(0.125, 0.3, 40, 90)]);
        tl.set_open(1, &[n(0.0, 0.5, 40, 90)]);
        tl.set_open(1, &[n(0.0, 0.5, 40, 90)]);
        assert_eq!(tl.drain_closed()[1], vec![n(0.125, 0.3, 40, 90)]);
        tl.extend(1, &[(0.5, n(0.0, 2.5, 40, 90))]);
        assert_eq!(take_for_save(&mut tl, false).0[1], vec![n(0.0, 0.125, 40, 90)]);
    }

    #[test]
    fn a_closed_note_waits_one_window_so_the_next_strike_still_clips_it() {
        let mut tl = Timeline::new();
        tl.add(1, &[n(1.0, 0.8, 60, 90)]);
        tl.set_open(1, &[]);
        assert!(tl.drain_closed()[1].is_empty());
        assert_eq!(tl.held()[1].len(), 1);
        tl.add(1, &[n(1.5, 0.3, 60, 90)]);
        tl.set_open(1, &[]);
        assert_eq!(tl.drain_closed()[1], vec![n(1.0, 0.5, 60, 90)]);
        assert_eq!(tl.held()[1], vec![n(1.5, 0.3, 60, 90)]);
    }

    fn final_note_is_never_changed(mid: bool) -> Vec<Note> {
        let mut tl = Timeline::new();
        tl.add(1, &[n(1.0, 1.0, 60, 90)]);
        tl.set_open(1, &[]);
        tl.set_open(1, &[]);
        let mut out = Vec::new();
        if mid {
            out.extend(tl.drain_closed()[1].iter().cloned());
        }
        tl.add(1, &[n(1.5, 1.0, 60, 90)]);
        tl.add(1, &[n(1.2, 0.3, 60, 90)]);
        tl.extend(1, &[(2.0, n(1.0, 5.0, 60, 90))]);
        out.extend(take_for_save(&mut tl, false).0[1].iter().cloned());
        by_start(&mut out);
        out
    }

    #[test]
    fn a_final_note_is_never_changed_and_a_late_strike_starts_at_its_end() {
        let want = vec![n(1.0, 1.0, 60, 90), n(2.0, 0.5, 60, 90)];
        assert_eq!(final_note_is_never_changed(false), want);
        assert_eq!(final_note_is_never_changed(true), want);
    }

    #[test]
    fn put_back_restores_the_taken_notes_unchanged() {
        let mut tl = Timeline::new();
        tl.add(1, &[n(0.0, 3.0, 60, 90), n(1.0, 0.5, 62, 90)]);
        tl.set_open(1, &[]);
        tl.set_open(1, &[]);
        let (taken, held) = take_for_save(&mut tl, true);
        assert_eq!(taken[1].len(), 2);
        assert_eq!(held, 0);
        tl.add(1, &[n(2.0, 1.5, 60, 90)]);
        tl.put_back(&taken);
        assert_eq!(take_for_save(&mut tl, false).0[1], vec![n(0.0, 3.0, 60, 90), n(1.0, 0.5, 62, 90), n(3.0, 0.5, 60, 90)]);
    }

    enum Op {
        Add(Vec<Note>),
        Ext(Vec<(f64, Note)>),
        Open(Vec<Note>),
        CloseAll,
        Drain,
        FailedSave,
    }

    fn run_ops(ops: &[Op], mid: bool) -> Vec<Note> {
        let mut tl = Timeline::new();
        let mut out: Vec<Note> = Vec::new();
        for op in ops {
            match op {
                Op::Add(v) => tl.add(1, v),
                Op::Ext(v) => tl.extend(1, v),
                Op::Open(v) => tl.set_open(1, v),
                Op::CloseAll => tl.close_all(),
                Op::Drain => {
                    if mid {
                        out.extend(tl.drain_closed()[1].iter().cloned());
                    }
                }
                Op::FailedSave => {
                    if mid {
                        let t = tl.drain_closed();
                        tl.put_back(&t);
                    }
                }
            }
        }
        out.extend(take_for_save(&mut tl, false).0[1].iter().cloned());
        by_start(&mut out);
        out
    }

    #[test]
    fn timeline_invariant_random_ops() {
        for seed in 1..=300u64 {
            let mut s = seed;
            let mut known: Vec<Note> = Vec::new();
            let mut ops: Vec<Op> = Vec::new();
            for _ in 0..60 {
                let r = lcg(&mut s);
                if r < 0.40 {
                    let count = 1 + (lcg(&mut s) * 3.0) as usize;
                    let mut batch = Vec::new();
                    for _ in 0..count {
                        let t = (lcg(&mut s) * 48.0).floor() * 0.125;
                        let dur = 0.125 * (1.0 + (lcg(&mut s) * 12.0).floor());
                        let pitch = 40 + (lcg(&mut s) * 2.0) as u8;
                        let note = n(t, dur, pitch, 90);
                        known.push(note.clone());
                        batch.push(note);
                    }
                    ops.push(Op::Add(batch));
                } else if r < 0.60 {
                    if known.is_empty() {
                        continue;
                    }
                    let k = known[(lcg(&mut s) * known.len() as f64) as usize].clone();
                    let dur = 0.125 * (1.0 + (lcg(&mut s) * 24.0).floor());
                    ops.push(Op::Ext(vec![(k.t + k.dur, Note { dur, ..k.clone() })]));
                } else if r < 0.80 {
                    let from = known.len().saturating_sub(6);
                    let open: Vec<Note> = known[from..].iter().filter(|_| lcg(&mut s) < 0.5).cloned().collect();
                    ops.push(Op::Open(open));
                } else if r < 0.84 {
                    ops.push(Op::CloseAll);
                } else if r < 0.94 {
                    ops.push(Op::Drain);
                } else {
                    ops.push(Op::FailedSave);
                }
            }
            let single = run_ops(&ops, false);
            let mid = run_ops(&ops, true);
            assert_eq!(mid, single, "seed {seed}: mid-save + final differ from one final save");
            assert!(same_key_overlaps(&single).is_empty(), "seed {seed}: {:?}", single);
            assert!(single.iter().all(|x| x.dur > 0.0), "seed {seed}: {:?}", single);
        }
    }

    #[test]
    fn timeline_invariant_writer_like_windows_match_the_oracle() {
        for seed in 1..=200u64 {
            let mut s = seed ^ 0xabcdef;
            let mut mid = Timeline::new();
            let mut single = Timeline::new();
            let mut mid_out: Vec<Note> = Vec::new();
            let mut asked: Vec<Note> = Vec::new();
            let mut open: Vec<Note> = Vec::new();
            for w in 0..40usize {
                let w0 = 0.5 * w as f64;
                let limit = w0 + 1.0;
                let mut ext: Vec<(f64, Note)> = Vec::new();
                let mut next_open: Vec<Note> = Vec::new();
                for o in open.iter() {
                    let stays = lcg(&mut s) < 0.6;
                    let end = if stays {
                        limit + 0.125 * (1.0 + (lcg(&mut s) * 8.0).floor())
                    } else {
                        limit - 0.125 * (lcg(&mut s) * 4.0).floor()
                    };
                    let u = Note { dur: end - o.t, ..o.clone() };
                    if u.dur != o.dur {
                        ext.push((o.t + o.dur, u.clone()));
                        if let Some(a) = asked.iter_mut().find(|a| a.t == o.t && a.pitch == o.pitch) {
                            a.dur = u.dur;
                        }
                    }
                    if stays {
                        next_open.push(u);
                    }
                }
                let mut batch: Vec<Note> = Vec::new();
                for j in 0..4usize {
                    let t = w0 + 0.125 * j as f64;
                    for pitch in [40u8, 41] {
                        if lcg(&mut s) >= 0.35 {
                            continue;
                        }
                        let closed = lcg(&mut s) < 0.5;
                        let end = if closed {
                            (t + 0.125 * (1.0 + (lcg(&mut s) * 4.0).floor())).min(limit)
                        } else {
                            limit + 0.125 * (1.0 + (lcg(&mut s) * 8.0).floor())
                        };
                        let note = n(t, end - t, pitch, 90);
                        batch.push(note.clone());
                        asked.push(note.clone());
                        if !closed {
                            next_open.push(note);
                        }
                    }
                }
                for tl in [&mut mid, &mut single] {
                    tl.add(1, &batch);
                    tl.extend(1, &ext);
                    tl.set_open(1, &next_open);
                }
                if lcg(&mut s) < 0.3 {
                    mid_out.extend(mid.drain_closed()[1].iter().cloned());
                }
                open = next_open;
            }
            let mut single_out = take_for_save(&mut single, false).0[1].clone();
            mid_out.extend(take_for_save(&mut mid, false).0[1].iter().cloned());
            by_start(&mut single_out);
            by_start(&mut mid_out);
            let mut expected: Vec<Note> = asked
                .iter()
                .map(|a| {
                    let next = asked.iter().filter(|b| b.pitch == a.pitch && b.t > a.t).map(|b| b.t).fold(f64::INFINITY, f64::min);
                    Note { dur: a.dur.min(next - a.t), ..a.clone() }
                })
                .collect();
            by_start(&mut expected);
            assert_eq!(single_out, expected, "seed {seed}: single save against the oracle");
            assert_eq!(mid_out, expected, "seed {seed}: mid + final against the oracle");
        }
    }

    #[test]
    fn live_list_stays_small_under_a_long_held_note() {
        let mut tl = Timeline::new();
        let d = n(0.0, 600.0, 30, 90);
        tl.add(1, &[d.clone()]);
        tl.set_open(1, &[d.clone()]);
        for w in 1..=2000 {
            let t = 0.5 * w as f64;
            tl.add(1, &[n(t, 0.25, 40, 90), n(t + 0.25, 0.25, 41, 90)]);
            assert!(tl.live[1].len() <= 5, "window {w}: {} live", tl.live[1].len());
            tl.set_open(1, &[d.clone()]);
        }
        assert_eq!(tl.done[1].len(), 3998);
        assert_eq!(tl.held()[1].len(), 3);
    }

    #[test]
    fn save_status_texts() {
        assert_eq!(save_status("x.mid", 0, 0), "no notes yet");
        assert_eq!(save_status("x.mid", 0, 2), "no finished notes yet (2 held)");
        assert_eq!(save_status("x.mid", 3, 0), "saved x.mid (3 notes)");
        assert_eq!(save_status("x.mid", 3, 2), "saved x.mid (3 notes, 2 held for the next save)");
    }

    /// #61: two notes of one key on one tick collapse to one note that lasts to the longer end.
    #[test]
    fn midi_equal_tick_same_key_collapses_to_the_longer() {
        let tracks = [vec![], vec![n(1.0, 0.5, 50, 90), n(1.0, 1.0, 50, 100), n(1.0004, 0.2, 50, 80)], vec![]];
        let b = midi_bytes(&tracks);
        let ev: Vec<(u64, u8, u8)> = track_events(&b, 2).into_iter().filter(|e| e.2 == 50).collect();
        assert_eq!(ev, vec![(0, 0x90, 50), (960, 0x80, 50)]);
    }

    /// #61 in general: each key strictly alternates on and off, every off after its on, every on after the previous off.
    #[test]
    fn midi_keys_alternate_for_any_input() {
        for seed in 1..=50u64 {
            let mut s = seed;
            let mut drums = Vec::new();
            let mut bass = Vec::new();
            for i in 0..40 {
                let grid = lcg(&mut s) < 0.5;
                let t = if grid { (lcg(&mut s) * 20.0).floor() * 0.25 } else { lcg(&mut s) * 5.0 };
                let dur = if lcg(&mut s) < 0.1 { 0.0 } else { lcg(&mut s) * 1.5 };
                if i % 4 == 0 {
                    drums.push(n(t, dur, 0, 100));
                } else {
                    bass.push(n(t, dur, 50 + (lcg(&mut s) * 3.0) as u8, 100));
                }
            }
            let b = midi_bytes(&[drums, bass, vec![]]);
            for (track, keys) in [(1usize, vec![38u8]), (2usize, vec![50u8, 51, 52])] {
                for k in keys {
                    let seq: Vec<(u64, u8)> = track_events(&b, track).into_iter().filter(|e| e.2 == k).map(|e| (e.0, e.1)).collect();
                    assert_eq!(seq.len() % 2, 0, "seed {seed} key {k}: unmatched event");
                    let mut prev_off = 0u64;
                    for (i, pair) in seq.chunks(2).enumerate() {
                        assert_eq!(pair[0].1, 0x90, "seed {seed} key {k} pair {i}: starts with an on");
                        assert_eq!(pair[1].1, 0x80, "seed {seed} key {k} pair {i}: an on is followed by its off");
                        assert!(pair[1].0 > pair[0].0, "seed {seed} key {k} pair {i}: off must come after its on");
                        assert!(pair[0].0 >= prev_off, "seed {seed} key {k} pair {i}: on before the previous off");
                        prev_off = pair[1].0;
                    }
                }
            }
        }
    }
}
