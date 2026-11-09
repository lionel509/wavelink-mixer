//! Breaker round 1 for PR #35 / issue #33 (busvol adopt-vs-re-apply). Test-only, never production code.
//! RED tests are real defects (expected = the safety contract in #33 / the PR text); PROBE tests pin
//! behaviour that is correct today and must stay that way.
use crate::busvol::{decide, Decision, Obs, Reading, Step, Tracker, ARTEFACT_S, SETTLE_S, STABLE_S};
use crate::config::{db_to_vol, vol_to_db, MAX_DB, MIN_DB};
use crate::pw::{Graph, Node};

const SONOS: f64 = -37.3;
const LATE: f64 = 60.0;

fn r(vol: f64, mute: bool) -> Option<Reading> {
    Some(Reading { vol, mute })
}
fn obs(saved_db: f64, saved_mute: bool, dev: Option<Reading>, age_s: f64, quiet_s: f64, stable_s: f64) -> Decision {
    decide(Obs { saved_db, saved_mute, dev, age_s, quiet_s, stable_s })
}
fn is_gain_adopt(s: &Step) -> bool {
    matches!(s, Step::Adopt { gain_db: Some(_), .. })
}
/// Drive the tracker every 0.5 s (the GUI's poll period) from t0 to t1 with a fixed observation;
/// returns every step taken.
fn run(t: &mut Tracker, t0: f64, t1: f64, node: Option<(u64, Option<Reading>)>, saved_db: f64, saved_mute: bool) -> Vec<(f64, Step)> {
    let mut out = vec![];
    let mut now = t0;
    while now <= t1 + 1e-9 {
        out.push((now, t.observe(now, node, saved_db, saved_mute)));
        now += 0.5;
    }
    out
}
fn first_adopt(steps: &[(f64, Step)]) -> Option<(f64, Step)> {
    steps.iter().find(|(_, s)| matches!(s, Step::Adopt { .. })).copied()
}
/// a Sonos that ignores our writes (stays at full scale) and has been around a long time
fn stuck_full_scale_tracker() -> Tracker {
    let mut t = Tracker::default();
    let steps = run(&mut t, 0.0, 45.0, Some((9, r(1.0, false))), SONOS, false);
    assert!(first_adopt(&steps).is_none(), "precondition: a stuck full-scale device is not adopted");
    t
}

// ---------------------------------------------------------------- RED: real defects

/// RED (high). The PR's contract: "None = Props not enumerated" must never be read as a level, and a
/// node's first real reading is an artefact. But the tracker only treats an *instance* change as an
/// appearance: a node that shows up without Props and gets them after the 5 s settle window has its
/// first reading treated as a deliberate external change and adopted 2 s later.
#[test]
fn red_first_reading_after_late_props_is_adopted() {
    let mut t = Tracker::default();
    assert_eq!(t.observe(0.0, Some((4, None)), SONOS, false), Step::Push);
    run(&mut t, 0.5, 6.0, Some((4, None)), SONOS, false);
    // Props enumerate at 6.5 s with a stale WirePlumber-restored level of -30 dB
    let steps = run(&mut t, 6.5, 12.0, Some((4, r(db_to_vol(-30.0), false))), SONOS, false);
    assert!(first_adopt(&steps).is_none(), "first real reading of the node was adopted as his gain: {:?}", first_adopt(&steps));
}

/// RED (high), same hole, Sonos worst case: Props first enumerate after the 30 s artefact window at the
/// reset default 1.0 -> config a3 = 0 dB (100 %, full blast on the next push / restart).
#[test]
fn red_full_scale_first_reading_after_30s_is_adopted_as_0db() {
    let mut t = Tracker::default();
    t.observe(0.0, Some((4, None)), SONOS, false);
    run(&mut t, 0.5, 31.0, Some((4, None)), SONOS, false);
    let steps = run(&mut t, 31.5, 40.0, Some((4, r(1.0, false))), SONOS, false);
    assert!(!steps.iter().any(|(_, s)| is_gain_adopt(s)), "Sonos adopted at 0 dB: {:?}", first_adopt(&steps));
}

