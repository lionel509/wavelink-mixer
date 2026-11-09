//! Performance layer between the tracker and what is heard (#23): turns the tracker's detections
//! into something that plays like a pianist. The tracker ends a note at the first real drop in its
//! key's energy, which on a pedalled recording sounds rushed and choppy, so here:
//! - a note the tracker releases keeps sounding (a virtual sustain pedal) until the harmony moves:
//!   a new bass note of another pitch class, a new note a step or less away, the same key struck
//!   again, too many voices, or HOLD_S after the tracker's off;
//! - a re-strike of a key within MERGE_S of its last strike is merged into it;
//! - notes above C7 are dropped (cymbal and hi-hat partials), at most RATE_MAX new notes per 100 ms;
//! - velocities are smoothed per lane and squeezed into a pianistic range, the top voice a bit louder;
//! - polyphony is capped at MAX_VOICES (pedalled notes go first).
//!
//! Streaming and causal: an on is passed through at once (no added latency); only offs are delayed.
//! Offline, `perform` replays notes through the same code and drops what is shorter than MIN_DUR_S.
use crate::notes::Note;
use crate::piano::Ev;

const HOLD_S: f64 = 2.5;
const MERGE_S: f64 = 0.12;
/// Shorter than this at the tracker's off: a ghost or a flam, not pedalled (and dropped offline).
const MIN_DUR_S: f64 = 0.13;
const TOP_PITCH: u8 = 96;
const MAX_VOICES: usize = 8;
const RATE_MAX: usize = 8;
const BASS_BELOW: u8 = 55;

#[derive(Clone, Copy)]
struct Held {
    pitch: u8,
    t_on: f64,
    /// The tracker's off time: the note now rings on the virtual pedal only.
    released: Option<f64>,
}

pub struct Pianist {
    /// Pedal hold after the tracker's off (MIXER_PIANO_HOLD, seconds) and the voice cap (MIXER_PIANO_VOICES).
    hold: f64,
    voices: usize,
    /// MIXER_PIANO_PEDALMERGE: a key re-detected within 300 ms of its release rings on.
    pedal_merge: bool,
    hands: bool,
    merge_s: f64,
    /// MIXER_PIANO_CC64: a real sustain pedal on the port instead of held notes.
    cc64: bool,
    pedal: bool,
    held: Vec<Held>,
    /// The tracker's on for this key was dropped or merged: swallow its off.
    swallow: [u8; 128],
    recent_ons: Vec<f64>,
    vel_avg: [f32; 3],
    /// The current bass note and when it was last struck.
    bass: Option<(u8, f64)>,
}

impl Default for Pianist {
    fn default() -> Self {
        Self::new()
    }
}

impl Pianist {
    pub fn new() -> Pianist {
        Pianist {
            hold: std::env::var("MIXER_PIANO_HOLD").ok().and_then(|v| v.parse().ok()).unwrap_or(HOLD_S),
            voices: std::env::var("MIXER_PIANO_VOICES").ok().and_then(|v| v.parse().ok()).unwrap_or(if crate::subtract::legato() || !crate::subtract::precision() { MAX_VOICES } else { 6 }),
            pedal_merge: crate::subtract::env_on("MIXER_PIANO_PEDALMERGE", true),
            hands: crate::subtract::env_on("MIXER_PIANO_HANDS", false),
            merge_s: std::env::var("MIXER_PIANO_MERGE_S").ok().and_then(|v| v.parse().ok()).unwrap_or(0.3),
            cc64: crate::subtract::env_on("MIXER_PIANO_CC64", false),
            pedal: false,
            held: Vec::with_capacity(16), swallow: [0; 128], recent_ons: Vec::with_capacity(16), vel_avg: [0.0; 3], bass: None }
    }

    fn release_where(&mut self, t: f64, out: &mut Vec<Ev>, f: impl Fn(&Held) -> bool) {
        let cc64 = self.cc64;
        self.held.retain(|h| {
            if f(h) {
                if !(cc64 && h.released.is_some()) {
                    out.push(Ev::Off { t: t.max(h.t_on), pitch: h.pitch });
                }
                false
            } else {
                true
            }
        });
    }

