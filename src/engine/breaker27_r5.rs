use super::*;

const TICK: f64 = 1.2;
const AFTER_RESTART: f64 = 1.5;
const N: usize = 59;

fn all(n: usize) -> Vec<String> { (0..n).map(|i| format!("n{i}")).collect() }

/// Drive the real Supervisor. `state(t, since_spawn, restarts) -> (alive, missing, eligible)`.
fn drive(total: f64, mut state: impl FnMut(f64, f64, usize) -> (bool, Vec<String>, usize)) -> (Vec<(f64, Why)>, Supervisor) {
    let base = Instant::now();
    let at = |t: f64| base + Duration::from_secs_f64(t);
    let mut sup = Supervisor::default();
    let mut started = -60.0f64;
    let mut ev = vec![];
    let mut t = 0.0;
    while t < total {
        let (alive, m, e) = state(t, t - started, ev.len());
        if let Some(why) = sup.tick(at(t), alive, false, &m, e) {
            if started > -59.0 { ev.push((t, why)); }
            started = t; sup.spawned(at(t));
            t += AFTER_RESTART; continue;
        }
        t += TICK;
    }
    (ev, sup)
}

// ---------------- red ----------------

/// R5-F1: the hard cap gives up (degraded) on the 11th loss, but degraded then only retries an
/// hour after the LAST restart, not when the rolling cap frees. Ten separate losses 5 min
/// apart, each fixed by one restart; the 11th (t=3000, also restart-fixable) waits until
/// ~6300 s although the cap window frees at ~3600 s: ~45 min of outage beyond the cap itself.
#[test]
fn r5_degraded_outlives_hard_cap_window() {
    let losses: Vec<f64> = (0..11).map(|k| 1.0 + 300.0 * k as f64).collect();
    let mut fixed_by: Vec<Option<usize>> = vec![None; losses.len()];
    let (ev, sup) = drive(7000.0, |t, since, r| {
        if since < 1.5 { return (true, all(N), N); }
        for (k, l) in losses.iter().enumerate() {
            if t >= *l && (k + 1 == losses.len() || t < losses[k + 1]) {
                let r0 = *fixed_by[k].get_or_insert(r);
                return (true, if r == r0 { vec![format!("route_{k}")] } else { vec![] }, N);
            }
        }
        (true, vec![], N)
    });
    let first_free = ev.iter().filter(|e| e.1 == Why::Stuck).next().map(|e| e.0 + 3600.0).unwrap();
    let fix = ev.iter().find(|e| e.0 >= losses[10]).map(|e| e.0).unwrap_or(f64::INFINITY);
    assert!(fix <= first_free + 70.0, "11th loss at {:.0}: cap frees at {first_free:.0}, repaired at {fix:.0} (degraded {:?}); restarts {:?}", losses[10], sup.degraded(), ev.iter().map(|e| e.0.round()).collect::<Vec<_>>());
}

/// R5-F2: `max_eligible` only ever grows, so once a dependency has been seen and then goes
/// away for good (EasyEffects stopped, a USB mic unplugged) `eligible < max_eligible` forever
/// and no clock is ever pruned. A route that blinked out at t=200 keeps its t=200 clock; when
/// it blinks again at t=1000 for ~2 s, the 5 s grace is skipped and the whole graph is
/// restarted at once (all audio cut) for a node that was coming back by itself.
#[test]
fn r5_stale_clock_after_permanent_dependency_loss() {
    let (ev, _) = drive(1100.0, |t, since, _| {
        if since < 1.5 { return (true, all(N), N); }
        let e = if t < 100.0 { N } else { N - 2 }; // EasyEffects stops at t=100 and stays off
        let blink = (200.0..202.5).contains(&t) || (1000.0..1002.5).contains(&t);
        (true, if blink { vec!["route_spotify_a1".into()] } else { vec![] }, e)
    });
    assert!(ev.is_empty(), "a 2 s blink (< 5 s grace) restarted the whole graph at {:?}", ev);
}

