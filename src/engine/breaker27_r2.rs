use super::*;

const TICK: f64 = 1.2;
const AFTER_RESTART: f64 = 1.5;

#[derive(Debug)]
struct Ev { t: f64, why: Why }

/// `missing(t, since_spawn, restarts_so_far) -> names`. Returns (restarts, degraded-entry times).
fn run2(total_expected: usize, total: f64, mut missing: impl FnMut(f64, f64, usize) -> Vec<String>) -> (Vec<Ev>, Vec<f64>) {
    let base = Instant::now();
    let at = |t: f64| base + Duration::from_secs_f64(t);
    let mut sup = Supervisor::default();
    let mut started = -60.0f64;
    let (mut ev, mut deg) = (vec![], vec![]);
    let mut was = false;
    let mut t = 0.0;
    while t < total {
        let m = missing(t, t - started, ev.len());
        let why = sup.tick(at(t), true, false, &m, total_expected);
        match (sup.degraded().is_some(), was) { (true, false) => { deg.push(t); was = true } (false, true) => was = false, _ => {} }
        if let Some(why) = why {
            if started > -59.0 { ev.push(Ev { t, why }); }
            started = t;
            sup.spawned(at(t));
            t += AFTER_RESTART;
            continue;
        }
        t += TICK;
    }
    (ev, deg)
}
fn s(v: &[&str]) -> Vec<String> { v.iter().map(|x| x.to_string()).collect() }

// ---------------- red ----------------

/// R2-F1: once degraded (one node a restart cannot fix), the engine ignores every NEW,
/// restart-fixable loss for up to an hour, e.g. the mic strip dying later. Only the original
/// set was given up on; a different set should still get the normal policy.
#[test]
fn r2_degraded_ignores_new_fixable_loss() {
    let n = 59;
    let mut lost_at_restarts: Option<usize> = None;
    let (ev, deg) = run2(n, 4000.0, |t, since, r| {
        if since < 1.0 { return (0..n).map(|i| format!("n{i}")).collect(); }
        let mut m = s(&["route_x_b9"]); // permanent
        if t >= 600.0 {
            let r0 = *lost_at_restarts.get_or_insert(r);
            if r == r0 { m.push("strip_mic".into()); m.push("strip_mic.in".into()); } // one restart fixes it
        }
        m
    });
    assert!(!deg.is_empty() && deg[0] < 600.0, "precondition: degraded before t=600 ({deg:?})");
    let first = ev.iter().find(|e| e.t >= 600.0).map(|e| e.t - 600.0).unwrap_or(f64::INFINITY);
    assert!(first <= 61.0, "mic strip lost at t=600 while degraded on another node: first restart after {first:.0}s");
}

/// R2-F2: run() passes expected(&cfg).len() as expected_total, but `missing` excludes
/// dependency-filtered nodes. With any dependency absent (default config: mic on
/// easyeffects_source, EasyEffects not running) missing.len() can never equal expected_total,
/// so the zero-node rule is dead. An alive child owning none of its nodes then goes down the
/// Stuck path and ends DEGRADED with the whole mixer silent for an hour.
#[test]
fn r2_zero_node_rule_dead_when_dependency_absent() {
    let cfg = Config::default_config();
    let has = |x: &str| x.starts_with("ch_") || x.starts_with("bus_"); // server sinks only
    let missing: Vec<String> = expected(&cfg).into_iter()
        .filter(|(n, dep)| !has(n) && dep.as_ref().map(|d| has(d)).unwrap_or(true))
        .map(|(n, _)| n).collect();
    let total = expected(&cfg).len();
    assert!(missing.len() < total, "precondition: the default config has a Source input");
    let (ev, deg) = run2(missing.len(), 900.0, |_, _, _| missing.clone()); // eligible count, as run() now passes
    let first = ev.first().map(|e| (e.t, e.why));
    assert!(matches!(first, Some((t, Why::ZeroNodes)) if t <= 13.0), "child owns 0 nodes: first restart {first:?}, degraded at {deg:?}");
    assert!(deg.is_empty(), "whole graph missing, engine went degraded at {deg:?} (silent mixer, next retry in 1 h)");
}

// ---------------- green probes ----------------

/// Degraded recovers by itself when the node comes back (30 s stable) and the next loss gets
/// the short grace.
#[test]
fn probe_degraded_recovers_when_node_returns() {
    let n = 59;
    let base = Instant::now(); let at = |t: f64| base + Duration::from_secs_f64(t);
    let mut sup = Supervisor::default();
    let (mut t, mut started) = (0.0, 0.0);
    sup.tick(at(t), false, false, &[], n); sup.spawned(at(t));
    let m = s(&["route_x_b9"]);
    while sup.degraded().is_none() {
        t += TICK;
        if sup.tick(at(t), true, false, &m, n).is_some() { sup.spawned(at(t)); started = t; }
        assert!(t < 2000.0);
    }
    let _ = started;
    let back = t;
    while sup.degraded().is_some() { t += TICK; assert!(sup.tick(at(t), true, false, &[], n).is_none()); assert!(t - back < 32.0); }
    assert_eq!(sup.patience(n), LOSS_GRACE);
}

/// R2-F3: a node that flaps on its own (gone 40 s / back 40 s, restarts do not change it).
/// Every 40 s complete stretch passes STABLE_AFTER, forgets all failures and re-arms the 5 s
/// grace, so the engine never degrades: 45 full-graph restarts (all audio cut) per hour, forever.
#[test]
fn r2_flapping_node_never_degrades() {
    let n = 59;
    let (ev, deg) = run2(n, 3600.0, |t, since, _| {
        if since < 1.0 { return (0..n).map(|i| format!("n{i}")).collect(); }
        if (t as u64 / 40) % 2 == 1 { s(&["strip_mic", "strip_mic.in"]) } else { vec![] }
    });
    eprintln!("flap: {} restarts/h, degraded entries {:?}", ev.len(), deg);
    assert!(ev.len() <= 12, "{} full-graph restarts in 1 h, degraded entries {deg:?}", ev.len());
}

/// Complete for 29 s after earlier failed restarts, then a loss: still counted as part of the
/// failure streak (F1 design); the patience is the backoff, not the 5 s grace.
#[test]
fn probe_29s_window_after_failures_uses_backoff() {
    let n = 59;
    let (ev, _) = run2(n, 400.0, |_, since, r| {
        if since < 1.0 { return (0..n).map(|i| format!("n{i}")).collect(); }
        if r < 2 { return s(&["a"]); }
        if since < 29.0 { vec![] } else if r == 2 { s(&["b"]) } else { vec![] }
    });
    assert!(ev.len() >= 3);
    let gap = ev[2].t - (ev[1].t + 29.0);
    eprintln!("29s window: loss -> restart {gap:.1}s");
    assert!(gap <= 61.0);
}