/// RED (high). `pushed` (the "device ignores us, it is not his" pin) is cleared by ANY reading change,
/// including one poll without Props (a pw-dump taken while Props are re-enumerated). The device comes
/// back on exactly the value we were overriding and it is adopted.
#[test]
fn red_transient_missing_props_unpins_overridden_full_scale() {
    let mut t = stuck_full_scale_tracker();
    assert_eq!(t.observe(45.5, Some((9, None)), SONOS, false), Step::Nothing);
    let steps = run(&mut t, 46.0, 55.0, Some((9, r(1.0, false))), SONOS, false);
    assert!(!steps.iter().any(|(_, s)| is_gain_adopt(s)), "overridden reset value adopted after a Props blip: {:?}", first_adopt(&steps));
}

/// RED (high). Flapping AirPlay sink: stuck at the reset 1.0 (ignoring us), reads 0 for one poll while
/// idle, then 1.0 again. Same unpinning: the reset value we were fighting becomes a3 = 0 dB.
#[test]
fn red_flapping_idle_zero_unpins_overridden_full_scale() {
    let mut t = stuck_full_scale_tracker();
    t.observe(45.5, Some((9, r(0.0, false))), SONOS, false);
    let steps = run(&mut t, 46.0, 55.0, Some((9, r(1.0, false))), SONOS, false);
    assert!(!steps.iter().any(|(_, s)| is_gain_adopt(s)), "flap 1.0 -> 0 -> 1.0 adopted 0 dB: {:?}", first_adopt(&steps));
}

/// RED (medium). "Muted means mute" goes one step too far: with the bus saved muted, a muted device at
/// full scale is InSync, so the saved volume is never re-applied behind the mute. His unmute (GUI M,
/// SL MkIII CC 51-58) only sends set-mute, so the Sonos plays at 100 % until the next re-apply
/// (>= one 500 ms poll + wpctl). A muted node's volume must not be *adopted*, but it should still be
/// *re-applied*.
#[test]
fn red_full_scale_behind_saved_mute_is_never_reapplied() {
    let d = obs(SONOS, true, r(1.0, true), LATE, LATE, LATE);
    assert_eq!(d, Decision::Reapply, "muted device at 100 % behind a saved mute: {d:?}");
    // also on first sight / right after appearing
    let d = obs(SONOS, true, r(1.0, true), 0.5, 0.5, 0.5);
    assert_eq!(d, Decision::Reapply, "right after appear: {d:?}");
}

/// RED (low). NaN slips past the "volume 0 is never a gain" rule: `d.vol < ZERO_VOL` is false for NaN,
/// so a NaN reading is adopted as -inf (config gain -60). (pw-dump JSON cannot carry NaN today, but
/// decide() is the guard and claims NaN-safety via vol_to_db.)
#[test]
fn red_nan_reading_is_adopted_as_minus_inf() {
    let d = obs(SONOS, false, r(f64::NAN, false), LATE, LATE, LATE);
    assert!(!matches!(d, Decision::Adopt { gain_db: Some(_), .. }), "NaN adopted as a gain: {d:?}");
}

/// RED (low). The +12 dB clamp hides a too-loud device: wpctl 2.0 is +18 dB, vol_to_db clamps it to
/// +12, so a bus saved at +12 reads InSync and the device is never brought back to +12.
#[test]
fn red_device_above_max_db_reads_in_sync_at_max() {
    let d = obs(MAX_DB, false, r(2.0, false), LATE, LATE, LATE);
    assert_ne!(d, Decision::InSync, "device at +18 dB treated as matching a saved +12 dB");
}

// ---------------------------------------------------------------- PROBES: correct today, keep it

