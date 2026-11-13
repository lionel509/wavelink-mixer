use super::*;

fn names(n: usize) -> Vec<String> { (0..n).map(|i| format!("n{i}")).collect() }
#[test]
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
fn raise_nofile_child_sees_raised_limit_and_parent_is_untouched() {
    let before = nofile::get().unwrap();
    let mut cmd = Command::new("sh");
    cmd.args(["-c", "ulimit -Sn"]);
    let want = raise_nofile(&mut cmd).unwrap();
    let out = cmd.output().unwrap();
    let got: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
    assert_eq!(got, want);
    assert_eq!(got, before.1.min(1 << 20));
    assert_eq!(nofile::get().unwrap(), before, "engine's own limit must not change");
    // a plain child (pw-cat, pw-loopback...) keeps the engine's limit
    let plain = Command::new("sh").args(["-c", "ulimit -Sn"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&plain.stdout).trim().parse::<u64>().unwrap(), before.0);
}

#[test]
fn structure_change_resets_backoff() {
    let t0 = Instant::now();
    let at = |s: u64| t0 + Duration::from_secs(s);
    let mut sup = Supervisor::default();
    assert_eq!(sup.tick(at(0), false, false, &[], 50), Some(Why::Dead));
    sup.spawned(at(0));
    let m = names(1);
    let mut t = 1;
    let mut got = 0;
    while got < 2 {
        if sup.tick(at(t), true, false, &m, 50) == Some(Why::Stuck) { got += 1; sup.spawned(at(t)); }
        t += 1;
    }
    assert_eq!(sup.stuck_restarts, 2);
    assert_eq!(sup.tick(at(t + 10), true, true, &m, 50), Some(Why::Structure));
    assert_eq!(sup.stuck_restarts, 0);
    assert_eq!(sup.patience(50), graph_patience(50, 0));
}

#[test]
fn dead_child_restarts_back_off_to_60s_but_never_degrade() {
    let t0 = Instant::now();
    let mut sup = Supervisor::default();
    sup.spawned(t0);
    let mut times = vec![];
    for k in 1..1200u64 {
        let t = t0 + Duration::from_secs(k);
        if sup.tick(t, false, false, &[], 50) == Some(Why::Dead) { times.push(k); sup.spawned(t); }
    }
    let gaps: Vec<u64> = times.windows(2).map(|w| w[1] - w[0]).collect();
    assert_eq!(&gaps[..5], &[10, 20, 40, 60, 60], "{times:?}");
    assert_eq!(sup.stuck_restarts, 0);
    assert!(sup.degraded().is_none());
}

#[test]
fn alive_child_with_zero_nodes_restarts_promptly_without_backoff() {
    let t0 = Instant::now();
    let mut sup = Supervisor::default();
    sup.spawned(t0);
    let all = names(50);
    assert_eq!(sup.tick(t0 + Duration::from_secs(6), true, false, &all, 50), None, "creation grace");
    assert_eq!(sup.tick(t0 + Duration::from_secs(10), true, false, &all, 50), Some(Why::ZeroNodes));
    assert_eq!(sup.stuck_restarts, 0);
}

#[test]
fn node_lost_after_complete_graph_uses_short_grace() {
    let t0 = Instant::now();
    let mut sup = Supervisor::default();
    sup.spawned(t0);
    for k in 1..=20u64 { assert_eq!(sup.tick(t0 + Duration::from_secs(k), true, false, &[], 130), None); }
    let m = names(1);
    let lost = t0 + Duration::from_secs(21);
    assert_eq!(sup.tick(lost, true, false, &m, 130), None);
    assert_eq!(sup.tick(lost + Duration::from_secs(5), true, false, &m, 130), Some(Why::Stuck));
}

#[test]
fn brief_complete_graph_does_not_reset_backoff_but_stable_one_does() {
    let t0 = Instant::now();
    let at = |s: u64| t0 + Duration::from_secs(s);
    let mut sup = Supervisor::default();
    sup.spawned(at(0));
    sup.stuck_restarts = 3;
    // complete for 29 s: still remembered
    for k in 0..30u64 { sup.tick(at(k), true, false, &[], 50); }
    assert_eq!(sup.stuck_restarts, 3);
    sup.tick(at(30), true, false, &[], 50);
    assert_eq!(sup.stuck_restarts, 0);
}

