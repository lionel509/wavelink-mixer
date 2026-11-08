//! Auto-normalize: moves a channel's gain slider so its short-term loudness sits at TARGET_DB.
//! Fed by the pre-fader RMS the IN meter tap already computes (no extra tap, no audio-path change).

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
