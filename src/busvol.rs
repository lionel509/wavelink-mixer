//! Physical output buses (#33): the saved bus gain is the source of truth, not the device.
//!
//! A hardware bus fader *is* the device volume (a3 Sonos: 0 dB = 100 % = the speaker at full).
//! Devices report volumes that are not his: a PipeWire restart brings sinks back at 100 %, a node
//! whose Props are not enumerated yet reads as nothing, an AirPlay sink can read 0 while muted or
//! idle. So the GUI re-applies the saved gain when a node (re)appears and whenever the device drifts
//! shortly after that (or after our own write), and only takes a device value into the config when
//! it is a deliberate external change: stable for `STABLE_S`, outside the settle window, and not a
//! volume of 0 (silence is what mute is for).
//!
//! Mapping (both directions, `config::db_to_vol` / `config::vol_to_db`):
//! wpctl volume `v` (cubic, what `wpctl get-volume` prints) = 10^(dB/60), dB = 60·log10(v);
//! the node's linear channelVolume is v³, so dB is also 20·log10(channelVolume).

use crate::config::{vol_to_db, MIN_DB};

/// After a node appears (or the GUI starts, or we write to it) device values are artefacts for this long.
pub const SETTLE_S: f64 = 5.0;
/// A device value must hold this long before it counts as a deliberate external change.
pub const STABLE_S: f64 = 2.0;
/// dB difference treated as the same level (the GUI rounds to 0.1 dB, wpctl to 6 decimals).
pub const TOL_DB: f64 = 0.15;
/// Re-applies are rate-limited, and give up after this many without the device reading changing.
pub const REAPPLY_EVERY_S: f64 = 1.0;
pub const REAPPLY_TRIES: u32 = 5;
/// wpctl volume below this is silence, not a level
pub const ZERO_VOL: f64 = 1e-6;
/// wpctl volume at or above this reads as a full-scale reset default ...
pub const FULL_VOL: f64 = 0.999;
/// ... and is not adopted this soon after the node appeared, however stable.
pub const ARTEFACT_S: f64 = 30.0;

/// What the device reports: wpctl volume (cubic, 0..) and mute.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Reading {
    pub vol: f64,
    pub mute: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Decision {
    /// Device matches the saved gain/mute.
    InSync,
    /// A difference that is not (yet) a deliberate change: leave it.
    Wait,
    /// Push the saved gain and mute to the device.
    Reapply,
    /// A deliberate external change: take these into the config (None = keep the saved one).
    Adopt { gain_db: Option<f64>, mute: Option<bool> },
}

fn same_db(a: f64, b: f64) -> bool {
    (a <= MIN_DB && b <= MIN_DB) || (a - b).abs() <= TOL_DB
}

/// The dB a device volume is saved as (0.1 dB steps, like the fader).
pub fn adopted_db(vol: f64) -> f64 {
    let db = vol_to_db(vol);
    if db <= MIN_DB { MIN_DB } else { (db * 10.0).round() / 10.0 }
}

/// Everything the adopt-vs-re-apply decision looks at, for one physical bus.
#[derive(Clone, Copy, Debug)]
pub struct Obs {
    pub saved_db: f64,
    pub saved_mute: bool,
    /// None while the node's Props are not enumerated (never read that as a level)
    pub dev: Option<Reading>,
    /// seconds since the node appeared (or this GUI first saw it)
    pub age_s: f64,
    /// seconds since we (or his fader) last wrote to the device
    pub quiet_s: f64,
    /// seconds the device reading has been unchanged
    pub stable_s: f64,
}