#[test]
fn probe_timing_edges_exact() {
    let v = db_to_vol(-20.0);
    // exactly SETTLE_S after appear and after our last write: no longer forced to re-apply
    assert_eq!(obs(SONOS, false, r(v, false), SETTLE_S, SETTLE_S, LATE), Decision::Adopt { gain_db: Some(-20.0), mute: None });
    assert_eq!(obs(SONOS, false, r(v, false), SETTLE_S - 1e-9, LATE, LATE), Decision::Reapply);
    assert_eq!(obs(SONOS, false, r(v, false), LATE, SETTLE_S - 1e-9, LATE), Decision::Reapply);
    // exactly STABLE_S
    assert_eq!(obs(SONOS, false, r(v, false), LATE, LATE, STABLE_S - 1e-9), Decision::Wait);
    assert_eq!(obs(SONOS, false, r(v, false), LATE, LATE, STABLE_S), Decision::Adopt { gain_db: Some(-20.0), mute: None });
    // exactly ARTEFACT_S for full scale
    assert_eq!(obs(SONOS, false, r(1.0, false), ARTEFACT_S - 1e-9, LATE, LATE), Decision::Reapply);
    assert_eq!(obs(SONOS, false, r(1.0, false), ARTEFACT_S, LATE, LATE), Decision::Adopt { gain_db: Some(0.0), mute: None });
    // 0.999 is still "full scale" (FULL_VOL), 0.998 is a level
    assert_eq!(obs(SONOS, false, r(0.999, false), 10.0, LATE, LATE), Decision::Reapply);
    assert!(matches!(obs(SONOS, false, r(0.998, false), 10.0, LATE, LATE), Decision::Adopt { gain_db: Some(_), .. }));
}

/// A deliberate 100 % on the Scarlett (pavucontrol): reverted inside the 30 s after the GUI first saw
/// the node (documented trade-off), adopted after it. NB: busvol cannot tell the Scarlett from the
/// Sonos, so a stable external 100 % on a3 after 30 s is adopted too (see review).
#[test]
fn probe_deliberate_full_scale_on_scarlett() {
    let mut t = Tracker::default();
    let s0 = db_to_vol(-4.2);
    run(&mut t, 0.0, 40.0, Some((1, r(s0, false))), -4.2, false);
    let steps = run(&mut t, 40.5, 45.0, Some((1, r(1.0, false))), -4.2, false);
    assert_eq!(first_adopt(&steps).map(|x| x.1), Some(Step::Adopt { gain_db: Some(0.0), mute: None }));
    let at = first_adopt(&steps).unwrap().0;
    assert!(at >= 40.5 + STABLE_S - 1e-9 && at <= 40.5 + STABLE_S + 0.5, "adopted at {at}");
    // inside 30 s of first sight: re-applied, never adopted
    let mut t = Tracker::default();
    run(&mut t, 0.0, 5.0, Some((1, r(s0, false))), -4.2, false);
    let steps = run(&mut t, 5.5, 25.0, Some((1, r(1.0, false))), -4.2, false);
    assert!(first_adopt(&steps).is_none());
    assert!(steps.iter().any(|(_, s)| *s == Step::Push));
}

/// A PipeWire restart that keeps the serial but changes the cookie is a new instance: push, no adopt.
#[test]
fn probe_cookie_change_without_node_change_is_a_reappearance() {
    let mut g = Graph::default();
    let n = Node { id: 50, serial: 77, name: "test_hw_a3".into(), vol: 1.0, has_vol: true, ..Default::default() };
    g.cookie = 0x1234_5678;
    let i1 = g.instance(&n);
    g.cookie = 0x8765_4321;
    let i2 = g.instance(&n);
    assert_ne!(i1, i2);
    // serial 0 falls back to the id
    let n0 = Node { id: 50, serial: 0, ..n.clone() };
    assert_ne!(g.instance(&n0), g.instance(&Node { id: 51, ..n0.clone() }));

    let mut t = Tracker::default();
    run(&mut t, 0.0, 40.0, Some((i1, r(db_to_vol(SONOS), false))), SONOS, false);
    assert_eq!(t.observe(40.5, Some((i2, r(1.0, false))), SONOS, false), Step::Push);
    let steps = run(&mut t, 41.0, 100.0, Some((i2, r(1.0, false))), SONOS, false);
    assert!(first_adopt(&steps).is_none(), "{:?}", first_adopt(&steps));
}

/// Node appears muted at 0 (AirPlay idle) and then unmutes at a level after the settle window: that is
/// adopted as his (external unmute at a level); unmuting at 0 is not.
#[test]
fn probe_appears_muted_then_unmutes() {
    let mut t = Tracker::default();
    run(&mut t, 0.0, 20.0, Some((7, r(0.0, true))), SONOS, false);
    let steps = run(&mut t, 20.5, 40.0, Some((7, r(0.0, false))), SONOS, false);
    assert!(first_adopt(&steps).is_none(), "unmute at 0 adopted: {:?}", first_adopt(&steps));
    // (a change within 5 s of our last re-apply would be re-applied over: by design)
    let v = db_to_vol(-25.0);
    let steps = run(&mut t, 40.5, 50.0, Some((7, r(v, false))), SONOS, false);
    assert_eq!(first_adopt(&steps).map(|x| x.1), Some(Step::Adopt { gain_db: Some(-25.0), mute: None }));
}

