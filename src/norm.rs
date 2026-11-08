//! Auto-normalize: moves a channel's gain slider so its short-term loudness sits at TARGET_DB.
//! Fed by the pre-fader RMS the IN meter tap already computes (no extra tap, no audio-path change).
//! His own slider move always wins; a fast burst detector drops auto's boost above his position at once.

pub const TARGET_DB: f64 = -20.0;   // short-term RMS dBFS the sliders aim for
pub const GATE_DB: f64 = -50.0;     // quieter than this is silence: hold, never pump noise up
pub const MAX_BOOST_DB: f64 = 12.0; // auto's own gain range, above his position
pub const MAX_CUT_DB: f64 = -24.0;
pub const WINDOW_S: f64 = 3.0;      // loudness window (power-domain EMA), like short-term LUFS
pub const ATTACK_DB_S: f64 = 4.0;   // too loud: slider falls at most this fast
pub const RELEASE_DB_S: f64 = 1.5;  // too quiet: slider rises at most this fast
pub const DEADBAND_DB: f64 = 1.0;   // within this of the wanted gain: don't move
pub const STEP_DB: f64 = 0.5;       // emit only when the slider would move at least this much
pub const MIN_EMIT_S: f64 = 0.25;   // at most 4 slider writes per second per channel
pub const MANUAL_HOLD_S: f64 = 3.0; // after his own move, auto waits this long
pub const FADER_MIN_DB: f64 = -60.0; // the fader's range (dash.rs taper ends); auto never asks past it
pub const FADER_MAX_DB: f64 = 12.0;
pub const SETTLE_S: f64 = 3.0;       // audio heard since the last reset before a move made without it is anchored
pub const RESET_S: f64 = 3.0;        // this much continuous silence forgets the loudness (the next song is measured fresh)
pub const FAST_WINDOW_S: f64 = 0.25; // burst detector window (power EMA)
pub const FAST_OVER_DB: f64 = 9.0;   // short-term output this far over `out` removes auto's boost at once

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Channel {
    pub gain: f64,          // slider dB as auto believes it is
    pub base: f64,          // his slider position; 0.0 until his first move
    pub out: f64,           // wanted output loudness, dBFS (loudness + gain); TARGET_DB until anchored
    pub pending: bool,      // his move awaits SETTLE_S of audio before it can be anchored
    pub loud: Option<f64>,  // slow smoothed power (linear); None when nothing was heard since the last reset
    pub acc: f64,           // slow EMA numerator (power)
    pub wsum: f64,          // slow EMA weight (bias correction)
    pub facc: f64,          // fast EMA numerator (power)
    pub fwsum: f64,         // fast EMA weight
    pub heard: f64,         // seconds above the gate since the last reset
    pub quiet: f64,         // seconds of continuous silence
    pub hold: f64,          // seconds of manual hold left
    pub since_emit: f64,
    pub emitted: f64,       // last value handed to the slider
}

impl Channel {
    pub fn new(gain_db: f64) -> Channel {
        Channel {
            gain: gain_db, base: 0.0, out: TARGET_DB, pending: false, loud: None,
            acc: 0.0, wsum: 0.0, facc: 0.0, fwsum: 0.0, heard: 0.0, quiet: 0.0,
            hold: 0.0, since_emit: MIN_EMIT_S, emitted: gain_db,
        }
    }

    pub fn loud_db(&self) -> Option<f64> {
        self.loud.map(|p| 10.0 * p.log10())
    }

    /// His move wins: the slider stays where he put it; it becomes his position (`base`) and, once measured against settled audio, his wanted output level (`out`).
    pub fn manual(&mut self, gain_db: f64) {
        if !gain_db.is_finite() {
            return;
        }
        let g = gain_db.clamp(FADER_MIN_DB, FADER_MAX_DB);
        self.gain = g;
        self.emitted = g;
        self.base = g;
        self.hold = MANUAL_HOLD_S;
        match self.loud_db() {
            Some(l) if self.heard >= SETTLE_S && self.quiet == 0.0 => {
                self.out = l + g;
                self.pending = false;
            }
            _ => self.pending = true,
        }
    }