    /// Feeds tracker events; `now` is the tracker's clock (for the pedal timeout).
    pub fn push(&mut self, evs: &[Ev], now: f64, out: &mut Vec<Ev>) {
        for e in evs {
            match *e {
                Ev::On { t, pitch, vel } => self.on(t, pitch, vel, out),
                Ev::Off { t, pitch } => self.off(t, pitch, out),
                Ev::Pedal { .. } => out.push(*e),
            }
        }
        let hold = self.hold;
        let before = self.held.len();
        self.release_where(now, out, |h| h.released.is_some_and(|r| now - r > hold));
        if self.cc64 && self.pedal && self.held.len() < before && self.held.iter().all(|h| h.released.is_some()) {
            self.repedal(now, out);
        }
    }

    /// CC 64: lift (everything pedalled stops) and press again.
    fn repedal(&mut self, t: f64, out: &mut Vec<Ev>) {
        out.push(Ev::Pedal { t, down: false });
        out.push(Ev::Pedal { t, down: true });
        self.held.retain(|h| h.released.is_none());
    }

    fn on(&mut self, t: f64, pitch: u8, vel: u8, out: &mut Vec<Ev>) {
        if pitch > TOP_PITCH {
            self.swallow[pitch as usize] += 1;
            return;
        }
        if let Some(h) = self.held.iter_mut().find(|h| h.pitch == pitch) {
            // pedal merge: the key came back soon after the tracker let it go, while the pedal
            // still holds it: keep the ringing note instead of striking it again
            let pedal_merge = self.pedal_merge && h.released.is_some_and(|r| t - r < self.merge_s);
            if t - h.t_on < MERGE_S || pedal_merge {
                // still held: its pending off ends the merged note; already off: wait for the new off
                if h.released.take().is_none() {
                    self.swallow[pitch as usize] += 1;
                }
                return;
            }
        }
        self.recent_ons.retain(|&r| t - r < 0.1);
        if self.recent_ons.len() >= RATE_MAX {
            self.swallow[pitch as usize] += 1;
            return;
        }
        self.recent_ons.push(t);
        // the same key again, or the pedal changes: a new bass pitch class, or a step against a pedalled note
        // a low note that is not the bass's octave or fifth (left-hand root-fifth patterns keep the pedal)
        let mut new_bass = false;
        if pitch < BASS_BELOW {
            match self.bass {
                Some((bp, bt)) => {
                    let iv = (pitch as i32 - bp as i32).rem_euclid(12);
                    if iv != 0 && iv != 7 {
                        new_bass = true;
                        self.bass = Some((pitch, t));
                    } else if pitch <= bp || t - bt > self.hold {
                        self.bass = Some((pitch, t));
                    } else if iv == 0 {
                        self.bass = Some((bp, t));
                    }
                }
                None => self.bass = Some((pitch, t)),
            }
        }
        if self.cc64 {
            // a real pedal: down from the first note, changed on a new bass; single pedalled notes
            // cannot be lifted
            if !self.pedal {
                out.push(Ev::Pedal { t, down: true });
                self.pedal = true;
            } else if new_bass && self.held.iter().any(|h| h.released.is_some()) {
                self.repedal(t, out);
            }
            self.release_where(t, out, |h| h.pitch == pitch);
        } else {
            self.release_where(t, out, |h| {
                h.pitch == pitch || (h.released.is_some() && (new_bass || (h.pitch as i32 - pitch as i32).abs() <= 2))
            });
        }
        while self.held.len() >= self.voices {
            let i = self.held.iter().position(|h| h.released.is_some()).unwrap_or(0);
            let h = self.held.remove(i);
            if !(self.cc64 && h.released.is_some()) {
                out.push(Ev::Off { t: t.max(h.t_on), pitch: h.pitch });
            }
        }
        // velocity: smoothed per lane, squeezed to 48..112, top voice +8
        // per hand (MIXER_PIANO_HANDS: left below C4, right from C4) or per lane
        let lane = if self.hands { (pitch >= 60) as usize } else { crate::piano::lane(pitch).index() };
        let a = &mut self.vel_avg[lane];
        *a = if *a == 0.0 { vel as f32 } else { *a * 0.6 + vel as f32 * 0.4 };
        let v = 48.0 + (*a - 30.0).max(0.0) * 0.6;
        let top = pitch >= 60 && self.held.iter().all(|h| h.pitch < pitch || h.released.is_some());
        let v = if top { v + 8.0 } else { v - 4.0 };
        let vel = v.round().clamp(30.0, 112.0) as u8;
        self.held.push(Held { pitch, t_on: t, released: None });
        out.push(Ev::On { t, pitch, vel });
    }