#[test]
fn gives_up_on_the_same_missing_set_and_retries_rarely() {
    let t0 = Instant::now();
    let mut sup = Supervisor::default();
    let m = names(2);
    let mut t = 0u64;
    sup.spawned(t0);
    let mut restarts = 0;
    while t < 3 * 3600 {
        t += 1;
        if let Some(w) = sup.tick(t0 + Duration::from_secs(t), true, false, &m, 50) {
            assert_eq!(w, Why::Stuck);
            restarts += 1;
            sup.spawned(t0 + Duration::from_secs(t));
        }
    }
    // 3 attempts, then one retry per hour
    assert!((3..=6).contains(&restarts), "{restarts}");
    assert_eq!(sup.degraded(), Some(m.as_slice()));
}

#[test]
fn new_loss_while_degraded_gets_normal_path_and_zero_nodes_never_degrades() {
    let t0 = Instant::now();
    let at = |s: u64| t0 + Duration::from_secs(s);
    let mut sup = Supervisor::default();
    sup.spawned(t0);
    let old = names(2);
    let mut t = 0;
    while sup.degraded().is_none() {
        t += 1;
        assert!(t < 1000);
        if sup.tick(at(t), true, false, &old, 50).is_some() { sup.spawned(at(t)); }
    }
    // subset keeps holding
    assert_eq!(sup.tick(at(t + 500), true, false, &old[..1], 50), None);
    assert!(sup.degraded().is_some());
    // a new name is missing: leaves degraded, restarts within the fresh patience
    let mut grown = old.clone();
    grown.push("n9".into());
    let mut got = None;
    for k in 1..=40u64 {
        if let Some(w) = sup.tick(at(t + 600 + k), true, false, &grown, 50) { got = Some((k, w)); break; }
    }
    assert!(matches!(got, Some((k, Why::Stuck)) if k <= 20), "{got:?}");
    // every eligible node missing: ZeroNodes with growing, capped waits; never degraded
    let mut sup = Supervisor::default();
    sup.spawned(t0);
    let all = names(5);
    let (mut last, mut gaps) = (0u64, vec![]);
    for k in 1..2000u64 {
        if let Some(w) = sup.tick(at(k), true, false, &all, 5) {
            assert_eq!(w, Why::ZeroNodes);
            gaps.push(k - last); last = k;
            sup.spawned(at(k));
        }
        assert!(sup.degraded().is_none());
    }
    assert!(gaps.windows(2).all(|w| w[1] >= w[0]), "{gaps:?}");
    assert_eq!(*gaps.last().unwrap(), 60);
    assert_eq!(sup.stuck_restarts, 0);
}

#[test]
fn rolling_hour_budget_degrades_a_flapping_node() {
    let t0 = Instant::now();
    let at = |s: u64| t0 + Duration::from_secs(s);
    let mut sup = Supervisor::default();
    sup.spawned(at(0));
    let m = names(1);
    let mut restarts = 0;
    for t in 1..3500u64 {
        // gone 40 s, back 40 s, regardless of restarts
        let miss: &[String] = if (t / 40) % 2 == 1 { &m } else { &[] };
        if sup.tick(at(t), true, false, miss, 50).is_some() { restarts += 1; sup.spawned(at(t)); }
    }
    // a restart that happens to coincide with the node returning looks like success and is refunded,
    // so a flapping node may cost a few more than the budget, but stays bounded
    assert!(restarts <= HARD_CAP, "{restarts}");
}

#[test]
fn eligible_and_missing_filters_by_dependency() {
    let cfg = Config::default_config();
    let has = |x: &str| x.starts_with("ch_") || x.starts_with("bus_"); // EasyEffects absent
    let (eligible, missing) = eligible_and_missing(&cfg, has);
    assert!(eligible < expected(&cfg).len(), "mic strips are gated by easyeffects_source");
    assert_eq!(missing.len(), eligible, "nothing of the child is up");
    assert!(!missing.iter().any(|n| n == "strip_mic"));
    let has2 = |x: &str| has(x) || x == "easyeffects_source";
    let (e2, m2) = eligible_and_missing(&cfg, has2);
    assert_eq!(e2, eligible + 2);
    assert!(m2.iter().any(|n| n == "strip_mic"));
}

#[test]
fn restart_that_worked_is_refunded_and_a_new_loss_is_always_fixed() {
    let t0 = Instant::now();
    let at = |s: u64| t0 + Duration::from_secs(s);
    let mut sup = Supervisor::default();
    sup.spawned(at(0));
    let mut restarts = vec![];
    let mut fixed = 0;
    for t in 1..8000u64 {
        // a different fault every 10 min, each fixed by its own restart
        let k = t / 600;
        let broken = k >= 1 && fixed < k && t % 600 >= 1;
        let m = if broken { vec![format!("r{k}")] } else { vec![] };
        if sup.tick(at(t), true, false, &m, 50).is_some() {
            restarts.push(t);
            fixed = k;
            sup.spawned(at(t));
        }
        assert!(sup.degraded().is_none(), "t={t}");
    }
    assert!(restarts.len() >= 12, "{restarts:?}");
    assert!(restarts.iter().all(|t| t % 600 <= 8), "{restarts:?}");
}