#[test]
fn probe_negative_and_tiny_volumes() {
    // negative (bogus) volume: treated like 0, never a gain
    assert_eq!(obs(SONOS, false, r(-0.5, false), LATE, LATE, LATE), Decision::Reapply);
    assert_eq!(obs(SONOS, false, r(1e-7, false), LATE, LATE, LATE), Decision::Reapply);
    // infinite volume would clamp to +12; still never louder than MAX_DB in the config
    if let Decision::Adopt { gain_db: Some(g), .. } = obs(SONOS, false, r(f64::INFINITY, false), LATE, LATE, LATE) {
        assert!(g <= MAX_DB);
    }
    assert_eq!(vol_to_db(f64::INFINITY), MAX_DB);
    assert_eq!(db_to_vol(f64::NEG_INFINITY), 0.0);
    assert!(db_to_vol(f64::INFINITY) <= db_to_vol(MAX_DB) + 1e-12);
    assert_eq!(vol_to_db(0.0), MIN_DB);
}

/// His fader move while the device is mid-restart: nothing ever adopts the old value, and a full GUI
/// session of re-applies stays bounded.
#[test]
fn probe_wrote_during_reset_never_adopts() {
    let mut t = Tracker::default();
    t.observe(0.0, Some((3, r(1.0, false))), SONOS, false);
    for k in 0..20 {
        let now = 0.5 + k as f64 * 0.5;
        t.wrote(now);
        let s = t.observe(now, Some((3, r(1.0, false))), -30.0, false);
        assert!(!matches!(s, Step::Adopt { .. }), "{s:?} at {now}");
    }
    let steps = run(&mut t, 10.5, 200.0, Some((3, r(1.0, false))), -30.0, false);
    assert!(first_adopt(&steps).is_none());
    assert!(steps.iter().filter(|(_, s)| *s == Step::Push).count() <= 5);
}

// ================================================================ Round 2 (91cf4a8 + ef80635)

use crate::busvol::{write_db, PIN_CLEAR_S, TOL_DB};
use crate::config::{Bus, BusKind, RAOP_MAX_DB};
use crate::dash::{db_to_pos, pos_to_db};

fn sonos_bus(target: &str, gain_db: f64, max_db: Option<f64>) -> Bus {
    let mut b = Bus::new("a3".into(), "Sonos".into(), "#fff".into(), BusKind::Hardware { target: target.into() });
    b.gain_db = gain_db;
    b.max_db = max_db;
    b
}

/// Round-1 reds, harder variants (the fixes must not be fitted to the exact sequences of round 1).
#[test]
fn r2_probe_round1_fixes_hold_on_variants() {
    // R1/R2: Props arrive late, muted at full scale, after 40 s; then unmute at full scale
    let mut t = Tracker::default();
    t.observe(0.0, Some((4, None)), SONOS, false);
    run(&mut t, 0.5, 40.0, Some((4, None)), SONOS, false);
    let mut steps = run(&mut t, 40.5, 50.0, Some((4, r(1.0, true))), SONOS, false);
    steps.extend(run(&mut t, 50.5, 70.0, Some((4, r(1.0, false))), SONOS, false));
    assert!(!steps.iter().any(|(_, s)| is_gain_adopt(s)), "{:?}", first_adopt(&steps));
    // R3: a 3-poll (1.5 s) Props gap, twice
    let mut t = stuck_full_scale_tracker();
    let mut steps = run(&mut t, 45.5, 46.5, Some((9, None)), SONOS, false);
    steps.extend(run(&mut t, 47.0, 50.0, Some((9, r(1.0, false))), SONOS, false));
    steps.extend(run(&mut t, 50.5, 51.5, Some((9, None)), SONOS, false));
    steps.extend(run(&mut t, 52.0, 70.0, Some((9, r(1.0, false))), SONOS, false));
    assert!(!steps.iter().any(|(_, s)| is_gain_adopt(s)), "{:?}", first_adopt(&steps));
    // R4: flap to idle 0 for 1.5 s (just under STABLE_S), then to 0.9995 (still "full scale", within TOL)
    let mut t = stuck_full_scale_tracker();
    let mut steps = run(&mut t, 45.5, 47.0, Some((9, r(0.0, false))), SONOS, false);
    steps.extend(run(&mut t, 47.5, 70.0, Some((9, r(0.9995, false))), SONOS, false));
    assert!(!steps.iter().any(|(_, s)| is_gain_adopt(s)), "{:?}", first_adopt(&steps));
}