// ---------------- probes ----------------

/// A stronger random world than property_limiter_holds_for_random_event_sequences: adds the
/// round-1/4 spawn-phase-locked faults (nodes that appear then die `fuse` s after every spawn),
/// and checks LIVENESS during the chaos, not only after 65 healthy minutes: a fixable outage
/// must end within 20 min unless the hard cap was reached in the hour before.
#[test]
fn probe_fuzz_phase_locked_faults_liveness() {
    struct R(u64);
    impl R { fn n(&mut self) -> u64 { self.0 ^= self.0 >> 12; self.0 ^= self.0 << 25; self.0 ^= self.0 >> 27; self.0.wrapping_mul(0x2545F4914F6CDD1D) } fn b(&mut self, k: u64) -> u64 { self.n() % k } }
    let mut worst = 0.0f64;
    let mut worst_seed = 0;
    let mut max_hour = 0;
    for seed in 1..=400u64 {
        let mut r = R(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1);
        let fuse_node = r.b(12) as usize;
        let fuse = [0.0, 3.0, 12.0, 31.0, 35.0, 62.0][r.b(6) as usize]; // 0 = no fuse
        let bad = if r.b(3) == 0 { Some(r.b(12) as usize) } else { None };
        let base = Instant::now(); let at = |t: f64| base + Duration::from_secs_f64(t);
        let mut sup = Supervisor::default();
        let mut present = [true; 12];
        let (mut t, mut spawned) = (0.0f64, -100.0f64);
        let mut stuck: Vec<f64> = vec![];
        let mut outage: [Option<f64>; 12] = [None; 12];
        let mut first = true;
        while t < 3.0 * 3600.0 {
            if r.b(40) == 0 { let i = r.b(12) as usize; present[i] = false; }
            if fuse > 0.0 && t - spawned >= fuse { present[fuse_node] = false; }
            let young = t - spawned < 2.0;
            let miss: Vec<String> = (0..12).filter(|i| young || !present[*i]).map(|i| format!("r{i}")).collect();
            for i in 0..12 {
                let fixable = !present[i] && Some(i) != bad && !young;
                match (fixable, outage[i]) {
                    (true, None) => outage[i] = Some(t),
                    (false, Some(s)) => {
                        let d = t - s;
                        let capped = stuck.iter().filter(|u| **u > s - 3600.0 && **u <= t).count() >= HARD_CAP;
                        if !capped && d > worst { worst = d; worst_seed = seed; }
                        outage[i] = None;
                    }
                    _ => {}
                }
            }
            if let Some(why) = sup.tick(at(t), true, false, &miss, 12) {
                if !first && why == Why::Stuck { stuck.push(t); }
                first = false;
                spawned = t; sup.spawned(at(t));
                for i in 0..12 { present[i] = Some(i) != bad; }
                t += AFTER_RESTART; continue;
            }
            t += TICK;
        }
        for (i, s) in stuck.iter().enumerate() { max_hour = max_hour.max(stuck[i..].iter().take_while(|u| **u < s + 3599.9).count()); }
    }
    eprintln!("fuzz: worst uncapped fixable outage {worst:.0}s (seed {worst_seed}), max stuck/h {max_hour}");
    assert!(max_hour <= HARD_CAP);
    assert!(worst <= 1200.0, "seed {worst_seed}: a fixable outage lasted {worst:.0}s without the hard cap being reached");
}

/// A crash every 61 s (dead child, exempt from the cap): restarted each time (60 s gap
/// ceiling), never degraded. Restarting is the only way back to audio, so this is accepted.
#[test]
fn probe_crash_every_61s_keeps_restarting() {
    let (ev, sup) = drive(3600.0, |_, since, _| (since < 61.0, if since < 1.5 { all(N) } else { vec![] }, N));
    assert!(ev.iter().all(|e| e.1 == Why::Dead) && ev.len() >= 50 && sup.degraded().is_none(), "{} restarts", ev.len());
}
