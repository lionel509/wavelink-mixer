//! Auto-normalize: moves a channel's gain slider so its short-term loudness sits at TARGET_DB.
//! Fed by the pre-fader RMS the IN meter tap already computes (no extra tap, no audio-path change).

pub const TARGET_DB: f64 = -20.0;   // short-term RMS dBFS the sliders aim for
pub const GATE_DB: f64 = -50.0;     // quieter than this is silence: hold, never pump noise up
pub const MAX_BOOST_DB: f64 = 12.0; // auto's own gain range, before his offset
pub const MAX_CUT_DB: f64 = -24.0;
pub const WINDOW_S: f64 = 3.0;      // loudness window (power-domain EMA), like short-term LUFS
pub const ATTACK_DB_S: f64 = 4.0;   // too loud: slider falls at most this fast
pub const RELEASE_DB_S: f64 = 1.5;  // too quiet: slider rises at most this fast
pub const DEADBAND_DB: f64 = 1.0;   // within this of the wanted gain: don't move
pub const STEP_DB: f64 = 0.5;       // emit only when the slider would move at least this much
pub const MIN_EMIT_S: f64 = 0.25;   // at most 4 slider writes per second per channel
pub const MANUAL_HOLD_S: f64 = 3.0; // after his own move, auto waits this long

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Channel {
    pub gain: f64,          // slider dB as auto believes it is
    pub offset: f64,        // his preference relative to the target, set by manual()
    pub loud: Option<f64>,  // smoothed power (linear); None until the first non-silent reading
    pub hold: f64,          // seconds of manual hold left
    pub since_emit: f64,
    pub emitted: f64,       // last value handed to the slider
}

impl Channel {
    pub fn new(gain_db: f64) -> Channel {
        Channel { gain: gain_db, offset: 0.0, loud: None, hold: 0.0, since_emit: MIN_EMIT_S, emitted: gain_db }
    }

    pub fn loud_db(&self) -> Option<f64> {
        self.loud.map(|p| 10.0 * p.log10())
    }

    /// He (or MIDI) moved the slider: his position wins and becomes the preference; auto pauses MANUAL_HOLD_S.
    pub fn manual(&mut self, gain_db: f64) {
        self.gain = gain_db;
        self.emitted = gain_db;
        self.hold = MANUAL_HOLD_S;
        if let Some(l) = self.loud_db() {
            self.offset = gain_db - (TARGET_DB - l).clamp(MAX_CUT_DB, MAX_BOOST_DB);
        }
    }

    /// One tick with the channel's pre-fader RMS (dBFS). Some(db) = move the slider there.
    pub fn step(&mut self, rms_db: f64, dt: f64) -> Option<f64> {
        self.since_emit += dt;
        if self.hold > 0.0 {
            self.hold -= dt;
            return None;
        }
        if !rms_db.is_finite() || rms_db < GATE_DB {
            return None;
        }
        let p = 10f64.powf(rms_db / 10.0);
        let a = 1.0 - (-dt / WINDOW_S).exp();
        let l = match self.loud {
            Some(l) => l + (p - l) * a,
            None => p,
        };
        self.loud = Some(l);
        let want = (TARGET_DB - 10.0 * l.log10()).clamp(MAX_CUT_DB, MAX_BOOST_DB) + self.offset;
        let diff = want - self.gain;
        if diff.abs() < DEADBAND_DB {
            return None;
        }
        let rate = if diff < 0.0 { ATTACK_DB_S } else { RELEASE_DB_S };
        self.gain += diff.signum() * (rate * dt).min(diff.abs());
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
        let rate = ATTACK_DB_S.max(RELEASE_DB_S);
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
                assert!((c.gain - before).abs() <= rate * dt + 1e-9, "case {case}: moved {}", c.gain - before);
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
}
