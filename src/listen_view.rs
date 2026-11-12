//! The pure parts of the Listen page (#83): lane geometry, the latency text and the speed slider.
//! No GTK here, so the tests run headless.
#![allow(dead_code)]
use crate::notes::Note;
use crate::lat::Summary;

/// Seconds of notes on the page at once.
pub const SPAN_S: f64 = 8.0;

/// The rectangles (x, y, w, h) of one lane's notes at a page of `w` x `h`. Lane `i` is the `i`-th third of the
/// height; time runs left to right, `now` at the right edge.
pub fn lane_rects(_notes: &[Note], _i: usize, _now: f64, _w: f64, _h: f64) -> Vec<(f64, f64, f64, f64)> {
    Vec::new()
}

/// The speed stop for a slider value: the left end (0) is the fastest stop (3), the right end (3) is today's.
pub fn speed_of_slider(_v: f64) -> usize {
    0
}

/// The latency line under the slider: the measured numbers at this stop, or what to expect before any note.
pub fn lat_text(_speed: usize, _s: Option<Summary>) -> String {
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcg(s: &mut u64) -> f64 {
        *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((*s >> 33) as f64) / (1u64 << 31) as f64
    }

    fn note(t: f64, dur: f64, pitch: u8) -> Note {
        Note { t, dur, pitch, vel: 90 }
    }

    #[test]
    fn lane_rects_stay_inside_their_lane_at_any_size() {
        let now = 20.0;
        let mut s = 5u64;
        let notes: Vec<Note> = (0..300)
            .map(|_| {
                let t = now - SPAN_S - 1.0 + lcg(&mut s) * (SPAN_S + 2.0);
                let pitch = if lcg(&mut s) < 0.2 { 0 } else { 21 + (lcg(&mut s) * 88.0) as u8 };
                note(t, 0.05 + lcg(&mut s) * 2.0, pitch)
            })
            .collect();
        for (w, h) in [(200.0, 120.0), (700.0, 300.0), (1100.0, 500.0), (2400.0, 1200.0)] {
            for i in 0..3 {
                let want = notes.iter().filter(|n| n.t + n.dur >= now - SPAN_S && n.t <= now).count();
                let r = lane_rects(&notes, i, now, w, h);
                assert_eq!(r.len(), want, "{w}x{h} lane {i}");
                let (top, bot) = (i as f64 * h / 3.0, (i + 1) as f64 * h / 3.0);
                for (x, y, rw, rh) in r {
                    assert!(x >= 0.0 && x + rw <= w + 1e-9, "{w}x{h}: x {x} w {rw}");
                    assert!(y >= top - 1e-9 && y + rh <= bot + 1e-9, "{w}x{h} lane {i}: y {y} h {rh}");
                }
            }
        }
    }

    #[test]
    fn lane_rects_scale_with_the_width() {
        let now = 20.0;
        let n = note(now - SPAN_S / 2.0, 0.1, 60);
        for w in [200.0, 700.0, 1100.0, 2400.0] {
            let r = lane_rects(std::slice::from_ref(&n), 1, now, w, 300.0);
            assert_eq!(r.len(), 1, "{w}");
            assert!((r[0].0 - w / 2.0).abs() <= 1.0, "{w}: x {}", r[0].0);
        }
    }

    #[test]
    fn lat_text_shows_measured_or_expected() {
        let s = Summary { n: 41, min: 212.0, p50: 431.0, p95: 655.0, max: 702.0 };
        assert_eq!(lat_text(3, Some(s)), "latency 431 ms median · 655 ms p95 · 41 notes");
        let none = lat_text(3, None);
        assert!(none.contains("not measured") && none.contains("expect 150–650 ms"), "{none}");
    }

    #[test]
    fn slider_maps_left_to_fastest() {
        assert_eq!(speed_of_slider(0.0), 3);
        assert_eq!(speed_of_slider(1.0), 2);
        assert_eq!(speed_of_slider(2.0), 1);
        assert_eq!(speed_of_slider(3.0), 0);
    }
}
