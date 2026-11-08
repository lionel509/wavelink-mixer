//! Note data for the Stems feature: the rolling timeline, the SMF writer, and the private fluidsynth player.
//! Unit 1 skeleton: bodies are `todo!()` until Unit 5.
#![allow(dead_code, unused_imports, unused_variables)]
use crate::config::glib_home;
use std::path::PathBuf;

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

pub struct Timeline {}

impl Timeline {
    pub fn new() -> Self {
        todo!()
    }

    pub fn add(&mut self, stem: usize, notes: &[Note]) {
        todo!()
    }

    /// Notes with t < to and t + dur >= from.
    pub fn window(&self, stem: usize, from: f64, to: f64) -> Vec<Note> {
        todo!()
    }

    /// Drops notes with t + dur < before.
    pub fn prune(&mut self, before: f64) {
        todo!()
    }

    pub fn drain_all(&mut self) -> [Vec<Note>; 3] {
        todo!()
    }
}

/// SMF 1, 480 tpq. Track 0 is the tempo map; then drums (ch 9, pitch 0 -> key 38), bass (ch 1, prog 33), other (ch 2, prog 0).
pub fn midi_bytes(tracks: &[Vec<Note>; 3]) -> Vec<u8> {
    todo!()
}

/// Per note: (t + lag, "noteon C K V") and (t + lag + d, "noteoff C K"). Drums: K = 38 if pitch == 0, d = 0.1.
pub fn synth_events(stem: usize, notes: &[Note], lag: f64) -> Vec<(f64, String)> {
    todo!()
}

/// Removes and returns the lines with time <= now, in ascending time order (stable).
pub fn due(pending: &mut Vec<(f64, String)>, now: f64) -> Vec<String> {
    todo!()
}

pub fn synth_bin() -> &'static str {
    todo!()
}

/// glib_home()/.local/share/wavelink/soundfonts/FluidR3_GM.sf2, if it exists.
pub fn synth_font() -> Option<PathBuf> {
    todo!()
}

/// ["-a", "pipewire", "-n", "-r", "48000", "-g", "0.6"]; the font is appended by spawn.
pub fn synth_args() -> Vec<String> {
    todo!()
}

/// PipeWire props routing the synth into `node`.
pub fn synth_props(node: &str) -> String {
    todo!()
}

pub struct Synth {}

impl Synth {
    /// PIPEWIRE_PROPS from synth_props; stdin piped; then "prog 1 33\nprog 2 0\n". No font -> Err("soundfont missing").
    pub fn spawn(out_node: &str) -> Result<Synth, String> {
        todo!()
    }

    pub fn queue(&mut self, ev: Vec<(f64, String)>) {
        todo!()
    }

    /// Writes the due lines and flushes; returns the count.
    pub fn pump(&mut self, now: f64) -> usize {
        todo!()
    }
}

impl Drop for Synth {
    /// Writes "reset\n", kills and waits.
    fn drop(&mut self) {
        todo!()
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
