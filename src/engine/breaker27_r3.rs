use super::*;

const TICK: f64 = 1.2;
const AFTER_RESTART: f64 = 1.5;
const N: usize = 59;

#[derive(Debug)]
struct Ev { t: f64, why: Why }

fn run3(total: f64, mut missing: impl FnMut(f64, f64, usize) -> Vec<String>) -> (Vec<Ev>, Vec<(f64, Vec<String>)>) {
    let base = Instant::now();
    let at = |t: f64| base + Duration::from_secs_f64(t);
    let mut sup = Supervisor::default();
    let mut started = -60.0f64;
    let (mut ev, mut deg) = (vec![], vec![]);
    let mut was = false;
    let mut t = 0.0;
    while t < total {
        let m = missing(t, t - started, ev.len());
        let why = sup.tick(at(t), true, false, &m, N);
        match (sup.degraded(), was) { (Some(d), false) => { deg.push((t, d.to_vec())); was = true } (None, true) => was = false, _ => {} }
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
fn all() -> Vec<String> { (0..N).map(|i| format!("n{i}")).collect() }

// ---------------- red ----------------

/// R3-F1: restarts that WORKED (graph complete right after, stable for minutes) still use up
/// the rolling-hour budget. Six separate losses, each fixed by one restart, then a seventh at
/// t=1860 that one restart would also fix: the engine goes degraded and leaves it broken until
/// an hour after the 6th restart (~50 min). A restart followed by a stable complete graph is
/// proof restarts help; it should not count like a flap.
#[test]
fn r3_successful_restarts_exhaust_budget_and_block_next_fix() {
    let losses = [60.0, 360.0, 660.0, 960.0, 1260.0, 1560.0, 1860.0];
    let mut fixed_by: Vec<Option<usize>> = vec![None; losses.len()];
    let (ev, deg) = run3(4000.0, |t, since, r| {
        if since < 1.0 { return all(); }
        for (k, l) in losses.iter().enumerate() {
            if t >= *l && (k + 1 == losses.len() || t < losses[k + 1]) {
                let r0 = *fixed_by[k].get_or_insert(r);
                return if r == r0 { vec![format!("route_{k}")] } else { vec![] };
            }
        }
        vec![]
    });
    let first = ev.iter().find(|e| e.t >= 1860.0).map(|e| e.t - 1860.0).unwrap_or(f64::INFINITY);
    assert!(first <= 15.0, "7th fixable loss: first restart after {first:.0}s, degraded {deg:?}; restarts at {:?}", ev.iter().map(|e| e.t.round()).collect::<Vec<_>>());
}

/// R3-F2: the subset rule (R2-F1 fix) is a no-op once degraded was entered through the budget:
/// the new node clears degraded, but restart_log still holds 6 entries, so the next stuck
/// decision re-degrades with {old, new} without a single restart.
#[test]
fn r3_subset_rule_defeated_by_full_budget() {
    // 6 stuck restarts with a different (unfixable) set each time -> budget path to degraded
    let (ev, deg) = run3(3000.0, |t, since, r| {
        if since < 1.0 { return all(); }
        let mut m = vec![format!("x{}", r.min(6))];
        if t >= 1200.0 && r <= 7 { m.push("strip_mic".into()); } // new, one restart fixes it
        m
    });
    assert!(!deg.is_empty() && deg[0].0 < 1200.0, "precondition: degraded via budget before t=1200: {deg:?}");
    let first = ev.iter().find(|e| e.t >= 1200.0).map(|e| e.t - 1200.0).unwrap_or(f64::INFINITY);
    assert!(first <= 61.0, "new node lost at t=1200 while degraded on the budget: first restart after {first:.0}s; degraded entries {deg:?}");
}

// ---------------- green probes ----------------

/// Exactly-1 h edge: an entry exactly 3600 s old no longer counts; 6 in the window block the 7th.
#[test]
fn probe_budget_window_edge() {
    let base = Instant::now(); let at = |t: f64| base + Duration::from_secs_f64(t);
    let mut sup = Supervisor::default();
    sup.tick(at(0.0), false, false, &[], N); sup.spawned(at(0.0));
    let mut restarts = vec![];
    let mut t = 0.0;
    let mut k = 0;
    while t < 3700.0 {
        t += 1.0;
        let m = vec![format!("u{k}")]; // new set every child: same_set never trips
        if sup.tick(at(t), true, false, &m, N) == Some(Why::Stuck) { restarts.push(t); sup.spawned(at(t)); k += 1; }
        if sup.degraded().is_some() { break; }
    }
    assert_eq!(restarts.len(), RESTART_CAP, "{restarts:?}");
}

/// ZeroNodes never consumes the budget nor degrades, and waits 10/20/40/60/60 s.
#[test]
fn probe_zero_nodes_independent_of_budget() {
    let (ev, deg) = run3(600.0, |_, _, _| all());
    assert!(deg.is_empty());
    assert!(ev.iter().all(|e| e.why == Why::ZeroNodes));
    let gaps: Vec<f64> = ev.windows(2).map(|w| (w[1].t - w[0].t).round()).collect();
    assert!(gaps.iter().all(|g| *g <= 62.0), "{gaps:?}");
    assert!(ev.len() >= 8);
}

/// R3-F3 (live-confirmed): when easyeffects_source goes away and comes back (EasyEffects
/// restarted / crashed), the child's mic strip nodes are gone and only a graph restart brings
/// them back. Private PipeWire, default config, source toggled every 15 s: the strips vanished
/// on the 2nd return and stayed gone for 90+ s with no restart, because each dep-absent window
/// filters them out (missing = 0 = "complete") and resets Settle before the 27 s patience.
/// Here EasyEffects restarts every 8 min (absent 3 s): each one costs a full-graph restart and
/// a budget slot, so from the 7th the engine goes degraded and the mic stays dead ~1 h.
#[test]
fn r3_easyeffects_restarts_exhaust_budget_mic_dead() {
    let base = Instant::now(); let at = |t: f64| base + Duration::from_secs_f64(t);
    let mut sup = Supervisor::default();
    let strips = vec!["strip_mic.in".to_string(), "strip_mic".to_string()];
    let (mut t, mut dep_prev, mut alive_strips) = (0.0f64, true, true);
    let mut started = -60.0f64;
    let mut outage_start: Option<f64> = None;
    let mut worst: f64 = 0.0;
    while t < 3600.0 {
        let dep = (t % 480.0) >= 3.0 || t < 480.0;
        if dep && !dep_prev { alive_strips = false; } // strip dies when the target comes back
        dep_prev = dep;
        let m: Vec<String> = if t - started < 1.0 { (0..N).map(|i| format!("n{i}")).collect() }
            else if dep && !alive_strips { strips.clone() } else { vec![] };
        let eligible = if dep { N } else { N - 2 };
        if dep && !alive_strips { outage_start.get_or_insert(t); } else if let Some(o) = outage_start.take() { worst = worst.max(t - o); }
        if let Some(_) = sup.tick(at(t), true, false, &m, eligible) {
            started = t; sup.spawned(at(t)); if dep { alive_strips = true; }
            t += AFTER_RESTART; continue;
        }
        t += TICK;
    }
    if let Some(o) = outage_start { worst = worst.max(t - o); }
    assert!(worst <= 60.0, "worst mic outage after an EasyEffects restart: {worst:.0}s (degraded: {:?})", sup.degraded());
}

/// Dependency fast toggle (period 15 s) after one restart: the strip stays dead as long as
/// the toggling continues (each absent window resets Settle); documents the live run.
#[test]
fn r3_fast_dependency_toggle_never_repairs() {
    let base = Instant::now(); let at = |t: f64| base + Duration::from_secs_f64(t);
    let mut sup = Supervisor::default();
    sup.tick(at(0.0), false, false, &[], N); sup.spawned(at(0.0));
    sup.stuck_restarts = 1; // as in the live run: one earlier restart, patience 27 s
    let strips = vec!["strip_mic.in".to_string(), "strip_mic".to_string()];
    let mut t = 10.0; let mut restarted = false;
    while t < 600.0 {
        let dep = ((t / 15.0) as u64) % 2 == 0;
        let m = if dep { strips.clone() } else { vec![] };
        if sup.tick(at(t), true, false, &m, if dep { N } else { N - 2 }).is_some() { restarted = true; break; }
        t += TICK;
    }
    assert!(restarted, "mic strip dead whenever EasyEffects is up, for 10 min, never restarted");
}
