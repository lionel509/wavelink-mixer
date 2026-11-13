use super::*;

const TICK: f64 = 1.2; // 1 s sleep + pw-dump
const AFTER_RESTART: f64 = 1.5;

struct Ev { t: f64, stuck: bool }

/// `missing(t, since_spawn, restarts_so_far)`; `structure_change_at`: times at which the config
/// structure changes (forces a restart like run() does).
fn sim(n: usize, total: f64, structure_change_at: &[f64], mut missing: impl FnMut(f64, f64, usize) -> usize) -> Vec<Ev> {
    let base = Instant::now();
    let at = |t: f64| base + Duration::from_secs_f64(t);
    let mut sup = Supervisor::default();
    let mut child_started = -60.0f64;
    let mut pending_struct: Vec<f64> = structure_change_at.to_vec();
    let mut ev: Vec<Ev> = vec![];
    let mut t = 0.0;
    while t < total {
        let m = missing(t, t - child_started, ev.len());
        let names: Vec<String> = (0..m).map(|i| format!("n{i}")).collect();
        let struct_changed = pending_struct.first().map_or(false, |s| t >= *s);
        // alive: the first spawn happens when there is no child yet (spawned_at None => Dead)
        if let Some(why) = sup.tick(at(t), true, struct_changed, &names, n) {
            if why == Why::Structure { pending_struct.remove(0); }
            if child_started > -59.0 { ev.push(Ev { t, stuck: why == Why::Stuck }); }
            child_started = t;
            sup.spawned(at(t));
            t += AFTER_RESTART;
            continue;
        }
        t += TICK;
    }
    ev
}

fn cfg_nxm(nin: usize, nbus: usize) -> Config {
    let mut cfg = Config::default_config();
    let tmpl = cfg.inputs[0].clone();
    let bus = cfg.buses.iter().find(|b| matches!(b.kind, BusKind::Virtual { .. })).unwrap().clone();
    cfg.inputs.truncate(nin.min(cfg.inputs.len()));
    while cfg.inputs.len() < nin { let mut i = tmpl.clone(); i.id = format!("x{}", cfg.inputs.len()); i.name = i.id.clone(); cfg.inputs.push(i); }
    cfg.buses.truncate(nbus.min(cfg.buses.len()));
    while cfg.buses.len() < nbus {
        let mut b = bus.clone(); let k = cfg.buses.len();
        b.id = format!("y{k}"); b.name = b.id.clone();
        b.kind = BusKind::Virtual { node: format!("vmix{k}"), description: b.id.clone() };
        cfg.buses.push(b);
    }
    cfg
}

// ---------------- red: real defects ----------------

/// F1: a node that a restart cannot bring back (server-side fd/mem failure, i.e. the server's
/// own 1024 limit the PR leaves alone, or a child whose limit could not be raised because
/// hard == soft) makes the engine tear the WHOLE working graph down forever, every <=120 s.
/// Restarting is pointless after the same set came back missing a few times in a row.
#[test]
fn red_permanent_missing_node_restarts_forever() {
    let n = expected(&cfg_nxm(8, 5)).len();
    let ev = sim(n, 3600.0, &[], |_, since, _| if since < 1.0 { n } else { 1 });
    let stuck = ev.iter().filter(|e| e.stuck).count();
    assert!(stuck <= 4, "{stuck} full-graph restarts (audio cut each time) in 1 h for one permanently missing node; last at {:.0}s", ev.last().map_or(0.0, |e| e.t));
}

/// F2: a node lost from a graph that has been COMPLETE for minutes (not a slow fresh spawn,
/// so there is no creation race to wait for) is now left broken 3x longer than before at
/// 8x5, and >2x longer even on the small default config. Old rule: ~4 ticks (~5 s).
#[test]
fn red_steady_state_loss_restart_latency() {
    for (label, cfg) in [("default", Config::default_config()), ("8x5", cfg_nxm(8, 5))] {
        let n = expected(&cfg).len();
        let mut fixed = false;
        let ev = sim(n, 900.0, &[], |t, _, r| {
            if r >= 1 { fixed = true; }
            if t >= 600.0 && !fixed { 1 } else { 0 }
        });
        let first = ev.iter().find(|e| e.t >= 600.0).map(|e| e.t - 600.0).unwrap_or(f64::INFINITY);
        assert!(first <= 6.5, "{label} ({n} nodes): node lost at t=600 after a complete graph, restart only after {first:.1}s (old rule ~5 s)");
    }
}

