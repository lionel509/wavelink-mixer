use super::*;

const TICK: f64 = 1.2;
const AFTER_RESTART: f64 = 1.5;
const N: usize = 59;

/// Drive the real Supervisor. `state(t, since_spawn) -> (missing, eligible)`.
/// Returns (restart times, degraded ever).
fn drive(total: f64, mut state: impl FnMut(f64, f64, bool) -> (Vec<String>, usize)) -> (Vec<f64>, bool) {
    let base = Instant::now();
    let at = |t: f64| base + Duration::from_secs_f64(t);
    let mut sup = Supervisor::default();
    let mut started = -60.0f64;
    let (mut ev, mut deg) = (vec![], false);
    let mut t = 0.0;
    let mut just = false;
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
    (ev, deg)
}
fn all(n: usize) -> Vec<String> { (0..n).map(|i| format!("n{i}")).collect() }

// ---------------- red ----------------

/// R4-F1: the refund rule loops. A node that dies ~35 s after every spawn (a plugin or
/// buffer failure that takes a while, the F1 "appears then dies" pattern with a slower fuse):
/// each restart "worked" (complete within patience, complete for 30 s), so every slot is
/// refunded, stuck_restarts resets and the 5 s grace applies: a full-graph restart every
/// ~42 s forever, never degraded.
#[test]
fn r4_refund_loop_node_dies_after_35s() {
    let (ev, deg) = drive(3600.0, |_, since, _| {
        if since < 2.0 { (all(N), N) } else if since < 35.0 { (vec![], N) } else { (vec!["route_x_b1".into()], N) }
    });
    assert!(ev.len() <= 10, "{} full-graph restarts in 1 h (degraded {deg}); first gaps {:?}", ev.len(), ev.windows(2).take(4).map(|w| (w[1] - w[0]).round()).collect::<Vec<_>>());
}

/// EasyEffects crash-looping (source present `on` s / absent `off` s); the mic strip dies whenever
/// its target returns (ee_toggle.sh shows that live) and only a graph restart revives it.
/// Returns (restarts, seconds the mic was dead while its source was up).
fn flap_dep(on: f64, off: f64, perm: bool) -> (usize, f64, bool) {
    let mut strips_alive = true;
    let mut dep_prev = true;
    let mut dead = 0.0;
    let mut last_t = 0.0;
    let (ev, deg) = drive(3600.0, |t, since, just| {
        let dep = (t % (on + off)) < on;
        if dep && !dep_prev { strips_alive = false; }
        dep_prev = dep;
        if just && dep { strips_alive = true; }
        if dep && !strips_alive { dead += t - last_t; }
        last_t = t;
        let e = if dep { N } else { N - 2 };
        if since < 2.0 { return (all(e), e); }
        let mut m = if perm { vec!["route_perm".to_string()] } else { vec![] };
        if dep && !strips_alive { m.push("strip_mic.in".into()); m.push("strip_mic".into()); }
        (m, e)
    });
    (ev.len(), dead, deg)
}

/// R4-F2a: dependency flapping (20 s on / 5 s off): every return earns an unbudgeted free
/// restart (full-graph, all audio cut), forever, never degraded.
#[test]
fn r4_flapping_dependency_unbudgeted_restart_loop() {
    let (n, dead, deg) = flap_dep(20.0, 5.0, false);
    assert!(n <= 12, "{n} full-graph restarts in 1 h from a flapping dependency (degraded ever: {deg}, mic dead {dead:.0}s)");
}

/// R4-F2b: same flap (15 s on / 5 s off) plus one unrelated permanently missing node: every
/// eligible change resets Settle, the backoff patience (>15 s) never elapses, so NOTHING is
/// ever repaired: the mic is dead every time EasyEffects is up, for the whole hour.
#[test]
fn r4_flapping_dependency_freezes_all_repair() {
    let (n, dead, deg) = flap_dep(15.0, 5.0, true);
    // ROUND 5: `dead <= 600.0` cannot hold together with R4-F2a (<= 12 restarts/h for the same flap):
    // the source returns every 20 s and each return kills the strip, so keeping the mic alive for
    // >= 83% of the hour needs a graph restart (all audio cut) per cycle, i.e. ~170/h. The property
    // kept here is the one the finding is about: repair is not frozen (restarts happen, up to the
    // limiter) and stays bounded. That other nodes are not frozen by the flap is covered by
    // supervisor_tests::flapping_dependency_does_not_freeze_repair_of_other_nodes.
    assert!(n >= 3 && n <= HARD_CAP, "mic dead {dead:.0}s of the hour while its source was up; {n} restarts, degraded ever {deg}");
}

/// R4-F3: an input whose Source target is one of the child's OWN outputs (pw::Graph::sources()
/// does not filter the child's virtual mics, so the GUI offers "Voice Chat Mix" etc.). Every
/// spawn makes eligible grow when that output appears => free_restart. With one permanently
/// missing node every stuck restart is free: unbudgeted loop, never degraded.
#[test]
fn r4_self_dependency_grants_free_restart_every_spawn() {
    let mut cfg = Config::default_config();
    let mut i = cfg.inputs[0].clone();
    i.id = "loopmix".into(); i.name = "Loop".into();
    i.kind = InputKind::Source { target: "voice_chat_mix".into() };
    cfg.inputs.push(i);
    let perm = names::route(&cfg.inputs[0].id, &cfg.buses[0].id);
    let (ev, deg) = drive(3600.0, |_, since, _| {
        let has = |x: &str| {
            if x.starts_with("ch_") || x.starts_with("bus_") || x == "easyeffects_source" { return true; }
            if x == perm { return false; }
            // child nodes come up over the first 3 s (voice_chat_mix last)
            if x == "voice_chat_mix" { since >= 3.0 } else { since >= 1.5 }
        };
        let (e, m) = eligible_and_missing(&cfg, has); (m, e)
    });
    assert!(deg || ev.len() <= 8, "{} restarts in 1 h for one permanently missing node, never degraded", ev.len());
}

// ---------------- green probes ----------------

/// Refund only for a restart whose graph completed within patience: a node that dies 10 s
/// after spawn (< STABLE_AFTER) is not refunded and ends degraded.
#[test]
fn probe_fast_dying_node_degrades() {
    let (ev, deg) = drive(3600.0, |_, since, _| {
        if since < 2.0 { (all(N), N) } else if since < 10.0 { (vec![], N) } else { (vec!["route_x_b1".into()], N) }
    });
    assert!(deg && ev.len() <= 6, "{} restarts, degraded {deg}", ev.len());
}

/// Two dependencies swapping in the same tick (net eligible unchanged): no free restart,
/// normal budget applies.
#[test]
fn probe_two_dependencies_swap_is_budgeted() {
    let (ev, deg) = drive(3600.0, |t, since, _| {
        if since < 2.0 { return (all(N), N); }
        if t > 100.0 { (vec!["strip_a".into(), "strip_a.in".into()], N) } else { (vec![], N) }
    });
    assert!(deg && ev.len() <= 4, "{} restarts", ev.len());
}