/// Adopt vs re-apply for one physical bus.
pub fn decide(o: Obs) -> Decision {
    let Some(d) = o.dev else { return Decision::Wait };
    let mute_ok = d.mute == o.saved_mute;
    // muted means mute: a muted node's volume is not read as a level (raop can report 0 while muted)
    let vol_ok = (d.mute && o.saved_mute) || same_db(vol_to_db(d.vol), o.saved_db);
    if mute_ok && vol_ok {
        return Decision::InSync;
    }
    if o.age_s < SETTLE_S || o.quiet_s < SETTLE_S {
        // just appeared / restarted / we just wrote: whatever it reports is a reset or in flight, not his choice
        return Decision::Reapply;
    }
    if o.stable_s < STABLE_S {
        return Decision::Wait;
    }
    // exactly silent (an AirPlay sink reads 0 while muted or idle); quiet but audible levels below
    // the fader's -60 dB floor are adopted as -∞ like any other level
    let zero = d.vol < ZERO_VOL;
    // a full-scale reading soon after the node appeared is a reset default (AirPlay: 100 % = full blast)
    if !d.mute && d.vol >= FULL_VOL && o.age_s < ARTEFACT_S && !vol_ok {
        return Decision::Reapply;
    }
    // deliberate external change; a muted device's volume and a volume of 0 are never a gain
    let gain_db = (!vol_ok && !d.mute && !zero).then(|| adopted_db(d.vol));
    let mute = (!mute_ok).then_some(d.mute);
    match (gain_db, mute) {
        (None, None) => Decision::Reapply,
        // unmuted at 0 is not "unmute": keep his mute and level
        (None, Some(false)) if zero => Decision::Reapply,
        (g, m) => Decision::Adopt { gain_db: g, mute: m },
    }
}

/// What the GUI should do this poll.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Step {
    Nothing,
    /// Set the device to the saved gain and mute.
    Push,
    Adopt { gain_db: Option<f64>, mute: Option<bool> },
}

/// Per-bus state across graph polls. Times are seconds on any monotonic clock.
#[derive(Clone, Debug, Default)]
pub struct Tracker {
    /// node instance (object.serial, else id); None while absent
    inst: Option<u64>,
    appeared: f64,
    last_write: f64,
    reading: Option<Reading>,
    reading_since: f64,
    last_reapply: f64,
    tries: u32,
    /// we pushed and the device reading has not changed since: that reading is not an external change
    pushed: bool,
}

fn same_reading(a: Option<Reading>, b: Option<Reading>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.mute == b.mute && same_db(vol_to_db(a.vol), vol_to_db(b.vol)),
        (None, None) => true,
        _ => false,
    }
}

impl Tracker {
    /// We (or he, through the fader) just wrote to the device: what it reports next is settling.
    pub fn wrote(&mut self, now: f64) {
        self.last_write = now;
        self.tries = REAPPLY_TRIES;
    }