    /// One tick with the channel's pre-fader RMS (dBFS). Some(db) = move the slider there.
    pub fn step(&mut self, rms_db: f64, dt: f64) -> Option<f64> {
        self.since_emit += dt;
        let audible = rms_db.is_finite() && rms_db >= GATE_DB;
        if audible {
            let p = 10f64.powf(rms_db / 10.0);
            let a = 1.0 - (-dt / WINDOW_S).exp();
            self.acc += (p - self.acc) * a;
            self.wsum += (1.0 - self.wsum) * a;
            self.loud = Some(self.acc / self.wsum);
            let af = 1.0 - (-dt / FAST_WINDOW_S).exp();
            self.facc += (p - self.facc) * af;
            self.fwsum += (1.0 - self.fwsum) * af;
            self.heard += dt;
            self.quiet = 0.0;
            if self.pending && self.heard >= SETTLE_S {
                self.out = 10.0 * (self.acc / self.wsum).log10() + self.base;
                self.pending = false;
            }
        } else {
            self.quiet += dt;
            if self.quiet >= RESET_S {
                self.loud = None;
                self.acc = 0.0;
                self.wsum = 0.0;
                self.facc = 0.0;
                self.fwsum = 0.0;
                self.heard = 0.0;
            }
        }
        if self.hold > 0.0 {
            self.hold -= dt;
            return None;
        }
        if !audible || self.pending {
            return None;
        }
        let l = 10.0 * (self.acc / self.wsum).log10();
        let f = 10.0 * (self.facc / self.fwsum).log10();
        if self.gain > self.base && self.gain + f > self.out + FAST_OVER_DB {
            // burst: drop auto's boost now, never below his position
            self.gain = (self.out + FAST_OVER_DB - f).max(self.base);
        } else {
            let want = (self.out - l)
                .clamp(self.base + MAX_CUT_DB, self.base + MAX_BOOST_DB)
                .clamp(FADER_MIN_DB, FADER_MAX_DB);
            let diff = want - self.gain;
            if diff.abs() < DEADBAND_DB {
                return None;
            }
            // never raise while short-term output is at or over his level
            if diff > 0.0 && self.gain + f >= self.out {
                return None;
            }
            let rate = if diff < 0.0 { ATTACK_DB_S } else { RELEASE_DB_S };
            self.gain += diff.signum() * (rate * dt).min(diff.abs());
        }
        if self.since_emit >= MIN_EMIT_S && (self.gain - self.emitted).abs() >= STEP_DB {
            self.since_emit = 0.0;
            self.emitted = (self.gain * 10.0).round() / 10.0;
            Some(self.emitted)
        } else {
            None
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic uniform [0, 1) from an LCG (no `rand` crate).
    fn unit(x: &mut u64) -> f64 {
        *x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (*x >> 11) as f64 / (1u64 << 53) as f64
    }

    #[test]
    fn silence_never_moves() {
        let mut c = Channel::new(0.0);
        for _ in 0..2000 {
            assert_eq!(c.step(-80.0, 0.03), None);
        }
        assert_eq!(c.gain, 0.0);
        assert!(c.loud.is_none());
    }

    #[test]
    fn loud_source_is_cut_at_attack_rate() {
        let mut c = Channel::new(0.0);
        let dt: f64 = 0.03;
        let mut t = 0.0;
        let mut first_some: Option<f64> = None;
        for _ in 0..(20.0 / dt as f64).round() as usize {
            let before = c.gain;
            let r = c.step(-8.0, dt);
            t += dt;
            if r.is_some() && first_some.is_none() {
                first_some = Some(t);
            }
            assert!((c.gain - before).abs() <= ATTACK_DB_S * dt + 1e-9, "step moved {}", c.gain - before);
        }
        assert!(first_some.expect("never emitted") <= 1.0);
        assert!((c.gain - -12.0).abs() <= DEADBAND_DB, "gain {}", c.gain);
        assert!((c.loud_db().unwrap() - -8.0).abs() < 0.05);
    }

    #[test]
    fn quiet_source_rises_slowly_up_to_max_boost() {
        let mut c = Channel::new(0.0);
        let dt: f64 = 0.03;
        let mut t = 0.0;
        let mut first11: Option<f64> = None;
        for _ in 0..1000 {
            let before = c.gain;
            c.step(-45.0, dt);
            t += dt;
            assert!((c.gain - before) <= RELEASE_DB_S * dt + 1e-9, "rose {}", c.gain - before);
            if c.gain >= 11.0 && first11.is_none() {
                first11 = Some(t);
            }
        }
        assert!(first11.expect("never reached 11 dB") >= 7.0);
        assert!((c.gain - MAX_BOOST_DB).abs() <= DEADBAND_DB, "gain {}", c.gain);
    }

    #[test]
    fn deadband_stops_hunting() {
        let mut c = Channel::new(0.0);
        for _ in 0..667 {
            c.step(-8.0, 0.03);
        }
        for _ in 0..667 {
            assert_eq!(c.step(-8.0, 0.03), None);
        }
    }

    #[test]
    fn manual_move_wins_and_becomes_preference() {
        let mut c = Channel::new(0.0);
        for _ in 0..667 {
            c.step(-8.0, 0.03);
        }
        c.manual(-18.0);
        for _ in 0..100 {
            assert_eq!(c.step(-8.0, 0.03), None);
        }
        assert_eq!(c.gain, -18.0);
        for _ in 0..667 {
            assert_eq!(c.step(-8.0, 0.03), None);
        }
        assert!((c.gain - -18.0).abs() < DEADBAND_DB, "gain {}", c.gain);
        for _ in 0..667 {
            c.step(-4.0, 0.03);
        }
        assert!((c.gain - -22.0).abs() <= DEADBAND_DB, "gain {}", c.gain);
    }

    #[test]
    fn emits_are_rate_limited_and_rounded() {
        let mut c = Channel::new(0.0);
        let dt: f64 = 0.01;
        let mut t = 0.0;
        let mut emits: Vec<(f64, f64)> = vec![];
        for _ in 0..1000 {
            t += dt;
            if let Some(v) = c.step(-8.0, dt) {
                emits.push((t, v));
            }
        }
        assert!(emits.len() >= 3, "only {} emits", emits.len());
        for w in emits.windows(2) {
            assert!(w[1].0 - w[0].0 >= MIN_EMIT_S - 1e-9, "emits {:.3} and {:.3} too close", w[0].0, w[1].0);
            assert!((w[1].1 - w[0].1).abs() >= 0.4, "steps {} and {}", w[0].1, w[1].1);
        }
        for (_, v) in &emits {
            assert!((v * 10.0 - (v * 10.0).round()).abs() < 1e-9, "not rounded to 0.1: {v}");
        }
    }

    #[test]
    fn normalizer_bounded_under_random_feed() {
        let mut x: u64 = 11;
        for case in 0..200 {
            let mut c = Channel::new(0.0);
            let mut hold_left = 0.0_f64;
            let mut t = 0.0_f64;
            let mut last_emit: Option<f64> = None;
            for _ in 0..2000 {
                let rms = -90.0 + 90.0 * unit(&mut x);
                let dt = 1.0 / 40.0 + (1.0 / 27.0 - 1.0 / 40.0) * unit(&mut x);
                if unit(&mut x) < 1.0 / 200.0 {
                    let g = -60.0 + 72.0 * unit(&mut x);
                    c.manual(g);
                    hold_left = MANUAL_HOLD_S;
                }
                let before = c.gain;
                let r = c.step(rms, dt);
                t += dt;
                assert!(c.gain - before <= RELEASE_DB_S * dt + 1e-9, "case {case}: rose {}", c.gain - before);
                if before - c.gain > ATTACK_DB_S * dt + 1e-9 {
                    assert!(c.gain >= c.base - 1e-9, "case {case}: fast cut ended at {} below his position {}", c.gain, c.base);
                }
                if rms < GATE_DB {
                    assert!(c.gain == before && r.is_none(), "case {case}: moved below the gate");
                }
                if hold_left > 0.0 {
                    assert!(r.is_none(), "case {case}: moved during a manual hold");
                    hold_left -= dt;
                }
                if r.is_some() {
                    if let Some(prev) = last_emit {
                        assert!(t - prev >= MIN_EMIT_S - 1e-9, "case {case}: emits {:.3} apart", t - prev);
                    }
                    last_emit = Some(t);
                }
            }
        }
    }

    /// Breaker C feed helper: run `secs` of constant `rms` at step `dt`, collecting slider writes.
    fn feed(c: &mut Channel, rms: f64, secs: f64, dt: f64) -> Vec<f64> {
        let n = (secs / dt).round() as usize;
        let mut out = vec![];
        for _ in 0..n {
            if let Some(v) = c.step(rms, dt) {
                out.push(v);
            }
        }
        out
    }

    #[test]
    fn manual_before_first_reading_is_kept() {
        let mut c = Channel::new(0.0);
        c.manual(-18.0);
        feed(&mut c, -8.0, 20.0, 0.03);
        assert!((c.gain - -18.0).abs() <= DEADBAND_DB, "auto snapped the slider back to {:.2} dB; his move was -18.0 dB", c.gain);
    }

    #[test]
    fn offset_never_asks_past_fader_top() {
        let mut c = Channel::new(0.0);
        feed(&mut c, -8.0, 20.0, 0.03);
        c.manual(12.0);
        let mut emits = feed(&mut c, -8.0, 10.0, 0.03);
        emits.extend(feed(&mut c, -45.0, 30.0, 0.03));
        let max = emits.iter().cloned().fold(f64::MIN, f64::max);
        assert!(max <= 12.0 + 1e-9, "auto asked the slider for {max:.2} dB; the fader tops out at +12");
        assert!(c.gain <= 12.0 + 1e-9, "internal gain {:.2} dB is past the fader top", c.gain);
    }

    #[test]
    fn offset_never_asks_past_fader_floor() {
        let mut c = Channel::new(0.0);
        feed(&mut c, -40.0, 20.0, 0.03);
        c.manual(-60.0);
        let emits = feed(&mut c, -8.0, 30.0, 0.03);
        let min = emits.iter().cloned().fold(f64::MAX, f64::min);
        assert!(min >= -60.0 - 1e-9, "auto asked the slider for {min:.2} dB; the fader bottoms out at -60");
        assert!(c.gain >= -60.0 - 1e-9, "internal gain {:.2} dB is past the fader floor", c.gain);
    }

    #[test]
    fn fader_range_matches_taper() {
        assert_eq!(FADER_MIN_DB, crate::dash::pos_to_db(0.0));
        assert_eq!(FADER_MAX_DB, crate::dash::pos_to_db(1.0));
    }

    #[test]
    fn xb1_fade_in_before_first_play_keeps_his_move() {
        let mut c = Channel::new(0.0);
        c.manual(-18.0);
        let mut w = feed(&mut c, -80.0, 4.0, 0.03);
        w.extend(feed(&mut c, -49.0, 0.03, 0.03));
        w.extend(feed(&mut c, -8.0, 20.0, 0.03));
        assert!(w.is_empty(), "auto wrote {w:?} after his move");
        assert_eq!(c.gain, -18.0);
        assert!(!c.pending);
    }

    #[test]
    fn xb1_click_before_first_play_keeps_his_move() {
        let mut c = Channel::new(0.0);
        c.manual(-18.0);
        let mut w = feed(&mut c, -80.0, 4.0, 0.03);
        w.extend(feed(&mut c, -2.0, 0.03, 0.03));
        w.extend(feed(&mut c, -8.0, 20.0, 0.03));
        assert!(w.is_empty(), "auto wrote {w:?} after his move");
        assert_eq!(c.gain, -18.0);
        assert!((c.out - -25.93).abs() < 0.1, "out {:.2}", c.out);
    }

    #[test]
    fn xb2_move_in_silence_after_play_keeps_his_move() {
        let mut c = Channel::new(0.0);
        feed(&mut c, -8.0, 20.0, 0.03);
        feed(&mut c, -80.0, 60.0, 0.03);
        c.manual(-18.0);
        let w = feed(&mut c, -80.0, 4.0, 0.03);
        let w2 = feed(&mut c, -30.0, 20.0, 0.03);
        assert!(w.is_empty() && w2.is_empty(), "auto wrote {w:?} {w2:?} after his move");
        assert_eq!(c.gain, -18.0);
        feed(&mut c, -10.0, 20.0, 0.03);
        assert!((c.gain - -38.0).abs() <= 1.0, "gain {:.2}", c.gain);
    }

    #[test]
    fn move_right_after_resume_keeps_his_move() {
        let mut c = Channel::new(0.0);
        feed(&mut c, -8.0, 20.0, 0.03);
        feed(&mut c, -80.0, 60.0, 0.03);
        feed(&mut c, -30.0, 1.0, 0.03);
        c.manual(-18.0);
        let w = feed(&mut c, -30.0, 20.0, 0.03);
        assert!(w.is_empty(), "auto wrote {w:?} after his move");
        assert_eq!(c.gain, -18.0);
    }

    #[test]
    fn move_in_short_gap_uses_settled_loudness() {
        let mut c = Channel::new(0.0);
        feed(&mut c, -8.0, 20.0, 0.03);
        feed(&mut c, -80.0, 1.0, 0.03);
        c.manual(-18.0);
        assert!(c.pending);
        let w = feed(&mut c, -8.0, 20.0, 0.03);
        assert!(w.is_empty(), "auto wrote {w:?} after his move");
        assert_eq!(c.gain, -18.0);
        feed(&mut c, -4.0, 20.0, 0.03);
        assert!((c.gain - -22.0).abs() <= 1.0, "gain {:.2}", c.gain);
    }

    #[test]
    fn burst_after_quiet_ambience_is_cut_fast() {
        let dt: f64 = 0.03;
        let mut c = Channel::new(0.0);
        feed(&mut c, -40.0, 30.0, dt);
        assert!((c.gain - 12.0).abs() <= 1.0, "ambience gain {:.2}", c.gain);
        let mut t = 0.0;
        for step in 0..200 {
            t += dt;
            let r = c.step(-6.0, dt);
            if step == 0 {
                assert!(r.is_some(), "first burst step did not write");
            }
            if t <= 0.3 {
                assert!(c.emitted - 6.0 <= 0.0, "t={t:.2}: slider at {:.2}", c.emitted);
            } else {
                assert!(c.emitted - 6.0 <= -5.0, "t={t:.2}: slider at {:.2}", c.emitted);
            }
        }
        assert!(c.emitted <= -8.0, "slider at {:.2} after 6 s of burst", c.emitted);
    }

    #[test]
    fn measurement_runs_during_hold() {
        let mut c = Channel::new(0.0);
        feed(&mut c, -8.0, 20.0, 0.03);
        c.manual(-18.0);
        let mut w = feed(&mut c, -8.0, 0.5, 0.03);
        w.extend(feed(&mut c, -4.0, 2.4, 0.03));
        assert!(w.is_empty(), "auto wrote {w:?} inside the hold");
        assert!(c.loud_db().unwrap() > -6.0, "loud {:?}", c.loud_db());
        feed(&mut c, -4.0, 1.5, 0.03);
        assert!(c.gain <= -19.5, "gain {:.2}", c.gain);
    }

    #[test]
    fn manual_ignores_non_finite() {
        let mut c = Channel::new(0.0);
        feed(&mut c, -8.0, 20.0, 0.03);
        let before = c;
        c.manual(f64::NAN);
        c.manual(f64::INFINITY);
        c.manual(f64::NEG_INFINITY);
        assert_eq!(c, before);
        feed(&mut c, -4.0, 20.0, 0.03);
        assert!((c.gain - -16.0).abs() <= 1.1, "gain {:.2}", c.gain);
    }

    #[test]
    fn boost_capped_above_his_position() {
        let mut c = Channel::new(0.0);
        feed(&mut c, -8.0, 20.0, 0.03);
        c.manual(-18.0);
        let mut w = feed(&mut c, -8.0, 5.0, 0.03);
        w.extend(feed(&mut c, -80.0, 60.0, 0.03));
        w.extend(feed(&mut c, -45.0, 30.0, 0.03));
        let max = w.iter().cloned().fold(f64::MIN, f64::max);
        assert!(max <= -6.0 + 0.05, "auto asked for {max:.2} dB; his position is -18, so the cap is -6");
        assert!((c.gain - -6.0).abs() <= 1.0, "gain {:.2}", c.gain);
    }

    #[test]
    fn silence_resets_loudness_memory() {
        let mut c = Channel::new(0.0);
        feed(&mut c, -8.0, 20.0, 0.03);
        feed(&mut c, -80.0, 2.9, 0.03);
        assert!(c.loud_db().is_some());
        feed(&mut c, -80.0, 0.2, 0.03);
        assert!(c.loud_db().is_none());
        feed(&mut c, -80.0, 56.9, 0.03);
        feed(&mut c, -30.0, 2.0, 0.03);
        assert!((c.loud_db().unwrap() - -30.0).abs() <= 0.5, "loud {:?}", c.loud_db());
        feed(&mut c, -30.0, 18.0, 0.03);
        assert!((c.gain - 10.0).abs() <= 1.1, "gain {:.2}", c.gain);
    }

    #[test]
    fn manual_move_wins_under_segment_feed() {
        let mut x: u64 = 7;
        let mut fast_cuts = 0usize;
        let mut pending_steps = 0usize;
        for case in 0..300 {
            let mut c = Channel::new(0.0);
            let mut last_manual: Option<f64> = None;
            for seg in 0..40 {
                let silent = unit(&mut x) < 0.3;
                let level = if silent { -80.0 } else { -60.0 + 60.0 * unit(&mut x) };
                let dur = 0.2 + 14.8 * unit(&mut x);
                if silent && unit(&mut x) < 0.3 {
                    let g = -60.0 + 72.0 * unit(&mut x);
                    c.manual(g);
                    last_manual = Some(g);
                }
                let mut used = 0.0;
                while used < dur {
                    if unit(&mut x) < 1.0 / 400.0 {
                        let g = -60.0 + 72.0 * unit(&mut x);
                        c.manual(g);
                        last_manual = Some(g);
                    }
                    let dt = 1.0 / 40.0 + (1.0 / 27.0 - 1.0 / 40.0) * unit(&mut x);
                    used += dt;
                    let pend = c.pending;
                    let before = c.gain;
                    let r = c.step(level, dt);
                    let cap = last_manual.unwrap_or(0.0) + MAX_BOOST_DB;
                    if pend {
                        pending_steps += 1;
                        assert!(r.is_none() && c.gain == before, "case {case} seg {seg}: moved while his move was pending");
                    }
                    assert!(c.gain <= cap + 1e-9, "case {case} seg {seg}: gain {:.2} above his position + 12 = {cap:.2}", c.gain);
                    if let Some(v) = r {
                        assert!(v <= cap + 0.05 + 1e-9, "case {case} seg {seg}: asked {v:.2}, cap {cap:.2}");
                    }
                    assert!(c.gain - before <= RELEASE_DB_S * dt + 1e-9, "case {case} seg {seg}: rose {}", c.gain - before);
                    if before - c.gain > ATTACK_DB_S * dt + 1e-9 {
                        fast_cuts += 1;
                        assert!(c.gain >= c.base - 1e-9, "case {case} seg {seg}: fast cut ended at {:.2}, below his position {:.2}", c.gain, c.base);
                    }
                    assert!(c.gain.is_finite(), "case {case} seg {seg}: gain not finite");
                }
            }
        }
        assert!(fast_cuts > 0, "no fast cut happened, so the property is vacuous");
        assert!(pending_steps > 0, "no pending step happened, so the property is vacuous");
    }
}