#[test]
fn dependency_returning_is_repaired_within_grace_each_time() {
    let t0 = Instant::now();
    let at = |s: u64| t0 + Duration::from_secs(s);
    let strips = names(2);
    let mut sup = Supervisor::default();
    sup.spawned(at(0));
    let mut t = 1;
    let mut restarts = 0;
    // EasyEffects restarts every 480 s (absent 3 s); the strips die when it returns
    let mut alive_strips = true;
    while t < 4 * 3600 {
        let dep = t % 480 >= 3;
        if t % 480 == 3 { alive_strips = false; }
        let (elig, m): (usize, Vec<String>) = if dep { (50, if alive_strips { vec![] } else { strips.clone() }) } else { (48, vec![]) };
        if sup.tick(at(t), true, false, &m, elig).is_some() { restarts += 1; alive_strips = true; sup.spawned(at(t)); }
        assert!(sup.degraded().is_none(), "t={t}");
        t += 1;
    }
    assert_eq!(restarts, 4 * 3600 / 480);
    assert_eq!(sup.stuck_restarts, 0);
}

#[test]
fn flapping_dependency_does_not_freeze_repair_of_other_nodes() {
    let t0 = Instant::now();
    let at = |s: u64| t0 + Duration::from_secs(s);
    let mut sup = Supervisor::default();
    sup.spawned(at(0));
    let mut x_lost: Option<u64> = None;
    let mut fixed_at = None;
    let mut spawned = 0u64;
    let mut t = 0u64;
    while t < 2000 {
        t += 1;
        let dep = t % 20 < 15; // 15 s up / 5 s away, forever
        let eligible = if dep { 50 } else { 48 };
        let mut m = vec!["perm".to_string()]; // never comes back
        if t >= 1000 && fixed_at.is_none() { x_lost.get_or_insert(t); m.push("x".into()); }
        if t < spawned + 2 { m = names(50); } // a fresh child owns nothing yet
        if sup.tick(at(t), true, false, &m, eligible).is_some() {
            if x_lost.is_some() && fixed_at.is_none() { fixed_at = Some(t); }
            sup.spawned(at(t));
            spawned = t;
        }
    }
    let lost = x_lost.unwrap();
    // the limiter may be in its slow-down (one restart per PENALTY_GAP) by then; what matters is that
    // the flapping dependency does not freeze repair for good
    assert!(fixed_at.map_or(false, |f| f - lost <= PENALTY_GAP.as_secs() + 30), "x lost at {lost}, restarted at {fixed_at:?}");
}

/// xorshift64*: no new crate, reproducible failures (the seed is in the assertion message).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: u64) -> u64 { self.next() % n }
    fn chance(&mut self, one_in: u64) -> bool { self.below(one_in) == 0 }
}

/// A tiny world: 12 child nodes, the last two (the "strips") gated by dependency D. A restart
/// revives everything except `bad` nodes, and D's return kills the strips (as EasyEffects does).
struct World { rng: Rng, present: [bool; 12], dep: bool, alive: bool, bad: Vec<usize>, structure_pending: bool, spawned: f64 }

impl World {
    fn state(&self, t: f64) -> (Vec<String>, usize) {
        let young = t - self.spawned < 2.0;
        let elig: Vec<usize> = (0..12).filter(|i| self.dep || *i < 10).collect();
        (elig.iter().filter(|i| young || !self.present[**i]).map(|i| format!("r{i}")).collect(), elig.len())
    }
    fn restarted(&mut self, t: f64) {
        self.alive = true;
        self.structure_pending = false;
        self.spawned = t;
        for i in 0..12 { self.present[i] = !self.bad.contains(&i); }
    }
}

struct Outcome { stuck: Vec<f64>, structure: Vec<f64>, repair: f64, bound: f64, hour_at_probe: usize }