    /// One graph poll. `node`: (instance, reading) when the device exists.
    pub fn observe(&mut self, now: f64, node: Option<(u64, Option<Reading>)>, saved_db: f64, saved_mute: bool) -> Step {
        let Some((inst, reading)) = node else {
            self.inst = None;
            return Step::Nothing;
        };
        if self.inst != Some(inst) {
            // appeared, re-appeared after a restart, or first seen by this GUI: the config wins
            *self = Tracker { inst: Some(inst), appeared: now, reading, reading_since: now, last_reapply: now, pushed: true, ..Default::default() };
            self.wrote(now);
            return Step::Push;
        }
        if !same_reading(self.reading, reading) {
            self.reading = reading;
            self.reading_since = now;
            self.tries = REAPPLY_TRIES;
            self.pushed = false;
        }
        let o = Obs {
            saved_db,
            saved_mute,
            dev: self.reading,
            age_s: now - self.appeared,
            quiet_s: now - self.last_write,
            stable_s: now - self.reading_since,
        };
        let push = match decide(o) {
            Decision::InSync | Decision::Wait => false,
            Decision::Reapply => true,
            // the device is still on the value we have been overriding: it ignores us, it is not his
            Decision::Adopt { .. } if self.pushed => true,
            Decision::Adopt { gain_db, mute } => return Step::Adopt { gain_db, mute },
        };
        if !push || self.tries == 0 || now - self.last_reapply < REAPPLY_EVERY_S {
            return Step::Nothing;
        }
        self.tries -= 1;
        self.last_reapply = now;
        self.last_write = now;
        self.pushed = true;
        Step::Push
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::db_to_vol;

    fn r(vol: f64, mute: bool) -> Option<Reading> {
        Some(Reading { vol, mute })
    }
    const LATE: f64 = 60.0;
    /// decide() with the node age and our last write both `age` seconds ago
    fn decide(saved_db: f64, saved_mute: bool, dev: Option<Reading>, age: f64, stable: f64) -> Decision {
        super::decide(Obs { saved_db, saved_mute, dev, age_s: age, quiet_s: age, stable_s: stable })
    }

    #[test]
    fn matching_device_is_in_sync() {
        assert_eq!(decide(-37.3, false, r(db_to_vol(-37.3), false), 0.0, 0.0), Decision::InSync);
        // the 6-decimal value wpctl was given still matches
        let v: f64 = format!("{:.6}", db_to_vol(-37.3)).parse().unwrap();
        assert_eq!(decide(-37.3, false, r(v, false), LATE, LATE), Decision::InSync);
        assert_eq!(decide(MIN_DB, false, r(0.0, false), LATE, LATE), Decision::InSync);
    }

    #[test]
    fn reset_to_100_after_appear_is_reapplied_not_adopted() {
        // PipeWire restart: Sonos back at 1.0 (0 dB); saved -37.3
        for q in [0.0, 1.0, 4.9] {
            assert_eq!(decide(-37.3, false, r(1.0, false), q, LATE), Decision::Reapply, "quiet {q}");
        }
    }

    #[test]
    fn zero_after_appear_is_reapplied() {
        assert_eq!(decide(-37.3, false, r(0.0, false), 0.5, 0.5), Decision::Reapply);
        assert_eq!(decide(-37.3, false, r(0.0, true), 0.5, 0.5), Decision::Reapply);
    }

    #[test]
    fn zero_is_never_adopted_as_gain() {
        // stable, long after appear: a volume of 0 is still not a level he picked
        assert_eq!(decide(-37.3, false, r(0.0, false), LATE, LATE), Decision::Reapply);
        // muted at 0, saved unmuted: adopt the mute only, the gain stays
        assert_eq!(decide(-37.3, false, r(0.0, true), LATE, LATE), Decision::Adopt { gain_db: None, mute: Some(true) });
        // saved muted, external unmute at 0: not an unmute to silence, re-apply
        assert_eq!(decide(-37.3, true, r(0.0, false), LATE, LATE), Decision::Reapply);
        // a quiet but audible level below the fader floor is his: -∞
        assert_eq!(decide(-37.3, false, r(0.05, false), LATE, LATE), Decision::Adopt { gain_db: Some(MIN_DB), mute: None });
    }

    #[test]
    fn muted_volume_is_not_read_as_a_level() {
        // both muted: whatever volume the muted node reports, it is in sync
        assert_eq!(decide(-37.3, true, r(0.0, true), LATE, LATE), Decision::InSync);
        assert_eq!(decide(-37.3, true, r(1.0, true), LATE, LATE), Decision::InSync);
        // external mute with some volume: only the mute is adopted
        assert_eq!(decide(-37.3, false, r(0.7, true), LATE, LATE), Decision::Adopt { gain_db: None, mute: Some(true) });
    }

    #[test]
    fn deliberate_external_change_is_adopted_once_stable() {
        let v = db_to_vol(-20.0);
        assert_eq!(decide(-37.3, false, r(v, false), LATE, 1.9), Decision::Wait);
        assert_eq!(decide(-37.3, false, r(v, false), LATE, 2.0), Decision::Adopt { gain_db: Some(-20.0), mute: None });
        // external unmute at a level: both
        assert_eq!(decide(-37.3, true, r(v, false), LATE, LATE), Decision::Adopt { gain_db: Some(-20.0), mute: Some(false) });
        // a stable 100 % long after the node appeared is his (deliberate)
        assert_eq!(decide(-37.3, false, r(1.0, false), LATE, LATE), Decision::Adopt { gain_db: Some(0.0), mute: None });
        // external unmute at the saved level: the mute only
        assert_eq!(decide(-37.3, true, r(db_to_vol(-37.3), false), LATE, LATE), Decision::Adopt { gain_db: None, mute: Some(false) });
    }

    #[test]
    fn full_scale_soon_after_appear_is_an_artefact_however_stable() {
        let o = |age_s: f64| super::decide(Obs { saved_db: -37.3, saved_mute: false, dev: r(1.0, false), age_s, quiet_s: LATE, stable_s: LATE });
        assert_eq!(o(10.0), Decision::Reapply);
        assert_eq!(o(ARTEFACT_S - 0.1), Decision::Reapply);
        assert_eq!(o(ARTEFACT_S), Decision::Adopt { gain_db: Some(0.0), mute: None });
        // a non-full external change at the same age is his
        let v = db_to_vol(-20.0);
        assert_eq!(super::decide(Obs { saved_db: -37.3, saved_mute: false, dev: r(v, false), age_s: 10.0, quiet_s: LATE, stable_s: LATE }),
                   Decision::Adopt { gain_db: Some(-20.0), mute: None });
    }

    #[test]
    fn our_own_write_in_flight_is_reapplied_not_adopted() {
        let v = db_to_vol(-20.0);
        let d = super::decide(Obs { saved_db: -10.0, saved_mute: false, dev: r(v, false), age_s: LATE, quiet_s: 1.0, stable_s: LATE });
        assert_eq!(d, Decision::Reapply);
    }

    #[test]
    fn unknown_props_wait() {
        assert_eq!(decide(-37.3, false, None, 0.0, 0.0), Decision::Wait);
        assert_eq!(decide(-37.3, false, None, LATE, LATE), Decision::Wait);
    }

    #[test]
    fn tracker_pushes_on_appear_and_reappear() {
        let mut t = Tracker::default();
        assert_eq!(t.observe(0.0, None, -37.3, false), Step::Nothing);
        assert_eq!(t.observe(1.0, Some((10, r(1.0, false))), -37.3, false), Step::Push);
        // our push lands
        assert_eq!(t.observe(1.5, Some((10, r(db_to_vol(-37.3), false))), -37.3, false), Step::Nothing);
        // PipeWire restart: gone, then back as a new instance at 100 %
        assert_eq!(t.observe(10.0, None, -37.3, false), Step::Nothing);
        assert_eq!(t.observe(12.0, Some((11, r(1.0, false))), -37.3, false), Step::Push);
        // still at 100 % (the push hasn't landed): re-applied, never adopted, for as long as it stays reset
        let mut pushes = 0;
        for k in 1..40 {
            match t.observe(12.0 + k as f64 * 0.5, Some((11, r(1.0, false))), -37.3, false) {
                Step::Push => pushes += 1,
                Step::Adopt { .. } => panic!("adopted a reset at step {k}"),
                Step::Nothing => {}
            }
        }
        assert!(pushes >= 1 && pushes <= REAPPLY_TRIES, "pushes {pushes}");
    }

    #[test]
    fn tracker_gui_start_with_muted_zero_sonos_reapplies() {
        let mut t = Tracker::default();
        // the GUI's first sight counts as an appearance
        assert_eq!(t.observe(100.0, Some((7, r(0.0, true))), -37.3, false), Step::Push);
        for k in 1..30 {
            let s = t.observe(100.0 + k as f64 * 0.5, Some((7, r(0.0, true))), -37.3, false);
            assert!(!matches!(s, Step::Adopt { gain_db: Some(_), .. }), "adopted volume 0 as a gain at {k}: {s:?}");
        }
    }

    #[test]
    fn tracker_adopts_stable_external_change() {
        let mut t = Tracker::default();
        t.observe(0.0, Some((3, r(db_to_vol(-15.3), false))), -15.3, false);
        assert_eq!(t.observe(10.0, Some((3, r(db_to_vol(-15.3), false))), -15.3, false), Step::Nothing);
        let v = db_to_vol(-6.0);
        assert_eq!(t.observe(10.5, Some((3, r(v, false))), -15.3, false), Step::Nothing);
        assert_eq!(t.observe(11.5, Some((3, r(v, false))), -15.3, false), Step::Nothing);
        assert_eq!(t.observe(12.5, Some((3, r(v, false))), -15.3, false), Step::Adopt { gain_db: Some(-6.0), mute: None });
    }

    #[test]
    fn tracker_his_fader_move_is_not_adopted_back() {
        let mut t = Tracker::default();
        t.observe(0.0, Some((3, r(db_to_vol(-15.3), false))), -15.3, false);
        // he moves the fader to -10 at t=20; the device still reports -15.3 for a while
        t.wrote(20.0);
        for k in 0..8 {
            let s = t.observe(20.0 + k as f64 * 0.5, Some((3, r(db_to_vol(-15.3), false))), -10.0, false);
            assert!(!matches!(s, Step::Adopt { .. }), "adopted the old device value at {k}");
        }
    }

    #[test]
    fn tracker_reapplies_are_bounded() {
        let mut t = Tracker::default();
        t.observe(0.0, Some((3, r(0.0, false))), -37.3, false);
        let pushes = (1..200).filter(|k| t.observe(*k as f64 * 0.5, Some((3, r(0.0, false))), -37.3, false) == Step::Push).count();
        assert!(pushes as u32 <= REAPPLY_TRIES, "pushes {pushes}");
    }
}