/// The override pin clears after exactly PIN_CLEAR_S of InSync, not before (Scarlett: no ceiling).
/// 4.5 s in sync then a jump back to the overridden 100 %: still refused. 5.0 s: cleared, and a held
/// 100 % is then a deliberate external change (adopted after STABLE_S) by design.
#[test]
fn r2_probe_pin_clears_at_exactly_5s_in_sync() {
    fn setup() -> Tracker {
        let mut t = Tracker::default();
        let s0 = db_to_vol(-4.2);
        run(&mut t, 0.0, 39.5, Some((1, r(s0, false))), -4.2, false);
        // he moves the fader to -10 at 40 while the device resets to 100 %: we push over 1.0 (pin = 1.0)
        t.wrote(40.0);
        assert_eq!(t.observe(40.0, Some((1, r(1.0, false))), -10.0, false), Step::Push);
        t
    }
    let v10 = db_to_vol(-10.0);
    for (hold, adopt_expected) in [(4.5, false), (PIN_CLEAR_S - 0.5, false), (PIN_CLEAR_S, true)] {
        let mut t = setup();
        // device lands at -10 at 41.0 and stays in sync for `hold` seconds (polls 41.0 ..= 41.0+hold)
        run(&mut t, 41.0, 41.0 + hold, Some((1, r(v10, false))), -10.0, false);
        let steps = run(&mut t, 41.5 + hold, 60.0, Some((1, r(1.0, false))), -10.0, false);
        let a = first_adopt(&steps);
        assert_eq!(a.is_some(), adopt_expected, "hold {hold}: {a:?}");
        if let Some((at, s)) = a {
            assert_eq!(s, Step::Adopt { gain_db: Some(0.0), mute: None });
            assert!((at - (41.5 + hold + STABLE_S)).abs() < 1e-9, "adopted at {at}");
        }
    }
    // with the AirPlay ceiling the same 5 s clear can never let 100 % in
    let mut t = Tracker::default();
    t.set_ceiling(RAOP_MAX_DB);
    run(&mut t, 0.0, 39.5, Some((1, r(db_to_vol(SONOS), false))), SONOS, false);
    run(&mut t, 40.0, 50.0, Some((1, r(db_to_vol(SONOS), false))), SONOS, false);
    let steps = run(&mut t, 50.5, 120.0, Some((1, r(1.0, false))), SONOS, false);
    assert!(first_adopt(&steps).is_none(), "{:?}", first_adopt(&steps));
}