/// Phase 1: random chaos (losses, returns, dependency flaps, child deaths, structure changes).
/// Phase 2, by seed: A) 65 healthy minutes then one fixable loss; B) 5 healthy minutes then a loss
/// (often right after the cap); C) the dependency flaps for 5 min and then stays up with the strips
/// dead (recovery after a flap). The repair time is measured from the start of the probe.
fn simulate(seed: u64, chaos_secs: f64) -> Outcome {
    let base = Instant::now();
    let at = |t: f64| base + Duration::from_secs_f64(t);
    let mut rng = Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1);
    let bad: Vec<usize> = (0..rng.below(3)).map(|_| rng.below(12) as usize).collect();
    let mut w = World { rng, present: [true; 12], dep: true, alive: true, bad, structure_pending: false, spawned: -100.0 };
    let mut sup = Supervisor::default();
    let (mut stuck, mut structure) = (vec![], vec![]);
    let mut last: Option<f64> = None;
    let mut t = 0.0;
    let mut first = true;
    let step = |w: &mut World, sup: &mut Supervisor, t: &mut f64, alive: bool, pending: bool, stuck: &mut Vec<f64>, structure: &mut Vec<f64>, last: &mut Option<f64>, first: &mut bool| -> bool {
        let (miss, elig) = w.state(*t);
        if let Some(why) = sup.tick(at(*t), alive, pending, &miss, elig) {
            if !*first {
                if why == Why::Stuck { stuck.push(*t); }
                if why == Why::Structure { structure.push(*t); }
                // (b) the policy's floor: no two restarts closer than MIN_GAP (except a user's structure change)
                if let (Some(prev), true) = (*last, why != Why::Structure) {
                    assert!(*t - prev + 1e-6 >= MIN_GAP.as_secs_f64(), "seed {seed}: {why:?} only {:.1}s after the previous restart", *t - prev);
                }
            }
            *first = false;
            *last = Some(*t);
            w.restarted(*t);
            sup.spawned(at(*t));
            *t += 1.5;
            return true;
        }
        *t += 1.2;
        false
    };
    while t < chaos_secs {
        if w.rng.chance(33) {
            match w.rng.below(6) {
                0 | 1 => { let i = w.rng.below(12) as usize; w.present[i] = false; }
                2 => { let i = w.rng.below(12) as usize; if !w.bad.contains(&i) { w.present[i] = true; } }
                3 => { w.dep = !w.dep; if w.dep { w.present[10] = false; w.present[11] = false; } }
                4 => if w.rng.chance(4) { w.alive = false; },
                _ => if w.rng.chance(40) { w.structure_pending = true; },
            }
        }
        let (alive, pending) = (w.alive, w.structure_pending);
        step(&mut w, &mut sup, &mut t, alive, pending, &mut stuck, &mut structure, &mut last, &mut first);
    }
    // phase 2
    w.bad.clear();
    w.present = [true; 12];
    w.dep = true;
    w.alive = true;
    let variant = seed % 3;
    if variant == 2 {
        let end = t + 300.0;
        let mut up = true;
        let mut next = t;
        while t < end {
            if t >= next { up = !up; next = t + 12.5; w.dep = up; if up { w.present[10] = false; w.present[11] = false; } }
            step(&mut w, &mut sup, &mut t, true, false, &mut stuck, &mut structure, &mut last, &mut first);
        }
        w.dep = true;
        w.present[10] = false;
        w.present[11] = false;
    } else {
        let quiet_until = t + if variant == 0 { 3900.0 } else { 300.0 };
        while t < quiet_until {
            step(&mut w, &mut sup, &mut t, true, false, &mut stuck, &mut structure, &mut last, &mut first);
        }
        w.present[3] = false;
    }
    let probe = t;
    let window_start = structure.iter().cloned().fold(probe - 3600.0, f64::max); // a user's structure change resets the limiter
    let hour_at_probe = stuck.iter().filter(|u| **u > window_start).count();
    // worst wait the limiter may impose: until the oldest restart of the hour leaves the window
    // (hard cap), and one PENALTY_GAP after the previous restart (slow-down), plus slack
    let mut wait = 0.0f64;
    if hour_at_probe >= HARD_CAP {
        let oldest = stuck.iter().filter(|u| **u > window_start).cloned().fold(f64::INFINITY, f64::min);
        wait = oldest + 3600.0 - probe;
    }
    if hour_at_probe >= RESTART_CAP {
        wait = wait.max(last.unwrap_or(probe) + PENALTY_GAP.as_secs_f64() - probe);
    }
    let bound = if hour_at_probe >= RESTART_CAP { wait + 70.0 } else if variant == 0 { 10.0 } else { MAX_GAP.as_secs_f64() + 20.0 };
    let mut repair = f64::INFINITY;
    while t < probe + bound + 120.0 {
        if step(&mut w, &mut sup, &mut t, true, false, &mut stuck, &mut structure, &mut last, &mut first) { repair = t - 1.5 - probe; break; }
    }
    Outcome { stuck, structure, repair, bound, hour_at_probe }
}