/// F3: stuck_restarts (the x2..x16 backoff) survives a structure change. A user who edits the
/// config after a stuck period gets a brand-new graph that inherits up to 120 s patience.
#[test]
fn red_backoff_survives_structure_change() {
    let n = expected(&cfg_nxm(8, 5)).len();
    // graph A is permanently stuck for 10 min, structure change at t=600; graph B also has a
    // missing node (different cause) that a restart would fix
    let mut b_spawned_at: Option<usize> = None;
    let ev = sim(n, 1200.0, &[600.0], |t, since, r| {
        if since < 1.0 { return n; }
        if t < 600.0 { return 1; }
        // r0 = restarts before the structure restart; B is the graph after it
        let r0 = *b_spawned_at.get_or_insert(r);
        if r > r0 + 1 { 0 } else { 1 }
    });
    let sc = ev.iter().find(|e| e.t >= 600.0).expect("structure restart").t;
    let next = ev.iter().find(|e| e.t > sc).map(|e| e.t - sc).unwrap_or(f64::INFINITY);
    let fresh = graph_patience(n, 0).as_secs_f64() + 3.0;
    assert!(next <= fresh, "new structure stuck for {next:.0}s before its first restart; a fresh graph should get ~{:.0}s", fresh - 3.0);
}

/// F0 (live-confirmed): in the #22 failure mode itself (child still at 1024 fds, e.g. hard ==
/// soft so the pre_exec raise is a no-op, or the server's own limit at large sizes) every node
/// first APPEARS (graph complete ~2 s after spawn), then the fd-starved ones die. The brief
/// complete graph resets stuck_restarts, so the x2..x16 backoff never engages: a restart every
/// ~18 s at 8x5 forever, each one "attempt 1". Private PipeWire, `ulimit -n 1024`, 90 s:
/// 58/58 nodes present at t=2,20,39,57,77 s, 4 restarts all "(stuck, attempt 1)".
#[test]
fn red_transient_complete_graph_defeats_backoff() {
    let n = expected(&cfg_nxm(8, 5)).len();
    let ev = sim(n, 600.0, &[], |_, since, _| if since < 1.0 { n } else if since < 3.5 { 0 } else { 7 });
    let stuck = ev.iter().filter(|e| e.stuck).count();
    // with the intended backoff (17, 34, 68, 120, 120 ...) 10 min holds at most ~7 restarts
    assert!(stuck <= 7, "{stuck} restarts in 10 min; intervals {:?}", ev.windows(2).map(|w| (w[1].t - w[0].t).round()).collect::<Vec<_>>());
}

// ---------------- green probes (regression tests) ----------------

#[test]
fn probe_zero_nodes_and_complete_never_stuck() {
    assert_eq!(graph_patience(0, 0), Duration::from_secs(8));
    let cfg = Config { inputs: vec![], buses: vec![], ..Config::default_config() };
    assert!(expected(&cfg).is_empty());
}

#[test]
fn probe_clock_earlier_than_spawn_no_panic() {
    let t0 = Instant::now() + Duration::from_secs(100);
    let mut sup = Supervisor::default();
    sup.spawned(t0);
    let m = vec!["a".to_string()];
    assert_eq!(sup.tick(t0, true, false, &m, 5), None);
    assert_eq!(sup.tick(t0 - Duration::from_secs(50), true, false, &m, 5), None);
}

#[test]
fn probe_patience_caps_and_monotone() {
    for r in [0u32, 1, 4, 5, 63, 64, u32::MAX] {
        assert!(graph_patience(10_000, r) <= Duration::from_secs(120));
    }
    let mut prev = Duration::ZERO;
    for n in 0..800 { let p = graph_patience(n, 0); assert!(p >= prev); prev = p; }
    for n in [1usize, 61, 135] {
        for r in 0..6 { assert!(graph_patience(n, r + 1) >= graph_patience(n, r)); }
    }
}

/// Transient fresh-spawn race (all nodes up within a few seconds) at 8x5: never restarts.
#[test]
fn probe_slow_spawn_8x5_no_restart() {
    let n = expected(&cfg_nxm(8, 5)).len();
    let ev = sim(n, 600.0, &[], |_, since, _| if since < 12.0 { (n as f64 * (1.0 - since / 12.0)).max(1.0) as usize } else { 0 });
    assert!(ev.is_empty(), "{} restarts", ev.len());
}

/// A fault that one restart clears (the #22 fd race once the limit is raised) recovers with
/// exactly one restart and the backoff resets.
#[test]
fn probe_one_restart_clears_fault() {
    let n = expected(&cfg_nxm(8, 5)).len();
    let ev = sim(n, 900.0, &[], |_, since, r| if since < 1.0 { n } else if r == 0 { 3 } else { 0 });
    assert_eq!(ev.len(), 1);
    assert!(ev[0].t < graph_patience(n, 0).as_secs_f64() + 4.0);
}