/// Deterministic fuzz of the tracker: random readings (None, 0, muted, NaN, > +12, full scale, levels
/// above/below the ceiling), random instance changes and fader writes. Invariants on every step:
/// never adopt a gain above the ceiling, never adopt from a NaN / zero / > +12 / muted reading,
/// never adopt within SETTLE_S of an appearance, never adopt full scale within ARTEFACT_S.
#[test]
fn r2_probe_fuzz_tracker_invariants() {
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut rnd = |n: u64| {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (seed >> 33) % n
    };
    let pool = [
        None, r(0.0, false), r(0.0, true), r(1.0, false), r(1.0, true), r(f64::NAN, false), r(2.0, false),
        r(db_to_vol(-10.0), false), r(db_to_vol(-25.0), false), r(db_to_vol(SONOS), false), r(db_to_vol(SONOS), true),
        r(0.9995, false), r(-0.3, false),
    ];
    for ceiling in [RAOP_MAX_DB, -30.0, MAX_DB] {
        for run_no in 0..200 {
            let mut t = Tracker::default();
            t.set_ceiling(ceiling);
            let (mut inst, mut appeared, mut saved, mut saved_mute) = (1u64, 0.0, SONOS.min(ceiling), false);
            let mut cur = pool[rnd(pool.len() as u64) as usize];
            let mut had = cur.is_some();
            let mut now = 0.0;
            t.observe(now, Some((inst, cur)), saved, saved_mute);
            for _ in 0..400 {
                now += 0.5;
                match rnd(40) {
                    0 => { inst += 1; appeared = now; had = false; }
                    1 | 2 => { saved = write_db(pos_to_db(rnd(1001) as f64 / 1000.0), ceiling); t.wrote(now); }
                    3 => { saved_mute = !saved_mute; t.wrote(now); }
                    4..=9 => { cur = pool[rnd(pool.len() as u64) as usize]; }
                    _ => {}
                }
                if !had && cur.is_some() { had = true; appeared = now; }
                let s = t.observe(now, Some((inst, cur)), saved, saved_mute);
                if let Step::Adopt { gain_db, mute } = s {
                    let d = cur.expect("adopt without a reading");
                    let ctx = format!("ceil {ceiling} run {run_no} t {now} saved {saved} reading {d:?} -> {s:?}");
                    assert!(now - appeared >= SETTLE_S, "adopt inside settle: {ctx}");
                    if let Some(g) = gain_db {
                        assert!(g <= ceiling + TOL_DB, "above ceiling: {ctx}");
                        assert!(!d.mute && d.vol.is_finite() && d.vol >= 1e-6 && d.vol <= db_to_vol(MAX_DB) * 1.000001, "bad source: {ctx}");
                        assert!(!(d.vol >= 0.999 && now - appeared < ARTEFACT_S), "full scale inside 30 s: {ctx}");
                        saved = g.min(ceiling);
                    }
                    if let Some(m) = mute { saved_mute = m; }
                }
            }
        }
    }
}

/// Ceiling: no fader position (drag, scroll, double-click, SL MkIII value/127) writes above it.
#[test]
fn r2_probe_write_db_never_exceeds_ceiling() {
    for ceil in [MIN_DB, -37.3, RAOP_MAX_DB, -0.1, 0.0, 6.0, MAX_DB] {
        for k in 0..=1000 {
            let db = write_db(pos_to_db(k as f64 / 1000.0), ceil);
            assert!(db <= ceil + 1e-12, "pos {k}/1000 ceil {ceil} -> {db}");
        }
        for cc in 0..=127u8 {
            assert!(write_db(pos_to_db(cc as f64 / 127.0), ceil) <= ceil + 1e-12);
        }
        assert!(write_db(pos_to_db(db_to_pos(0.0)), ceil) <= ceil);
        assert!(write_db(pos_to_db(f64::NAN), ceil) <= ceil);
        assert!(write_db(f64::INFINITY, ceil) <= ceil);
    }
}

/// write_db(-inf) returns -inf, which serde_json writes as `null` -> the config no longer parses ->
/// Config::load backs it up and resets to defaults. Not reachable from the slider today (pos_to_db
/// floors at -60), so a low-severity robustness probe: it must stay unreachable.
#[test]
fn r2_probe_write_db_negative_infinity_not_reachable_from_slider() {
    for k in 0..=1000 {
        assert!(pos_to_db(k as f64 / 1000.0).is_finite());
    }
    assert!(pos_to_db(-1.0).is_finite() && pos_to_db(2.0).is_finite());
}