    fn off(&mut self, t: f64, pitch: u8, out: &mut Vec<Ev>) {
        if self.swallow[pitch as usize] > 0 {
            self.swallow[pitch as usize] -= 1;
            return;
        }
        let Some(i) = self.held.iter().position(|h| h.pitch == pitch && h.released.is_none()) else {
            return;
        };
        if t - self.held[i].t_on < MIN_DUR_S {
            self.held.remove(i);
            out.push(Ev::Off { t, pitch });
        } else {
            self.held[i].released = Some(t);
            if self.cc64 {
                // the pedal holds the sound; the key itself comes up now
                out.push(Ev::Off { t, pitch });
            }
        }
    }

    /// Everything off (end of the session).
    pub fn finish(&mut self, t: f64, out: &mut Vec<Ev>) {
        self.release_where(t, out, |_| true);
        if self.pedal {
            out.push(Ev::Pedal { t, down: false });
            self.pedal = false;
        }
        self.swallow = [0; 128];
    }
}

/// Offline: replays tracker notes through the Pianist and drops ghosts shorter than MIN_DUR_S.
pub fn perform(notes: &[Note]) -> Vec<Note> {
    let mut evs: Vec<Ev> = Vec::with_capacity(notes.len() * 2);
    for n in notes {
        evs.push(Ev::On { t: n.t, pitch: n.pitch, vel: n.vel });
        evs.push(Ev::Off { t: n.t + n.dur, pitch: n.pitch });
    }
    let key = |e: &Ev| match *e {
        Ev::On { t, .. } => (t, 1),
        Ev::Off { t, .. } => (t, 0),
        Ev::Pedal { t, .. } => (t, 0),
    };
    evs.sort_by(|a, b| key(a).0.total_cmp(&key(b).0).then(key(a).1.cmp(&key(b).1)));
    let mut p = Pianist::new();
    let mut out = Vec::new();
    for e in &evs {
        let now = key(e).0;
        p.push(std::slice::from_ref(e), now, &mut out);
    }
    let end = evs.last().map_or(0.0, |e| key(e).0) + 0.5;
    p.finish(end, &mut out);
    crate::piano::events_to_notes(&crate::piano::apply_pedal(&out)).into_iter().filter(|n| n.dur >= MIN_DUR_S).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(t: f64, dur: f64, pitch: u8) -> Note {
        Note { t, dur, pitch, vel: 90 }
    }

    #[test]
    fn pedal_holds_until_the_bass_moves() {
        // F2 bass, arpeggio C3 A3 each 0.3 s, then a Bb2 bass (a fourth up: a new chord)
        let out = perform(&[n(0.0, 0.3, 41), n(0.3, 0.3, 48), n(0.6, 0.3, 57), n(1.2, 0.3, 46)]);
        let a = out.iter().find(|x| x.pitch == 57).unwrap();
        assert!((a.t + a.dur - 1.2).abs() < 1e-6, "A3 held to the bass change: {a:?}");
        let f = out.iter().find(|x| x.pitch == 41).unwrap();
        assert!((f.t + f.dur - 1.2).abs() < 1e-6, "F2 held to the bass change: {f:?}");
    }

    #[test]
    fn restrike_merge_ghost_drop_and_top_cap() {
        let out = perform(&[n(0.0, 0.5, 60), n(0.05, 0.3, 60), n(1.0, 0.05, 67), n(1.0, 0.5, 100), n(2.0, 0.1, 50), n(2.1, 0.6, 50)]);
        assert_eq!(out.iter().filter(|x| x.pitch == 60).count(), 1);
        // the tracker's own re-strike (off and on at the same instant) merges and still ends
        let d: Vec<_> = out.iter().filter(|x| x.pitch == 50).collect();
        assert!(d.len() == 1 && d[0].dur < 0.7 + HOLD_S + 0.6, "{d:?}");
        assert!(out.iter().all(|x| x.pitch != 67 && x.pitch != 100), "{out:?}");
    }

    #[test]
    fn stepwise_melody_does_not_smear() {
        let out = perform(&[n(0.0, 0.3, 72), n(0.4, 0.3, 74), n(0.8, 0.3, 76)]);
        let c = out.iter().find(|x| x.pitch == 72).unwrap();
        assert!((c.t + c.dur - 0.4).abs() < 1e-6, "{c:?}");
    }
}
