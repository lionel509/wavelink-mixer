use super::*;

#[test]
fn patience_scales_with_size_and_backs_off() {
    assert!(graph_patience(56, 0) > Duration::from_secs(5) + Duration::from_millis(1500));
    assert!(graph_patience(130, 0) > graph_patience(56, 0));
    assert_eq!(graph_patience(56, 1), graph_patience(56, 0) * 2);
    assert_eq!(graph_patience(56, 2), Duration::from_secs(60));
    assert_eq!(graph_patience(56, 40), Duration::from_secs(60));
    assert_eq!(graph_patience(500, 9), Duration::from_secs(60));
}

#[test]
fn slow_creation_within_patience_never_restarts() {
    let t0 = Instant::now();
    let mut sup = Supervisor::default();
    sup.spawned(t0);
    // 100 nodes appear one per 0.12 s: all up after 12 s, well inside the fresh patience
    for k in 0..400u64 {
        let t = t0 + Duration::from_millis(k * 120);
        let up = ((k * 120) / 120) as usize;
        let missing: Vec<String> = (up.min(100)..100).map(|i| format!("n{i}")).collect();
        assert_eq!(sup.tick(t, true, false, &missing, 100), None, "k={k}");
    }
}

#[test]
fn restarts_when_stuck_and_never_when_complete() {
    let t0 = Instant::now();
    let mut sup = Supervisor::default();
    sup.spawned(t0);
    let m = vec!["a".to_string(), "b".to_string(), "c".to_string()];
    let p = graph_patience(56, 0);
    assert_eq!(sup.tick(t0 + Duration::from_secs(1), true, false, &m, 56), None);
    assert_eq!(sup.tick(t0 + p / 2, true, false, &m, 56), None);
    assert_eq!(sup.tick(t0 + Duration::from_secs(1) + p, true, false, &m, 56), Some(Why::Stuck));
    let mut sup = Supervisor::default();
    sup.spawned(t0);
    for k in 1..500u64 { assert_eq!(sup.tick(t0 + Duration::from_secs(k), true, false, &[], 56), None); }
    // the old rule gave a 28-node graph ~5.5 s; the fresh patience must now outlast that
    assert!(graph_patience(56, 0) > Duration::from_millis(5500));
}

/// A large synthetic config: the child conf names every expected node exactly once.
#[test]
fn large_config_conf_covers_expected_nodes() {
    let mut cfg = Config::default_config();
    let tmpl = cfg.inputs[0].clone();
    let bus = cfg.buses[0].clone();
    while cfg.inputs.len() < 10 { let mut i = tmpl.clone(); i.id = format!("x{}", cfg.inputs.len()); i.name = i.id.clone(); cfg.inputs.push(i); }
    while cfg.buses.len() < 6 { let mut b = bus.clone(); b.id = format!("y{}", cfg.buses.len()); b.name = b.id.clone(); cfg.buses.push(b); }
    let conf = child_conf(&cfg);
    for (n, _) in expected(&cfg) {
        if n.starts_with("route_") || n.starts_with("strip_") {
            assert_eq!(conf.matches(&format!("node.name = \"{n}\"")).count(), 1, "{n}");
        }
    }
    // per input: strip.in + strip + 6 routes; plus the virtual buses' own nodes
    let virt = cfg.buses.iter().filter(|b| matches!(b.kind, BusKind::Virtual { .. })).count();
    assert_eq!(expected(&cfg).len(), 10 * (2 + 6) + virt);
}
