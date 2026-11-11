use super::*;

const TICK: f64 = 1.2;
const AFTER_RESTART: f64 = 1.5;
const N: usize = 59;
fn all(n: usize) -> Vec<String> { (0..n).map(|i| format!("n{i}")).collect() }
fn strips() -> Vec<String> { vec!["strip_mic.in".into(), "strip_mic".into()] }

/// `state(t, since_spawn, just_restarted) -> (missing, eligible)`; returns (restarts, degraded ever, sup)
fn drive(total: f64, mut state: impl FnMut(f64, f64, bool) -> (Vec<String>, usize)) -> (Vec<f64>, bool, Supervisor) {
    let base = Instant::now();
    let at = |t: f64| base + Duration::from_secs_f64(t);
    let mut sup = Supervisor::default();
    let (mut started, mut t, mut just) = (-60.0f64, 0.0, false);
    let (mut ev, mut deg) = (vec![], false);
    while t < total {
        let (m, e) = state(t, t - started, just);
        just = false;
        let why = sup.tick(at(t), true, false, &m, e);
        deg |= sup.degraded().is_some();
        if why.is_some() {
            if started > -59.0 { ev.push(t); }
            started = t; sup.spawned(at(t)); just = true;
            t += AFTER_RESTART; continue;
        }
        t += TICK;
    }
    (ev, deg, sup)
}

/// One permanently missing UNGATED route plus EasyEffects restarting every `period` s (absent
/// 5 s; strips healthy, they are not the problem). Every return resets same_set and
/// stuck_restarts, so neither give-up rule can fire.
fn perm_with_flap(period: f64) -> (usize, bool) {
    let (ev, deg, _) = drive(3.0 * 3600.0, |t, since, _| {
        let dep = (t % period) >= 5.0 || t < period;
        let e = if dep { N } else { N - 2 };
        if since < 1.5 { return (all(e), e); }
        (vec!["route_spotify_b1".into()], e)
    });
    (ev.iter().filter(|t| **t >= 3600.0).count() / 2, deg)
}

// ---------------- red ----------------

/// R6-F1: a flapping dependency launders a permanent, ungated node out of the give-up rules:
/// `eligible > last_eligible` zeroes same_set and stuck_restarts on every return, so
/// `same_set >= 3` never accumulates and the cap give-up (`stuck_restarts > 0`) never holds.
/// Steady state (hours 2-3): restarts at the penalty/hard-cap pace forever, never degraded.
/// Without the flap the same node degrades after 3 restarts and is retried once an hour.
#[test]
fn r6_flapping_dependency_keeps_permanent_node_out_of_degraded() {
    let (without, deg0) = perm_with_flap(1e9);
    assert!(deg0 && without <= 1, "baseline: no flap -> degraded, ~1 retry/h ({without}/h)");
    // measured: period 60 s -> 6/h, 120 s -> 6/h, >= 300 s -> 1/h (fine)
    let (with, deg) = perm_with_flap(120.0);
    assert!(deg && with <= 2, "EasyEffects restarting every 2 min: {with} full-graph restarts/h for a node no restart fixes (degraded ever: {deg}); baseline {without}/h");
}

// ---------------- probes ----------------

/// Dependency away for exactly DEPENDENCY_AWAY (decay fires) then back with the strips dead:
/// they are retried promptly (the decay does not strand them in degraded / without a clock).
#[test]
fn probe_dependency_away_exactly_5min_then_back() {
    for away in [295.0, 300.0, 301.2, 305.0] {
        let mut alive = true;
        let mut was = true;
        let ret = 600.0 + away;
        let (ev, _, _) = drive(1500.0, |t, since, just| {
            let dep = !(600.0..ret).contains(&t);
            if dep && !was { alive = false; }
            was = dep;
            if just && dep { alive = true; }
            let e = if dep { N } else { N - 2 };
            if since < 1.5 { return (all(e), e); }
            (if dep && !alive { strips() } else { vec![] }, e)
        });
        let fix = ev.iter().find(|t| **t >= ret).map(|t| t - ret).unwrap_or(f64::INFINITY);
        assert!(fix <= 15.0, "away {away}s: strips dead after return, restart after {fix:.0}s");
    }
}

/// A strip that no restart fixes, EasyEffects restarting every 3 min: laundering gives it
/// attempts after every return, but the hard cap bounds it.
#[test]
fn probe_broken_gated_strip_bounded_by_cap() {
    let (ev, _, _) = drive(3.0 * 3600.0, |t, since, _| {
        let dep = (t % 180.0) >= 5.0 || t < 180.0;
        let e = if dep { N } else { N - 2 };
        if since < 1.5 { return (all(e), e); }
        (if dep { strips() } else { vec![] }, e)
    });
    let per_h = ev.iter().filter(|t| **t >= 3600.0).count() / 2;
    eprintln!("broken gated strip + 3 min flap: {per_h} restarts/h");
    assert!(per_h <= HARD_CAP);
}