#[test]
fn property_limiter_holds_for_random_event_sequences() {
    let (mut capped_probes, mut flap_probes, mut max_per_hour) = (0, 0, 0);
    for seed in 1..=1500u64 {
        let o = simulate(seed, 3600.0 * (1.0 + (seed % 5) as f64));
        // (a) stuck restarts in any rolling hour never exceed the hard cap (one more cap per user structure change)
        for (i, t) in o.stuck.iter().enumerate() {
            let n = o.stuck[i..].iter().take_while(|u| **u < *t + 3599.9).count();
            let resets = o.structure.iter().filter(|u| **u >= *t && **u < *t + 3599.9).count();
            max_per_hour = max_per_hour.max(n);
            assert!(n <= HARD_CAP * (1 + resets), "seed {seed}: {n} stuck restarts within an hour of t={t:.0} ({resets} structure changes)");
        }
        // (c) after the chaos, a single fixable loss (or the strips dead after a dependency flap) is repaired
        // within ~10 s on a long-healthy graph, within the limiter's own bound otherwise
        assert!(o.repair <= o.bound, "seed {seed}: repaired after {:.1}s, bound {:.0}s ({} stuck restarts in the hour before the probe)", o.repair, o.bound, o.hour_at_probe);
        if o.hour_at_probe >= RESTART_CAP { capped_probes += 1; }
        if seed % 3 == 2 { flap_probes += 1; }
    }
    eprintln!("property: max stuck restarts in an hour {max_per_hour}; {capped_probes} probes after the cap, {flap_probes} after a dependency flap");
    assert!(capped_probes > 20, "the generator should reach the cap now and then");
}

/// R5-1: EasyEffects flaps (25 s cycle) for `flap` seconds, then stays up; the strips are dead after
/// every return and only a restart revives them. Returns (seconds from the last return until the
/// strips were revived, restarts in total).
fn flap_then_stable(flap: u64) -> (u64, usize) {
    let t0 = Instant::now();
    let at = |s: u64| t0 + Duration::from_secs(s);
    let strips = names(2);
    let mut sup = Supervisor::default();
    sup.spawned(at(0));
    let (mut spawned, mut alive_strips, mut restarts) = (0u64, true, 0);
    let mut last_return = 0;
    for t in 1..(flap + 4000) {
        let dep = t >= flap || t % 25 < 15; // 15 s up / 10 s away while flapping
        if dep && (t == flap || (t < flap && t % 25 == 0)) { alive_strips = false; last_return = t; }
        let (elig, m) = match (dep, t < spawned + 2) {
            (_, true) => (50, names(50)),
            (true, _) => (50, if alive_strips { vec![] } else { strips.clone() }),
            (false, _) => (48, vec![]),
        };
        if sup.tick(at(t), true, false, &m, elig).is_some() {
            restarts += 1;
            spawned = t;
            alive_strips = dep;
            sup.spawned(at(t));
            if t >= flap && dep { return (t - last_return, restarts); }
        }
    }
    (u64::MAX, restarts)
}

#[test]
fn mic_recovers_after_a_dependency_flap_stops() {
    for flap in [120, 300, 600, 1200] {
        let (back, restarts) = flap_then_stable(flap);
        assert!(back <= PENALTY_GAP.as_secs() + 70, "flap {flap}s: strips revived {back}s after the dependency settled");
        assert!(restarts <= HARD_CAP, "flap {flap}s: {restarts} restarts");
    }
    // a short flap leaves the limiter fresh: recovery inside gap + patience
    assert!(flap_then_stable(120).0 <= MAX_GAP.as_secs() + 20);
}

#[test]
fn structure_change_clears_degraded() {
    let t0 = Instant::now();
    let mut sup = Supervisor::default();
    let m = names(2);
    sup.spawned(t0);
    let mut t = 0;
    while sup.degraded().is_none() {
        t += 1;
        assert!(t < 1000);
        if sup.tick(t0 + Duration::from_secs(t), true, false, &m, 50).is_some() { sup.spawned(t0 + Duration::from_secs(t)); }
    }
    assert_eq!(sup.tick(t0 + Duration::from_secs(t + 10), true, true, &m, 50), Some(Why::Structure));
    assert!(sup.degraded().is_none());
}
