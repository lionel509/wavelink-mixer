//! `wavelink-mixer remote` (#37): the GTK console for a second computer. It is a client of the engine's protocol
//! (State/Delta/Meters in, Command out) built from the same `dash` widgets and `ui::CSS` as the main window.
//! On a Mac it also registers itself as a host and drives the Mac's own output volume (`macvol`).
//! SCAFFOLD: signatures and types only (U4 fills them in).
#![allow(dead_code)]
use crate::macvol::Volume;
use wavelink_proto::{Delta, Host};

/// `wavelink-mixer remote [ADDR] [--label NAME] [--no-host] [--save]`. Returns the process exit code.
pub fn run(_args: Vec<String>) -> i32 {
    todo!("U4: console run")
}

/// Host id from a hostname: lowercase, anything not `[a-z0-9]` becomes `-`, runs collapsed, `-` trimmed.
pub fn sanitize_host_id(_s: &str) -> String {
    todo!("U4: sanitize_host_id")
}

/// A host strip's edit, sent as `SetHostVolume` / `SetHostMute`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostEdit {
    Volume(u8),
    Mute(bool),
}

/// The edit a delta asks of host `my_id`, if it is for us (volume or mute). Deltas for other hosts give None.
pub fn host_edit_for(_d: &Delta, _my_id: &str) -> Option<HostEdit> {
    todo!("U4: host_edit_for")
}

/// One MacBook strip in the OUTPUTS row.
pub struct HostStrip {
    pub root: gtk::Box,
}

impl HostStrip {
    /// Show a new state of this host. OFFLINE: insensitive, readout "OFFLINE".
    pub fn set(&self, _h: &Host) {
        todo!("U4: HostStrip::set")
    }
}

/// Build the host strip (`.strip.bus.host`): label, 0..=100 fader with marks 0/25/50/75/100, "<n>%" readout, M toggle.
pub fn host_strip(_h: &Host, _on_edit: std::rc::Rc<dyn Fn(HostEdit)>) -> HostStrip {
    todo!("U4: host_strip")
}

/// Fader marks, percent.
pub const PCT_MARKS: [u8; 5] = [0, 25, 50, 75, 100];

/// Fader position of a percent value: 0 -> 0.0, 100 -> 1.0, linear.
pub fn fader_pos(_p: u8) -> f64 {
    todo!("U4: fader_pos")
}

/// Echo guard between the Mac's volume and the engine (#37). Our own CoreAudio writes come back as watch wakes;
/// those must not be reported as user changes.
pub struct Echo {}

impl Echo {
    pub fn new() -> Echo {
        todo!("U4: Echo::new")
    }
    /// An engine delta wants `want` on this Mac. Some = write it; None = CoreAudio already reads it (no write).
    pub fn on_delta(&mut self, _current: Option<Volume>, _want: Volume) -> Option<Volume> {
        todo!("U4: Echo::on_delta")
    }
    /// A CoreAudio watch wake read `current`. Some = report it to the engine; None = it was our own write.
    pub fn on_watch(&mut self, _current: Volume) -> Option<Volume> {
        todo!("U4: Echo::on_watch")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wavelink_proto::Delta;

    #[test]
    fn sanitize_host_id_lowercases_and_strips() {
        assert_eq!(sanitize_host_id("Lionel's MacBook Pro"), "lionel-s-macbook-pro");
        assert_eq!(sanitize_host_id("  "), "");
    }

    #[test]
    fn echo_swallows_our_own_write_once_and_reports_external_changes() {
        let v = |volume: u8| Volume { volume, mute: false };
        let mut e = Echo::new();
        // the engine wants 30 while the Mac reads 50: write it
        assert_eq!(e.on_delta(Some(v(50)), v(30)), Some(v(30)));
        // the wake our own write caused is swallowed once
        assert_eq!(e.on_watch(v(30)), None);
        // a volume key press on the Mac is reported
        assert_eq!(e.on_watch(v(80)), Some(v(80)));
        // the engine wants what CoreAudio already reads: no write (keeps two Macs from looping)
        assert_eq!(e.on_delta(Some(v(80)), v(80)), None);
    }

    #[test]
    fn delta_for_other_host_is_ignored() {
        let d = Delta::HostVolume { host: "studio".into(), volume: 40 };
        assert_eq!(host_edit_for(&d, "macbook"), None);
        let mine = Delta::HostVolume { host: "macbook".into(), volume: 40 };
        assert_eq!(host_edit_for(&mine, "macbook"), Some(HostEdit::Volume(40)));
        let mute = Delta::HostMute { host: "macbook".into(), mute: true };
        assert_eq!(host_edit_for(&mute, "macbook"), Some(HostEdit::Mute(true)));
    }

    #[test]
    fn pct_marks_and_fader_pos() {
        assert_eq!(PCT_MARKS, [0, 25, 50, 75, 100]);
        assert_eq!(fader_pos(0), 0.0);
        assert_eq!(fader_pos(25), 0.25);
        assert_eq!(fader_pos(50), 0.5);
        assert_eq!(fader_pos(100), 1.0);
    }
}
