//! Attack-to-note-out latency and the speed stops of the Listen page (#83).
//! Stop 0 is today's processing (a 0.5 s guard before each window's keep slice); each faster stop
//! trades a shorter guard for the same 0.5 s cadence, so a note leaves sooner after its attack.
#![allow(dead_code)]
use std::time::{Duration, Instant};

/// Guard (seconds) before each keep slice, by speed stop. Index = speed; 0 = full processing (today).
pub const GUARD_S: [f64; 4] = [0.50, 0.35, 0.25, 0.15];
/// Length of the keep slice (seconds). Stop 0 keeps [end - 1.0, end - 0.5), as on main.
pub const KEEP_S: f64 = 0.5;

/// The guard for a speed stop; out-of-range stops clamp to the fastest.
pub fn guard(speed: usize) -> f64 {
    GUARD_S[speed.min(GUARD_S.len() - 1)]
}

/// (guard, guard + KEEP_S) in ms: the latency a note can expect at this stop, before model time.
pub fn expected_ms(speed: usize) -> (u32, u32) {
    let g = guard(speed);
    ((g * 1000.0).round() as u32, ((g + KEEP_S) * 1000.0).round() as u32)
}

/// Latency numbers in ms over the last 4096 notes; `n` counts every note.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Summary {
    pub n: u64,
    pub min: f64,
    pub p50: f64,
    pub p95: f64,
    pub max: f64,
}

/// Records, for each note, the wall time from the attack to the note entering the timeline.
/// `anchor` pins one wall instant to one stream time; nothing is recorded before it.
pub struct Lat {
    anchor: Option<(Instant, f64)>,
    ring: Vec<f64>,
    n: u64,
}

impl Lat {
    pub fn new() -> Lat {
        Lat { anchor: None, ring: Vec::new(), n: 0 }
    }

    /// The stream was at `stream_s` at wall time `now`.
    pub fn anchor(&mut self, _now: Instant, _stream_s: f64) {}

    /// A note at stream time `t` was handed over at wall time `now`.
    pub fn note(&mut self, _now: Instant, _t: f64) {}

    pub fn summary(&self) -> Option<Summary> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sep::{FPS, MIN_NOTE_FRAMES};

    #[test]
    fn note_latency_is_wall_time_minus_stream_time() {
        let t0 = Instant::now();
        let mut l = Lat::new();
        l.anchor(t0, 0.0);
        l.note(t0 + Duration::from_secs(1), 0.4);
        let s = l.summary().expect("one note");
        assert_eq!(s.n, 1);
        assert!((s.p50 - 600.0).abs() < 1e-6, "{s:?}");
    }

    #[test]
    fn summary_gives_min_p50_p95_max_and_counts_past_the_cap() {
        let t0 = Instant::now();
        let mut l = Lat::new();
        l.anchor(t0, 0.0);
        // Latencies 1..=5000 ms; the ring keeps the last 4096 (905..=5000 ms).
        for i in 0..5000u64 {
            l.note(t0 + Duration::from_millis(i + 1), 0.0);
        }
        let s = l.summary().expect("notes");
        assert_eq!(s.n, 5000);
        assert!((s.min - 905.0).abs() < 1e-6, "{s:?}");
        assert!((s.p50 - 2953.0).abs() < 1e-6, "{s:?}");
        assert!((s.p95 - 4796.0).abs() < 1e-6, "{s:?}");
        assert!((s.max - 5000.0).abs() < 1e-6, "{s:?}");
    }

    #[test]
    fn nothing_is_recorded_before_the_anchor() {
        let mut l = Lat::new();
        l.note(Instant::now(), 0.4);
        assert!(l.summary().is_none());
    }

    #[test]
    fn stops_run_from_today_to_fastest() {
        assert_eq!(GUARD_S[0], 0.5);
        assert!(GUARD_S.windows(2).all(|w| w[0] > w[1]), "{GUARD_S:?}");
    }

    #[test]
    fn every_stop_leaves_room_for_a_minimum_note() {
        for s in 0..GUARD_S.len() {
            assert!(guard(s) * FPS > (MIN_NOTE_FRAMES + 1) as f64, "stop {s}");
        }
    }
}
