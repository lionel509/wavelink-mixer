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
