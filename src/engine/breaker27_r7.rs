use super::*;

const TICK: f64 = 1.2;
const AFTER_RESTART: f64 = 1.5;
const N: usize = 59;
fn all(n: usize) -> Vec<String> { (0..n).map(|i| format!("n{i}")).collect() }

/// Permanent ungated `route_perm` + EasyEffects absent 5 s every `period` s starting at
/// `phase`; `strips_die`: the mic strips die whenever the source returns (the live behaviour)
/// and a restart revives them. Returns (restarts per hour over hours 2-3, deferral flips).
fn world(period: f64, phase: f64, strips_die: bool) -> (f64, usize) { world2(period, phase, strips_die, true) }
fn world2(period: f64, phase: f64, strips_die: bool, perm: bool) -> (f64, usize) {
    let base = Instant::now();
    let at = |t: f64| base + Duration::from_secs_f64(t);
    let mut sup = Supervisor::default();
    let (mut started, mut t) = (-60.0f64, 0.0);
    let (mut ev, mut flips, mut was_def) = (vec![], 0usize, false);
    let (mut alive_strips, mut dep_prev) = (true, true);
    while t < 3.0 * 3600.0 {
        let dep = t < 60.0 || ((t - phase).rem_euclid(period)) >= 5.0;
        if dep && !dep_prev && strips_die { alive_strips = false; }
        dep_prev = dep;
        let e = if dep { N } else { N - 2 };
        let m: Vec<String> = if t - started < 1.5 { all(e) } else {
            let mut m = if perm { vec!["route_perm".to_string()] } else { vec![] };
            if dep && !alive_strips { m.push("strip_mic.in".into()); m.push("strip_mic".into()); }
            m
        };
        let why = sup.tick(at(t), true, false, &m, e);
        if sup.deferred_at_cap() != was_def { was_def = !was_def; flips += 1; }
        if why.is_some() {
            if started > -59.0 { ev.push(t); }
            started = t; sup.spawned(at(t));
            if dep { alive_strips = true; }
            t += AFTER_RESTART; continue;
        }
        t += TICK;
    }
    (ev.iter().filter(|x| **x >= 3600.0).count() as f64 / 2.0, flips)
}

fn sweep(strips_die: bool) -> Vec<(f64, f64, f64)> {
    let mut bad = vec![];
    let mut p = 30.0;
    while p <= 240.0 {
        for q in 0..4 {
            let phase = p * q as f64 / 4.0 + 0.37;
            let (r, _) = world(p, phase, strips_die);
            if r > 2.0 { bad.push((p, phase.round(), r)); }
        }
        p += 15.0;
    }
    bad
}

/// R6-F1 regression over periods 30..240 s (step 15) x 4 phases, strips healthy.
#[test]
fn r7_r6_fixed_across_periods_and_phases() {
    let bad = sweep(false);
    assert!(bad.is_empty(), "(period, phase, restarts/h) above 2/h: {bad:?}");
}

/// Variant: same, but the strips die on every return (as ee_toggle.sh shows live), so the
/// missing set alternates between [perm] and [perm, strips]: same_set never matches, and the
/// consecutive counter is zeroed on every return, so the cap give-up is disarmed too.
#[test]
fn r7_permanent_node_plus_dying_strips_flap() {
    let bad = sweep(true);
    // each return legitimately needs one restart for the strips; allow that, but the
    // permanent node must not add a loop of its own on top: bound by returns/h, capped
    assert!(bad.iter().all(|(_, _, r)| *r <= HARD_CAP as f64), "{bad:?}");
    for p in [30.0, 60.0, 120.0, 240.0] {
        eprintln!("period {p}: with perm {:?} / strips only {:?}", world2(p, 0.37, true, true).0, world2(p, 0.37, true, false).0);
    }
}

/// The deferral flag (engine.degraded write/remove + a log line per flip) must not flicker
/// while the cap holds.
#[test]
fn r7_deferral_flag_does_not_flicker() {
    let mut worst = (0usize, 0.0, false);
    for p in [30.0, 60.0, 90.0, 120.0, 240.0] {
        for die in [false, true] {
            let (_, flips) = world(p, 0.37, die);
            if flips > worst.0 { worst = (flips, p, die); }
        }
    }
    assert!(worst.0 <= 60, "deferred_at_cap flipped {} times in 3 h (period {}, strips_die {}): one log line + engine.degraded write/remove each", worst.0, worst.1, worst.2);
}