/// Ceiling defaults and hand-edited configs: every push path uses effective_db().
#[test]
fn r2_probe_ceiling_hand_edits() {
    let real = "raop_sonos_lionels_room";
    // his live target name matches the raop_ default
    assert_eq!(sonos_bus(real, SONOS, None).ceiling_db(), RAOP_MAX_DB);
    // hand-edited gain above the ceiling: pushed/shown at the ceiling
    for g in [0.0, 12.0, 40.0, f64::INFINITY] {
        assert_eq!(sonos_bus(real, g, None).effective_db(), RAOP_MAX_DB, "gain {g}");
    }
    assert_eq!(sonos_bus(real, f64::NAN, None).effective_db(), MIN_DB);
    // max_db NaN / inf / absurd -> destination default or the fader range, never louder than +12
    assert_eq!(sonos_bus(real, 0.0, Some(f64::NAN)).ceiling_db(), RAOP_MAX_DB);
    assert_eq!(sonos_bus(real, 0.0, Some(f64::INFINITY)).ceiling_db(), RAOP_MAX_DB);
    assert_eq!(sonos_bus(real, 0.0, Some(-500.0)).ceiling_db(), MIN_DB);
    // JSON: max_db null or missing = default; a string makes the whole config unreadable (backed up, defaults)
    let j = |m: &str| format!(r##"{{"id":"a3","name":"Sonos","color":"#fff","kind":{{"type":"hardware","target":"{real}"}},"gain_db":0.0,"mute":false{m}}}"##);
    assert_eq!(serde_json::from_str::<Bus>(&j(r#","max_db":null"#)).unwrap().effective_db(), RAOP_MAX_DB);
    assert!(serde_json::from_str::<Bus>(&j(r#","max_db":"-30""#)).is_err());
    // the default raop-discover name also matches
    assert_eq!(sonos_bus("raop_sink.Sonos-7828CAB27EEC.local.192.168.1.64.7000", 0.0, None).ceiling_db(), RAOP_MAX_DB);
}

/// Known gap (documented, not a defect of the code as specified): the default ceiling is keyed on the
/// node *name*. A RAOP sink renamed by a WirePlumber/raop-discover rule (node.name = "sonos_room") or
/// different case gets no ceiling: +12 dB, i.e. the speaker at 100 % and beyond.
#[test]
fn r2_probe_renamed_raop_destination_has_no_ceiling() {
    assert_eq!(sonos_bus("sonos_lionels_room", 0.0, None).ceiling_db(), MAX_DB);
    assert_eq!(sonos_bus("RAOP_sonos", 0.0, None).ceiling_db(), MAX_DB);
    assert_eq!(sonos_bus("sonos_lionels_room", 0.0, None).effective_db(), 0.0);
}

/// Model of the slider handler in ui.rs (`connect_value_changed`, 91cf4a8): it snaps the slider back
/// whenever the written dB sits below the position, which is meant for the ceiling but also fires for
/// the 0.1 dB rounding and the unity snap. A keyboard Up step (step_increment 0.001) is then undone:
/// the fader cannot be raised with the arrow keys from 0 dB or from most of -10..0 dB.
#[test]
fn red_r2_snap_back_also_undoes_rounding_keyboard_up_is_stuck() {
    // the handler's arithmetic (fix round 3: the snap-back decision is crate::busvol::snap_back, called by ui.rs)
    fn key_up(pos: f64, ceil: f64) -> f64 {
        let v = pos + 0.001;
        let db = write_db(pos_to_db(v), ceil);
        crate::busvol::snap_back(v, db, ceil).unwrap_or(v)
    }
    let mut stuck = vec![];
    for start in [0.0, -10.0, -6.0, -3.0, -30.0] {
        let mut pos = db_to_pos(start);
        for _ in 0..20 {
            pos = key_up(pos, MAX_DB);
        }
        let db = write_db(pos_to_db(pos), MAX_DB);
        if db < start + 0.5 {
            stuck.push(format!("{start} dB -> {db} dB"));
        }
    }
    assert!(stuck.is_empty(), "20 x Up on a +12 dB-ceiling fader is stuck: {stuck:?}");
}

/// Ordered wpctl pair: values are always finite plain decimals (no shell metacharacters reach sh; the
/// script only expands "$1".."$6"), and an unmute never precedes its volume.
#[test]
fn r2_probe_volume_mute_cmds_are_safe() {
    for db in [MIN_DB, -37.3, RAOP_MAX_DB, 0.0, MAX_DB, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let v = db_to_vol(db);
        for mute in [false, true] {
            let c = crate::pw::volume_mute_cmds(u64::MAX, v, mute);
            assert_eq!(c.len(), 2);
            let vol = c.iter().find(|x| x[0] == "set-volume").unwrap();
            assert!(vol[2].chars().all(|ch| ch.is_ascii_digit() || ch == '.'), "{vol:?}");
            assert!(vol[2].parse::<f64>().unwrap() <= db_to_vol(MAX_DB) + 1e-6);
            assert_eq!(c[if mute { 0 } else { 1 }][0], "set-mute");
        }
    }
}
