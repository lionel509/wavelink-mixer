//! Mixer window: input strips (gain, EQ, FX, routes), buses, and an apps panel.
#[path = "stems_view.rs"]
mod stems_view;
use crate::config::{self, db_to_vol, fmt_db, names, Bus, BusKind, Config, Input, InputKind, Route, FX_PRESETS};
use crate::engine::bus_fader_node;
use crate::dash::{db_to_pos, hex_rgb, Hit, pos_to_db, screw_row, Analyzer, Fft, GrMeter, Knob, Lamp, LedMeter, Patch, PatchBay, Spectrum, Vu};
use std::sync::{Arc, Mutex};
use std::collections::HashMap;
use crate::pw::{self, Graph, Side};
use adw::prelude::*;
use gtk::{gdk, glib};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

const CSS: &str = r#"
window, .main { background: #0e0e10; color: #e6e6e8; }
.strip {
  background-color: #2b2c30;
  background-image: repeating-linear-gradient(to bottom, rgba(255,255,255,.025) 0px, rgba(255,255,255,.025) 1px, rgba(0,0,0,.03) 1px, rgba(0,0,0,.03) 3px),
                    linear-gradient(to bottom, #34353a, #232428);
  border-radius: 6px; padding: 4px 8px; border: 1px solid #0b0b0c;
  box-shadow: inset 0 1px 0 rgba(255,255,255,.12), inset 0 -1px 0 rgba(0,0,0,.5), 0 4px 10px rgba(0,0,0,.6); }
.strip.bus { background-image: repeating-linear-gradient(to bottom, rgba(255,255,255,.025) 0px, rgba(255,255,255,.025) 1px, rgba(0,0,0,.03) 1px, rgba(0,0,0,.03) 3px),
                    linear-gradient(to bottom, #3a3b40, #26272b); }
.name { font-weight: 800; font-size: 13px; letter-spacing: 1px; color: #f0f0f2; text-shadow: 0 1px 0 #000; }
.small { font-size: 9px; font-weight: 700; letter-spacing: 1.5px; color: #a0a0a8; }
.db { font-size: 11px; font-feature-settings: "tnum"; color: #b8b8bd; }
.colorbar { min-height: 2px; border-radius: 1px; }
.section { font-size: 11px; font-weight: 700; letter-spacing: 2px; color: #5e5e66; }
scale.horizontal trough { min-height: 3px; background: #2a2a30; }
scale.horizontal slider { min-width: 12px; min-height: 12px; margin: -5px; background: #d8d8dc; border: none; box-shadow: none; }
scale.horizontal highlight { background: #6e6e76; }
button.mute { min-width: 32px; min-height: 24px; padding: 0; font-weight: 700; font-size: 11px; border-radius: 6px; background: #24242a; color: #b8b8bd; }
button.mute:checked { background: #e5291a; color: white; }
button.route { min-width: 36px; min-height: 18px; padding: 0 2px; font-weight: 800; font-size: 9px; border-radius: 3px; background: #15151a; color: #6c6c74; border: 1px solid #000; box-shadow: inset 0 1px 2px rgba(0,0,0,.8); }
button.eqbtn { min-height: 24px; min-width: 0; padding: 0 4px; font-size: 11px; font-weight: 600; border-radius: 6px; background: #222227; color: #b8b8bd; }
button.flatname { padding: 0 6px; min-height: 24px; }
.apps { background: #141417; border-radius: 14px; padding: 10px; }
.apps row { background: transparent; padding: 6px 2px; }
.sub { color: #7c7c84; font-size: 12px; }
.rackunit {
  background-color: #2b2c30; min-height: 86px; padding: 6px 4px; border-radius: 4px; border: 1px solid #0b0b0c;
  background-image: repeating-linear-gradient(to bottom, rgba(255,255,255,.03) 0px, rgba(255,255,255,.03) 1px, rgba(0,0,0,.03) 1px, rgba(0,0,0,.03) 3px),
                    linear-gradient(to bottom, #3a3b40, #232428);
  box-shadow: inset 0 1px 0 rgba(255,255,255,.12), 0 2px 5px rgba(0,0,0,.6); }
.ear { min-width: 14px; border-radius: 3px;
  background-image: radial-gradient(circle at 50% 18%, #c8c8cc 0px, #6a6a70 3px, transparent 4px),
                    radial-gradient(circle at 50% 82%, #c8c8cc 0px, #6a6a70 3px, transparent 4px); }
button.power { min-height: 20px; padding: 0 8px; font-size: 10px; font-weight: 800; border-radius: 3px;
  background: #151518; color: #6a6a72; border: 1px solid #000; }
button.power:checked { color: #ffb347; text-shadow: 0 0 6px rgba(255,170,60,.9); background: #1a1712; }
.btnlabel { font-size: 11px; font-weight: 700; }
.rackrow { padding: 2px 0; }
.nameplate { background-image: linear-gradient(to bottom, #3a3b40, #232428); border: 1px solid #0b0b0c; border-radius: 4px; padding: 8px 10px; }
button.lights { padding: 1px 2px; min-height: 0; }
label.led { font-size: 8px; font-weight: 800; letter-spacing: .5px; padding: 1px 3px; border-radius: 2px;
  color: #55555c; background: #141417; border: 1px solid #000; }
label.led.on { color: #1a1206; background: #ffb347; box-shadow: 0 0 6px rgba(255,170,60,.7); }
.mini-ana { border-radius: 3px; border: 1px solid #000; }
/* ---- Player page (Spotify-style) ---- */
.sp { background: #000; color: #fff; }
.sp label { color: #fff; }
.sp .sp-card { background: #121212; border-radius: 8px; padding: 8px; }
.sp .sp-main { padding: 0; }
.sp .sp-sub, .sp .sp-sub label { color: #b3b3b3; font-size: 13px; }
.sp .sp-h0 { font-size: 56px; font-weight: 900; letter-spacing: -1.5px; }
.sp .sp-h1 { font-size: 32px; font-weight: 800; }
.sp .sp-h2 { font-size: 24px; font-weight: 800; }
.sp .sp-kind { font-size: 13px; font-weight: 700; }
.sp .sp-nav { font-size: 15px; font-weight: 700; color: #fff; }
.sp button.sp-navbtn { background: none; padding: 8px 12px; color: #b3b3b3; }
.sp button.sp-navbtn:hover label { color: #fff; }
.sp entry.sp-search { min-height: 44px; border-radius: 22px; background: #242424; color: #fff; margin: 4px; }
.sp list, .sp list row { background: transparent; }
.sp list.sp-lib row { border-radius: 6px; padding: 4px 6px; }
.sp list.sp-lib row:hover { background: #1f1f1f; }
.sp list.sp-lib row:selected { background: #2a2a2a; }
.sp .sp-libtitle { font-size: 15px; font-weight: 500; }
.sp .sp-cover, .sp .sp-cover-round { border-radius: 4px; background: #282828; }
.sp .sp-cover-round { border-radius: 999px; }
.sp .sp-liked-tile { background-image: linear-gradient(135deg, #450af5, #c4efd9); color: #fff; }
.sp .sp-queue-tile { background: #2a2a2a; color: #fff; }
.sp .sp-head { padding: 64px 24px 24px 24px; background-image: linear-gradient(to bottom, #4a4a52, #121212); border-radius: 8px 8px 0 0; }
.sp .sp-head-liked { background-image: linear-gradient(to bottom, #5038a0, #121212); }
.sp .sp-head .sp-cover { box-shadow: 0 8px 24px rgba(0,0,0,.5); }
.sp button.sp-bigplay { min-width: 56px; min-height: 56px; border-radius: 28px; background: #1ed760; color: #000; padding: 0; }
.sp button.sp-bigplay:hover { background: #3be477; }
.sp .sp-cols { padding: 6px 24px; border-bottom: 1px solid #2a2a2a; margin: 0 24px 8px 24px; }
.sp .sp-colh { color: #b3b3b3; font-size: 13px; }
.sp list.sp-tracks { margin: 0 24px 24px 24px; }
.sp list.sp-tracks row { border-radius: 4px; padding: 6px 16px; }
.sp list.sp-tracks row:hover { background: #2a2a2a; }
.sp list.sp-tracks row:selected { background: #3e3e3e; }
.sp .sp-track { font-size: 15px; }
.sp .sp-num { color: #b3b3b3; font-size: 15px; }
.sp .sp-playing, .sp label.sp-playing { color: #1ed760; }
.sp flowboxchild { padding: 0; border-radius: 8px; }
.sp .sp-tile { padding: 12px; border-radius: 8px; background: #181818; }
.sp flowboxchild:hover .sp-tile { background: #282828; }
.sp .sp-bar { background: #000; padding: 8px 16px 12px 16px; min-height: 72px; }
.sp .sp-now { font-size: 14px; font-weight: 600; }
.sp .sp-time { color: #b3b3b3; font-size: 11px; font-feature-settings: "tnum"; }
.sp button.sp-icon, .sp togglebutton.sp-icon, .sp button.sp-icon:checked { background: none; box-shadow: none; color: #b3b3b3; min-width: 32px; min-height: 32px; padding: 0; }
.sp button.sp-icon:hover { color: #fff; }
.sp button.sp-icon:checked, .sp button.sp-icon.sp-on { color: #1ed760; }
.sp button.sp-play { min-width: 36px; min-height: 36px; border-radius: 18px; background: #fff; color: #000; padding: 0; }
.sp button.sp-play:hover { background: #f0f0f0; }
.sp scale.sp-scale trough { min-height: 4px; border-radius: 2px; background: #4d4d4d; }
.sp scale.sp-scale highlight { background: #fff; border-radius: 2px; }
.sp scale.sp-scale:hover highlight { background: #1ed760; }
.sp scale.sp-scale slider { min-width: 12px; min-height: 12px; margin: -4px; background: #fff; border: none; box-shadow: none; opacity: 0; }
.sp scale.sp-scale:hover slider { opacity: 1; }
.sp button.sp-devbtn { background: none; color: #b3b3b3; padding: 4px 10px; }
.sp button.sp-devbtn.sp-on, .sp button.sp-devbtn.sp-on label { color: #1ed760; }
.sp .sp-device { font-size: 12px; font-weight: 600; }
.sp button.sp-green { background: #1ed760; color: #000; font-weight: 700; border-radius: 22px; min-height: 44px; }
.sendcell { background: #17171a; border: 1px solid #000; border-radius: 4px; padding: 1px 1px 0 1px;
  box-shadow: inset 0 1px 2px rgba(0,0,0,.6); }
.sendcell button.route { min-width: 24px; min-height: 15px; padding: 0; font-size: 8px; }
button.solo { min-width: 32px; min-height: 24px; padding: 0; font-weight: 700; font-size: 11px; border-radius: 6px; background: #24242a; color: #b8b8bd; }
button.solo:checked { background: #f5c518; color: #111; }
button.norm { min-width: 32px; min-height: 24px; padding: 0; font-weight: 700; font-size: 11px; border-radius: 6px; background: #24242a; color: #b8b8bd; }
button.norm:checked { background: #5ac8fa; color: #111; }
.sendval { font-size: 7px; font-weight: 700; color: #a0a0a8; font-feature-settings: "tnum"; }
.person { background: #17171a; border: 1px solid #000; border-radius: 6px; padding: 6px 4px; }
label.speaklight { color: #3a3a40; font-size: 12px; }
label.speaklight.talking { color: #3be477; text-shadow: 0 0 6px rgba(60,230,120,.9); }
.syspanel { background: #17171a; border: 1px solid #000; border-radius: 6px; padding: 8px; }
.sysbig { font-size: 13px; font-weight: 800; font-feature-settings: "tnum"; color: #e6e6e8; }
.sysbig.warn { color: #ffb347; }
label.svc { font-size: 10px; font-weight: 800; padding: 3px 7px; border-radius: 4px; background: #17171a; border: 1px solid #000; }
label.svc.ok { color: #3be477; }
label.svc.bad { color: #ff5a4a; background: #2a1414; }
label.midi { font-size: 11px; font-weight: 800; color: #55555c; margin-right: 8px; }
label.midi.on { color: #3be477; }
dropdown.instrument button { min-height: 20px; padding: 0 4px; font-size: 10px; font-weight: 700; }
.spectrum { background: #121214; border-radius: 14px; padding: 10px 12px 6px 6px; }
/* console fader: thin slot, grooved cap with a white index line, printed scale */
scale.vertical trough { min-width: 4px; border-radius: 2px; background: #050506; box-shadow: inset 0 1px 2px rgba(0,0,0,.9); }
scale.vertical highlight { background: none; }
scale.vertical slider, scale.vertical slider:hover, scale.vertical slider:active {
  min-width: 26px; min-height: 40px; margin: -18px -11px; border-radius: 3px; border: 1px solid #000; outline: none;
  background-image: linear-gradient(to bottom, #4a4a50 0%, #2c2c31 18%, #3a3a40 30%, #2a2a2f 47%, #f2f2f2 47%, #f2f2f2 53%, #2a2a2f 53%, #3a3a40 70%, #2c2c31 82%, #1c1c20 100%);
  box-shadow: 0 3px 6px rgba(0,0,0,.75), inset 0 1px 0 rgba(255,255,255,.15); }
scale marks indicator { color: #4a4a52; min-height: 1px; min-width: 6px; }
scale marks label { font-size: 9px; color: #6a6a72; font-feature-settings: "tnum"; }
"#;

/// A fader bound to one node's volume (and optionally its mute).
struct Fader {
    node: RefCell<String>,
    scale: gtk::Scale,
    label: gtk::Label,
    mute: Option<gtk::ToggleButton>,
    /// row holding M (strips add S next to it)
    btns: gtk::Box,
    updating: Cell<bool>,
    busy: Cell<u32>,
    /// true while auto-normalize moves the slider (its moves are not his, so they are not saved at once)
    auto: Cell<bool>,
    /// Some(bus id) for a physical output bus: it shows the saved gain, never the device's (#33)
    bus: RefCell<Option<String>>,
    /// the loudest this fader may set (bus ceiling, #33; MAX_DB elsewhere)
    max_db: Cell<f64>,
}

impl Fader {
    /// Auto-normalize moves the real slider; the value-changed handler sees `auto` and does not count it as his move.
    fn set_auto(&self, db: f64) {
        self.auto.set(true);
        self.scale.set_value(db_to_pos(db));
        self.auto.set(false);
    }

    /// Show a saved level (physical buses: the config, not the device).
    fn show(&self, db: f64, mute: bool) {
        self.updating.set(true);
        self.scale.set_value(db_to_pos(db));
        self.label.set_label(&fmt_db(db));
        if let Some(m) = &self.mute {
            m.set_active(mute);
        }
        self.updating.set(false);
    }

    /// Saved gain on the slider, the node's mute on M.
    fn sync_saved(&self, g: &Graph, db: f64) {
        if self.busy.get() > 0 {
            self.busy.set(self.busy.get() - 1);
            return;
        }
        let mute = g.node(&self.node.borrow()).filter(|n| n.has_vol).map(|n| n.mute);
        let m_now = self.mute.as_ref().map(|m| m.is_active()).unwrap_or(false);
        self.show(db, mute.unwrap_or(m_now));
    }

    fn sync(&self, g: &Graph) {
        if self.busy.get() > 0 {
            self.busy.set(self.busy.get() - 1);
            return;
        }
        // a node whose Props are not enumerated yet has no volume to show (it is not 0 dB)
        if let Some(n) = g.node(&self.node.borrow()).filter(|n| n.has_vol) {
            self.updating.set(true);
            let db = config::vol_to_db(n.vol);
            self.scale.set_value(db_to_pos(db));
            self.label.set_label(&fmt_db(db));
            if let Some(m) = &self.mute {
                m.set_active(n.mute);
            }
            self.updating.set(false);
        }
    }
}

/// Peak meter fed by a `pw-record` child process (raw f32 on stdout).
struct Meter {
    child: std::process::Child,
    level: [std::sync::Arc<std::sync::atomic::AtomicU64>; 2],
    led: Option<Rc<LedMeter>>,
    vu: Option<Rc<Vu>>,
    /// latest (peak dB, rms dB), shared with rack-page gauges
    out: Rc<Cell<(f64, f64)>>,
    last: [Cell<f64>; 2],
    /// average (RMS) level for the VU, dBFS
    rms: std::sync::Arc<std::sync::atomic::AtomicU64>,
    last_rms: Cell<f64>,
    /// spectrum of this tap, shared with the analysers
    bands: Arc<Mutex<Vec<f32>>>,
    /// the pw node name this tap records as (`meter_<id>.<pid>`, unique per process)
    node: String,
    /// L/R solo view: which side of the stereo pair this meter shows
    side: Rc<Cell<Side>>,
}
impl Drop for Meter {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Meter {
    fn tick(&self, dt: f64) {
        let mut db = [0.0; 2];
        for c in 0..2 {
            let raw = f64::from_bits(self.level[c].swap(f64::NAN.to_bits(), std::sync::atomic::Ordering::Relaxed));
            db[c] = if raw.is_nan() { self.last[c].get() } else { raw };
            self.last[c].set(db[c]);
        }
        // L/R solo: both ears get the one side (the monitor shows what is heard)
        match self.side.get() {
            Side::Left => db[1] = db[0],
            Side::Right => db[0] = db[1],
            Side::Stereo => {}
        }
        if let Some(led) = &self.led {
            led.step(db[0], db[1], dt);
        }
        let raw = f64::from_bits(self.rms.swap(f64::NAN.to_bits(), std::sync::atomic::Ordering::Relaxed));
        let r = if raw.is_nan() { self.last_rms.get() } else { raw };
        self.last_rms.set(r);
        if let Some(vu) = &self.vu {
            vu.step(r, dt);
        }
        self.out.set((db[0].max(db[1]), r));
    }
}

/// Meter on a sink monitor / source (`target`), or unconnected (`None`, linked manually as `name`).
fn meter(name: &str, target: Option<(&str, bool)>, led: Option<Rc<LedMeter>>, vu: Option<Rc<Vu>>, side: Rc<Cell<Side>>) -> Option<Meter> {
    use std::io::Read;
    if crate::dash::no_audio() {
        return None;
    }
    use std::sync::atomic::{AtomicU64, Ordering};
    // a stale same-named tap from an earlier window can never take the link: every tap gets its pid
    let node = format!("{name}.{}", std::process::id());
    let mut props = format!("node.name={node} node.passive=true media.class=Stream/Input/Audio");
    let mut args: Vec<String> = vec![];
    match target {
        Some((t, sink)) => {
            if sink {
                props.push_str(" stream.capture.sink=true");
            }
            args.push("--target".into());
            args.push(t.into());
        }
        None => props.push_str(" node.autoconnect=false"),
    }
    args.extend(["-P".into(), format!("{{ {props} }}"), "--format".into(), "f32".into(), "--rate".into(), "48000".into(),
                 "--channels".into(), "2".into(), "--latency".into(), "20ms".into(), "--raw".into(), "-".into()]);
    // dies with this window (SIGKILL included): PDEATHSIG, see pw::die_with_parent
    let mut cmd = std::process::Command::new("pw-record");
    crate::pw::die_with_parent(&mut cmd);
    let mut child = cmd
        .args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let mut out = child.stdout.take()?;
    let level = [std::sync::Arc::new(AtomicU64::new(f64::NAN.to_bits())), std::sync::Arc::new(AtomicU64::new(f64::NAN.to_bits()))];
    let lv = level.clone();
    let rms = std::sync::Arc::new(AtomicU64::new(f64::NAN.to_bits()));
    let rv = rms.clone();
    let bands = Arc::new(Mutex::new(vec![0.0f32; crate::dash::BANDS]));
    let bv = bands.clone();
    std::thread::spawn(move || {
        let mut fft = Fft::new();
        let mut buf = vec![0u8; 4 * 2 * 960]; // 20 ms stereo f32
        let mut carry: Vec<u8> = vec![];
        while let Ok(n) = out.read(&mut buf) {
            if n == 0 {
                break;
            }
            carry.extend_from_slice(&buf[..n]);
            let frames = carry.len() / 8;
            let mut pk = [0.0f32; 2];
            let mut sq = 0.0f64;
            for f in 0..frames {
                for c in 0..2 {
                    let o = f * 8 + c * 4;
                    let x = f32::from_le_bytes([carry[o], carry[o + 1], carry[o + 2], carry[o + 3]]);
                    pk[c] = pk[c].max(x.abs());
                    sq += (x as f64) * (x as f64);
                }
                let o = f * 8;
                let l = f32::from_le_bytes([carry[o], carry[o + 1], carry[o + 2], carry[o + 3]]);
                let r = f32::from_le_bytes([carry[o + 4], carry[o + 5], carry[o + 6], carry[o + 7]]);
                fft.push(l, r);
            }
            if let Some(b) = fft.take() {
                *bv.lock().unwrap() = b;
            }
            if frames > 0 {
                let r = (sq / (2 * frames) as f64).sqrt();
                rv.store((if r > 0.0 { 20.0 * r.log10() } else { -90.0 }).to_bits(), Ordering::Relaxed);
            }
            carry.drain(..frames * 8);
            for c in 0..2 {
                let db = if pk[c] > 0.0 { 20.0 * (pk[c] as f64).log10() } else { -90.0 };
                let prev = f64::from_bits(lv[c].load(Ordering::Relaxed));
                if prev.is_nan() || db > prev {
                    lv[c].store(db.to_bits(), Ordering::Relaxed);
                }
            }
        }
    });
    Some(Meter { child, level, led, vu, out: Rc::new(Cell::new((-90.0, -90.0))), last: [Cell::new(-90.0), Cell::new(-90.0)], rms, last_rms: Cell::new(-90.0), bands, node, side })
}

pub struct App {
    cfg: RefCell<Config>,
    /// config.json as this window last read or wrote it: the base of the 3-way merge that keeps the
    /// engine's remote-control writes (#26) and this window's edits from overwriting each other
    synced: RefCell<Config>,
    /// N (auto-normalize) buttons by input id, so a remote toggle shows up here
    norm_btns: RefCell<HashMap<String, gtk::ToggleButton>>,
    /// inputs whose in-memory gain was last moved by auto-normalize, not by him (B36-8): on a merge, a gain
    /// another writer saved beats those moves
    auto_gain: RefCell<std::collections::HashSet<String>>,
    graph: RefCell<Graph>,
    dirty: Cell<bool>,
    content: gtk::Box,
    /// Spotify client page (kept alive for the window's lifetime)
    _player: Rc<crate::player::Player>,
    /// the Rack page: every input's outboard gear and EQ, all dials visible
    rack_page: gtk::Box,
    /// system gauges, shown under the output strips
    sys_panel: gtk::Box,
    faders: RefCell<Vec<Rc<Fader>>>,
    /// #33: per physical bus, adopt-vs-re-apply state across graph polls (survives rebuilds)
    bus_guard: RefCell<HashMap<String, crate::busvol::Tracker>>,
    routes: RefCell<Vec<(String, gtk::ToggleButton, Rc<Cell<bool>>)>>,
    meters: RefCell<Vec<Meter>>,
    /// rack-page only: input-level taps, gauge animations, and whether the page is showing
    rack_meters: RefCell<Vec<Meter>>,
    rack_ticks: RefCell<Vec<Box<dyn Fn(f64)>>>,
    /// console-page gauges/analysers (rebuilt with the strips)
    console_ticks: RefCell<Vec<Box<dyn Fn(f64)>>>,
    rack_visible: Cell<bool>,
    /// input-level taps run on every page but the Player (nothing there shows them)
    taps_on: Cell<bool>,
    levels: RefCell<HashMap<String, Rc<Cell<(f64, f64)>>>>,
    spectra: RefCell<HashMap<String, Arc<Mutex<Vec<f32>>>>>,
    patch: Rc<PatchBay>,
    /// keeps the strip knobs alive (their widgets only hold weak refs)
    knobs: RefCell<Vec<Rc<Knob>>>,
    /// keeps VUs/LED meters drawable even if their audio tap couldn't start
    keep: RefCell<Vec<Rc<dyn std::any::Any>>>,
    /// soloed inputs (temporary, never saved)
    solo: RefCell<std::collections::HashSet<String>>,
    /// MIDI control-surface targets (rebuilt with the strips)
    solo_btns: RefCell<HashMap<String, gtk::ToggleButton>>,
    send_knobs: RefCell<HashMap<(String, String), Rc<Knob>>>,
    /// MIDI pickup state per controller: (engaged, last incoming 0..1, last applied 0..1)
    pickup: RefCell<HashMap<u8, (bool, f64, f64)>>,
    meter_links: RefCell<Vec<(String, String)>>,
    /// auto-normalize state per input id (survives page switches)
    norm: RefCell<HashMap<String, crate::norm::Channel>>,
    /// auto moved a slider since the last save (saved at most once a minute)
    norm_moved: Cell<bool>,
    norm_save: Cell<f64>,
    /// L/R solo per key ("in:<id>" or "bus:<id>"); survives page switches
    sides: RefCell<HashMap<String, Rc<Cell<Side>>>>,
    side_btns: RefCell<Vec<gtk::ToggleButton>>,
    /// last (side, node ids) pushed to the graph per (out, in) link
    side_applied: RefCell<HashMap<(String, String), (Side, u64, u64)>>,
    side_clear: RefCell<Option<gtk::Button>>,
    /// link batches for the L/R solo, run one at a time in push order (off the GTK thread)
    side_q: Serial,
    apps: gtk::ListBox,
    apps_sig: RefCell<String>,
    css: gtk::CssProvider,
    stems: stems_view::State,
}

fn label_x(text: &str) -> gtk::Label {
    let l = label(text, "small");
    l.set_xalign(0.0);
    l
}

fn label(text: &str, class: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.add_css_class(class);
    l
}

fn eq_label(eq: &[f64]) -> String {
    if eq.iter().all(|g| g.abs() < 0.01) { "EQ".into() } else { "EQ ●".into() }
}

fn rack_label(r: &config::Rack) -> String {
    let n = [r.pre_on, r.gate_on, r.comp_on, r.exc_on, r.echo_on].iter().filter(|x| **x).count();
    if n == 0 { "RACK".into() } else { format!("RACK {n}") }
}

type RackSet = fn(&mut config::Rack, f64);
type RackGet = fn(&config::Rack) -> f64;
type RackPow = fn(&mut config::Rack) -> &mut bool;

impl App {
    /// Graphic EQ popover: one knob per band, live to PipeWire.
    fn eq_popover(self: &Rc<Self>, id: &str, name: &str, eq: &[f64], btn: &gtk::MenuButton) -> gtk::Popover {
        let pop = gtk::Popover::new();
        pop.set_child(Some(&self.eq_panel(id, name, eq, Some(btn.clone()))));
        pop
    }

    /// The 12-band EQ faceplate (used in the popover and on the Rack page).
    fn eq_panel(self: &Rc<Self>, id: &str, name: &str, eq: &[f64], btn: Option<gtk::MenuButton>) -> gtk::Box {
        let outer = gtk::Box::new(gtk::Orientation::Vertical, 10);
        outer.set_margin_top(12);
        outer.set_margin_bottom(12);
        outer.set_margin_start(14);
        outer.set_margin_end(14);
        let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let t = label(&format!("{} · 12-BAND EQ", name.to_uppercase()), "section");
        t.set_hexpand(true);
        t.set_xalign(0.0);
        head.append(&t);
        let mut preset_names = vec!["Presets…"];
        preset_names.extend(config::EQ_PRESETS.iter().map(|p| p.0));
        let presets = gtk::DropDown::from_strings(&preset_names);
        head.append(&presets);
        outer.append(&head);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 1);
        let mut knobs: Vec<(Rc<Knob>, gtk::Label)> = vec![];
        for (k, (_, band)) in config::EQ_BANDS.iter().enumerate() {
            let col = gtk::Box::new(gtk::Orientation::Vertical, 2);
            let g0 = eq.get(k).copied().unwrap_or(0.0);
            let val = label(&format!("{g0:+.1}"), "db");
            val.set_width_chars(4);
            let (app, id2, v2, b2) = (self.clone(), id.to_string(), val.clone(), btn.clone());
            let knob = Knob::new(31, g0, config::EQ_DB, move |g| {
                v2.set_label(&format!("{g:+.1}"));
                let eq = {
                    let mut c = app.cfg.borrow_mut();
                    let i = c.inputs.iter_mut().find(|i| i.id == id2).unwrap();
                    i.eq[k] = g;
                    i.eq.clone()
                };
                if let Some(b2) = &b2 {
                    b2.set_child(Some(&label(&eq_label(&eq), "btnlabel")));
                }
                app.mark_dirty();
                if let Some(nid) = app.node_id(&names::strip_in(&id2)) {
                    pw::set_eq(nid, &eq);
                }
            });
            knob.area.set_tooltip_text(Some("Drag or scroll · double-click resets"));
            col.append(&val);
            col.append(&knob.area);
            col.append(&label(band, "small"));
            row.append(&col);
            knobs.push((knob, val));
        }
        outer.append(&row);
        let (app, id2, b2) = (self.clone(), id.to_string(), btn.clone());
        presets.connect_selected_notify(move |d| {
            let sel = d.selected() as usize;
            if sel == 0 {
                return;
            }
            let eq: Vec<f64> = config::EQ_PRESETS[sel - 1].1.to_vec();
            for ((k, v), g) in knobs.iter().zip(&eq) {
                k.set_value(*g);
                v.set_label(&format!("{g:+.1}"));
            }
            if let Some(i) = app.cfg.borrow_mut().inputs.iter_mut().find(|i| i.id == id2) {
                i.eq = eq.clone();
            }
            if let Some(b2) = &b2 {
                b2.set_child(Some(&label(&eq_label(&eq), "btnlabel")));
            }
            app.mark_dirty();
            if let Some(nid) = app.node_id(&names::strip_in(&id2)) {
                pw::set_eq(nid, &eq);
            }
            // back to the placeholder so the same preset can be picked again
            let d = d.clone();
            glib::idle_add_local_once(move || d.set_selected(0));
        });
        outer
    }

    /// Apply a change to an input's rack and push every rack control live.
    fn rack_change(self: &Rc<Self>, id: &str, f: impl FnOnce(&mut config::Rack)) -> config::Rack {
        let rack = {
            let mut c = self.cfg.borrow_mut();
            let i = c.inputs.iter_mut().find(|i| i.id == id).unwrap();
            f(&mut i.rack);
            i.rack.clone()
        };
        self.mark_dirty();
        if let Some(nid) = self.node_id(&names::strip_in(id)) {
            pw::set_rack(nid, &rack);
        }
        rack
    }

    /// Outboard gear for one input: four 19" rack units with power switches and knobs.
    fn rack_popover(self: &Rc<Self>, id: &str, name: &str, btn: &gtk::MenuButton) -> gtk::Popover {
        let pop = gtk::Popover::new();
        let outer = gtk::Box::new(gtk::Orientation::Vertical, 6);
        outer.set_margin_top(10);
        outer.set_margin_bottom(10);
        outer.set_margin_start(10);
        outer.set_margin_end(10);
        let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let t = label(&format!("{} · OUTBOARD RACK", name.to_uppercase()), "section");
        t.set_xalign(0.0);
        t.set_hexpand(true);
        head.append(&t);
        let (b2, p2) = (btn.clone(), pop.clone());
        let r_app = self.clone();
        let id_s = id.to_string();
        head.append(&self.rack_preset_menu(id, move || {
            // reopen so every knob shows the preset's values
            let r = r_app.cfg.borrow().inputs.iter().find(|i| i.id == id_s).map(|i| i.rack.clone()).unwrap_or_default();
            b2.set_child(Some(&label(&rack_label(&r), "btnlabel")));
            p2.popdown();
        }));
        outer.append(&head);
        outer.append(&self.rack_units(id, Some(btn.clone()), gtk::Orientation::Vertical, false));
        pop.set_child(Some(&outer));
        pop
    }

    /// The four rack units for one input, stacked (popover) or side by side (Rack page).
    fn rack_units(self: &Rc<Self>, id: &str, btn: Option<gtk::MenuButton>, orient: gtk::Orientation, gauges: bool) -> gtk::Box {
        let outer = gtk::Box::new(orient, 6);
        let rack = self.cfg.borrow().inputs.iter().find(|i| i.id == id).map(|i| i.rack.clone()).unwrap_or_default();
        // (model, unit, power, knobs: (title, min, max, default, step, get, set, fmt))
        type K = (&'static str, f64, f64, f64, f64, RackGet, RackSet, fn(f64) -> String);
        let units: Vec<(&str, &str, RackPow, Vec<K>)> = vec![
            ("WL-1", "PREAMP", |r| &mut r.pre_on, vec![
                ("TRIM", -12.0, 24.0, 0.0, 0.5, |r| r.trim_db, |r, v| r.trim_db = v, |v| format!("{v:+.1}dB")),
                ("DRIVE", 0.0, 24.0, 6.0, 0.5, |r| r.drive_db, |r, v| r.drive_db = v, |v| format!("{v:.1}dB")),
                ("OUTPUT", -24.0, 6.0, 0.0, 0.5, |r| r.out_db, |r, v| r.out_db = v, |v| format!("{v:+.1}dB")),
            ]),
            ("WL-2", "NOISE GATE", |r| &mut r.gate_on, vec![
                ("THRESH", -60.0, 0.0, -45.0, 0.5, |r| r.gate_db, |r, v| r.gate_db = v, |v| format!("{v:.0}dB")),
            ]),
            ("WL-3", "COMPRESSOR", |r| &mut r.comp_on, vec![
                ("THRESH", -40.0, 0.0, -18.0, 0.5, |r| r.comp_thr_db, |r, v| r.comp_thr_db = v, |v| format!("{v:.1}dB")),
                ("RATIO", 0.0, 1.0, 0.5, 0.01, |r| r.comp_ratio, |r, v| r.comp_ratio = v, |v| format!("{:.0}%", v * 100.0)),
                ("ATTACK", 0.0, 1.0, 0.3, 0.01, |r| r.comp_attack, |r, v| r.comp_attack = v, |v| format!("{:.0}%", v * 100.0)),
                ("RELEASE", 0.0, 1.0, 0.5, 0.01, |r| r.comp_release, |r, v| r.comp_release = v, |v| format!("{:.0}%", v * 100.0)),
                ("MAKEUP", -12.0, 18.0, 3.0, 0.5, |r| r.comp_makeup_db, |r, v| r.comp_makeup_db = v, |v| format!("{v:+.1}dB")),
            ]),
            ("WL-4", "EXCITER", |r| &mut r.exc_on, vec![
                ("LOW", 0.0, 1.0, 0.25, 0.01, |r| r.exc_lo, |r, v| r.exc_lo = v, |v| format!("{:.0}%", v * 100.0)),
                ("HIGH", 0.0, 1.0, 0.25, 0.01, |r| r.exc_hi, |r, v| r.exc_hi = v, |v| format!("{:.0}%", v * 100.0)),
            ]),
            ("WL-5", "ECHO", |r| &mut r.echo_on, vec![
                ("TIME", 0.05, 1.5, 0.35, 0.01, |r| r.echo_time, |r, v| r.echo_time = v, |v| format!("{:.0}ms", v * 1000.0)),
                ("REPEATS", 0.0, 0.9, 0.5, 0.01, |r| r.echo_fb, |r, v| r.echo_fb = v, |v| format!("{:.0}%", v * 100.0)),
                ("MIX", 0.0, 1.0, 0.35, 0.01, |r| r.echo_mix, |r, v| r.echo_mix = v, |v| format!("{:.0}%", v * 100.0)),
            ]),
        ];
        for (model, unit, power, knobs) in units {
            let panel = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            panel.add_css_class("rackunit");
            // rack ear with screws
            let ear = gtk::Box::new(gtk::Orientation::Vertical, 0);
            ear.set_size_request(14, -1);
            ear.add_css_class("ear");
            panel.append(&ear);
            let head = gtk::Box::new(gtk::Orientation::Vertical, 4);
            head.set_size_request(70, -1);
            head.set_valign(gtk::Align::Center);
            let m = label(model, "small");
            m.set_xalign(0.0);
            let u = label(unit, "name");
            u.set_xalign(0.0);
            let mut cur = rack.clone();
            let on = *power(&mut cur);
            let sw = gtk::ToggleButton::with_label(if on { "● ON" } else { "○ OFF" });
            sw.add_css_class("power");
            sw.set_active(on);
            sw.set_halign(gtk::Align::Start);
            let (app, id2, b2) = (self.clone(), id.to_string(), btn.clone());
            sw.connect_toggled(move |t| {
                let v = t.is_active();
                t.set_label(if v { "● ON" } else { "○ OFF" });
                let r = app.rack_change(&id2, |r| *power(r) = v);
                if let Some(b2) = &b2 {
                    b2.set_child(Some(&label(&rack_label(&r), "btnlabel")));
                }
            });
            head.append(&m);
            head.append(&u);
            head.append(&sw);
            panel.append(&head);
            // knobs on top, the unit's gauge underneath (keeps units narrow)
            let face = gtk::Box::new(gtk::Orientation::Vertical, 6);
            face.set_valign(gtk::Align::Center);
            let knob_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            knob_row.set_halign(gtk::Align::Center);
            face.append(&knob_row);
            panel.append(&face);
            for (title, min, max, default, step, get, set, fmt) in knobs {
                let col = gtk::Box::new(gtk::Orientation::Vertical, 2);
                let v0 = get(&rack);
                let val = label(&fmt(v0), "db");
                val.set_width_chars(6);
                let (app, id2, v2) = (self.clone(), id.to_string(), val.clone());
                let knob = Knob::ranged(34, v0, min, max, default, step, move |v| {
                    v2.set_label(&fmt(v));
                    app.rack_change(&id2, |r| set(r, v));
                });
                knob.area.set_tooltip_text(Some("Drag or scroll · double-click resets"));
                col.append(&label(title, "small"));
                col.append(&knob.area);
                col.append(&val);
                knob_row.append(&col);
                self.knobs.borrow_mut().push(knob);
            }
            if gauges {
                let g = self.unit_gauge(id, model);
                g.set_halign(gtk::Align::Center);
                face.append(&g);
            }
            let ear2 = gtk::Box::new(gtk::Orientation::Vertical, 0);
            ear2.set_size_request(14, -1);
            ear2.set_hexpand(orient == gtk::Orientation::Vertical);
            ear2.set_halign(gtk::Align::End);
            ear2.add_css_class("ear");
            panel.append(&ear2);
            outer.append(&panel);
        }
        outer
    }

    /// The moving gauge for one rack unit (Rack page). Levels are real (input tap and channel
    /// output); gate state and gain reduction are computed from those levels and the settings.
    fn unit_gauge(self: &Rc<Self>, id: &str, model: &str) -> gtk::Widget {
        self.gauge_into(id, model, false, 86)
    }

    fn gauge_into(self: &Rc<Self>, id: &str, model: &str, console: bool, size: i32) -> gtk::Widget {
        let ticks = if console { &self.console_ticks } else { &self.rack_ticks };
        let zero = || Rc::new(Cell::new((-90.0, -90.0)));
        let pre = self.levels.borrow().get(&format!("pre:{id}")).cloned().unwrap_or_else(zero);
        let post = self.levels.borrow().get(&format!("post:{id}")).cloned().unwrap_or_else(zero);
        let (w, id) = (Rc::downgrade(self), id.to_string());
        let rack = move || w.upgrade().and_then(|a| a.cfg.borrow().inputs.iter().find(|i| i.id == id).map(|i| i.rack.clone())).unwrap_or_default();
        let after_pre = |r: &config::Rack, db: f64| if r.pre_on { db + r.trim_db + r.out_db } else { db };
        match model {
            "WL-1" => {
                let vu = Vu::new(size);
                vu.area.set_valign(gtk::Align::Center);
                let v2 = vu.clone();
                ticks.borrow_mut().push(Box::new(move |dt| {
                    let r = rack();
                    v2.step(after_pre(&r, pre.get().1), dt);
                }));
                vu.area.clone().upcast()
            }
            "WL-2" => {
                let lamp = Lamp::new();
                let (l2, open) = (lamp.clone(), Cell::new(false));
                ticks.borrow_mut().push(Box::new(move |_| {
                    let r = rack();
                    let lvl = after_pre(&r, pre.get().0);
                    // same hysteresis as the gate: opens at threshold, shuts 12 dB below
                    let o = if open.get() { lvl > r.gate_db - 12.0 } else { lvl > r.gate_db };
                    open.set(o);
                    l2.set(r.gate_on, o);
                }));
                lamp.area.clone().upcast()
            }
            "WL-3" => {
                let gr = GrMeter::new(size);
                let g2 = gr.clone();
                ticks.borrow_mut().push(Box::new(move |dt| {
                    let r = rack();
                    let over = (after_pre(&r, pre.get().1) - r.comp_thr_db).max(0.0);
                    let target = if r.comp_on { over * r.comp_ratio } else { 0.0 };
                    g2.step(target, r.comp_attack, r.comp_release, dt);
                }));
                gr.area.clone().upcast()
            }
            _ => {
                let vu = Vu::new(size);
                vu.area.set_valign(gtk::Align::Center);
                let v2 = vu.clone();
                ticks.borrow_mut().push(Box::new(move |dt| v2.step(post.get().1, dt)));
                vu.area.clone().upcast()
            }
        }
    }

    /// Whole-rack preset picker for one input.
    fn rack_preset_menu(self: &Rc<Self>, id: &str, then: impl Fn() + 'static) -> gtk::DropDown {
        let presets = config::rack_presets();
        let mut names = vec!["Rack preset…"];
        names.extend(presets.iter().map(|p| p.0));
        let dd = gtk::DropDown::from_strings(&names);
        let (app, id2) = (self.clone(), id.to_string());
        dd.connect_selected_notify(move |d| {
            let sel = d.selected() as usize;
            if sel == 0 {
                return;
            }
            let preset = presets[sel - 1].1.clone();
            app.rack_change(&id2, |r| *r = preset);
            then();
            let d = d.clone();
            glib::idle_add_local_once(move || d.set_selected(0));
        });
        dd
    }

    /// Patch bay click: open the thing that was clicked, right where it was clicked.
    fn patch_clicked(self: &Rc<Self>, hit: Hit, x: f64, y: f64) {
        let d = self.patch.data();
        let (inputs, buses) = {
            let c = self.cfg.borrow();
            (c.inputs.clone(), c.buses.clone())
        };
        let pop = gtk::Popover::new();
        pop.set_parent(&self.patch.area);
        pop.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        pop.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
        body.set_margin_top(10);
        body.set_margin_bottom(10);
        body.set_margin_start(10);
        body.set_margin_end(10);
        match hit {
            Hit::App(k) => {
                let Some((app, cur, serial)) = d.apps.get(k).cloned() else { return };
                body.append(&label(&format!("PATCH {} INTO", app.to_uppercase()), "section"));
                for (i, inp) in inputs.iter().enumerate().filter(|(_, i)| i.kind == InputKind::Virtual) {
                    let b = gtk::Button::with_label(&if i == cur { format!("● {}", inp.name) } else { inp.name.clone() });
                    let (sink, p2) = (names::ch(&inp.id), pop.clone());
                    b.connect_clicked(move |_| {
                        pw::move_stream(serial, &sink);
                        p2.popdown();
                    });
                    body.append(&b);
                }
            }
            Hit::Block(i, blk) => {
                let Some(inp) = inputs.get(i).cloned() else { return };
                match blk {
                    0..=3 | 6 => {
                        body.append(&label(&format!("{} · OUTBOARD RACK", inp.name.to_uppercase()), "section"));
                        body.append(&self.rack_units(&inp.id, None, gtk::Orientation::Vertical, false));
                    }
                    4 => {
                        body.append(&label(&format!("{} · FX PRESET", inp.name.to_uppercase()), "section"));
                        body.append(&label("Changing this rebuilds the channel (short audio gap).", "sub"));
                        for fx in FX_PRESETS {
                            let b = gtk::Button::with_label(&if *fx == inp.fx { format!("● {fx}") } else { fx.to_string() });
                            let (app, id, fx, p2) = (self.clone(), inp.id.clone(), fx.to_string(), pop.clone());
                            b.connect_clicked(move |_| {
                                if let Some(i) = app.cfg.borrow_mut().inputs.iter_mut().find(|i| i.id == id) {
                                    i.fx = fx.clone();
                                }
                                app.structural();
                                p2.popdown();
                            });
                            body.append(&b);
                        }
                    }
                    5 => body.append(&self.eq_panel(&inp.id, &inp.name, &inp.eq, None)),
                    _ => {
                        // FADER block: mute toggle
                        let m = !inp.mute;
                        if let Some(i) = self.cfg.borrow_mut().inputs.iter_mut().find(|i| i.id == inp.id) {
                            i.mute = m;
                        }
                        self.mark_dirty();
                        if let Some(nid) = self.node_id(&names::strip_in(&inp.id)) {
                            pw::set_mute(nid, m);
                        }
                        pop.unparent();
                        return;
                    }
                }
            }
            Hit::Sends(i) => {
                let Some(inp) = inputs.get(i).cloned() else { return };
                body.append(&label(&format!("{} · SENDS", inp.name.to_uppercase()), "section"));
                for b in &buses {
                    let r = inp.route(&b.id);
                    let sw = gtk::CheckButton::with_label(&format!("{} · {}", b.id.to_uppercase(), b.name));
                    sw.set_active(r.on);
                    let (app, iid, bid) = (self.clone(), inp.id.clone(), b.id.clone());
                    sw.connect_toggled(move |t| {
                        let on = t.is_active();
                        {
                            let mut c = app.cfg.borrow_mut();
                            let i = c.inputs.iter_mut().find(|i| i.id == iid).unwrap();
                            i.routes.entry(bid.clone()).or_insert(Route { on, level_db: 0.0 }).on = on;
                        }
                        app.mark_dirty();
                        if let Some(nid) = app.node_id(&names::route(&iid, &bid)) {
                            pw::set_mute(nid, !on);
                        }
                    });
                    body.append(&sw);
                }
            }
        }
        pop.set_child(Some(&body));
        pop.popup();
    }

    /// The IN/OUT meters' spectrum buffers for an input (empty if the meter isn't running).
    fn tap_spectra(&self, id: &str) -> (Arc<Mutex<Vec<f32>>>, Arc<Mutex<Vec<f32>>>) {
        let get = |k: String| self.spectra.borrow().get(&k).cloned().unwrap_or_else(|| Arc::new(Mutex::new(vec![0.0; crate::dash::BANDS])));
        (get(format!("pre:{id}")), get(format!("post:{id}")))
    }

    /// L/R solo state for a key ("in:<id>" or "bus:<id>"), created on first use.
    fn side_cell(&self, key: &str) -> Rc<Cell<Side>> {
        self.sides.borrow_mut().entry(key.to_string()).or_insert_with(|| Rc::new(Cell::new(Side::Stereo))).clone()
    }

    /// Copy one input's normalize state (his position, his anchored level) into the config so a restart resumes it (#7).
    fn norm_store(&self, id: &str) {
        let Some((b, o)) = self.norm.borrow().get(id).map(|c| c.saved()) else { return };
        if let Some(i) = self.cfg.borrow_mut().inputs.iter_mut().find(|i| i.id == id) {
            i.norm_base = b;
            i.norm_out = o;
        }
    }

    /// A manual fader move: his position wins (N on or off) and is kept for restarts.
    fn fader_manual(&self, node: &str, db: f64) {
        let id = self.cfg.borrow().inputs.iter().find(|i| names::strip_in(&i.id) == node).map(|i| i.id.clone());
        if let Some(id) = id {
            self.auto_gain.borrow_mut().remove(&id);
            let hit = self.norm.borrow_mut().get_mut(&id).map(|c| c.manual(db)).is_some();
            if hit {
                self.norm_store(&id);
            }
        }
    }

    /// The L/R buttons for one meter key (input or bus); they rewire the outputs when pressed.
    fn side_row(self: &Rc<Self>, key: String) -> gtk::Box {
        let cell = self.side_cell(&key);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        row.set_halign(gtk::Align::Center);
        let l_btn = gtk::ToggleButton::with_label("L");
        let r_btn = gtk::ToggleButton::with_label("R");
        for b in [&l_btn, &r_btn] {
            b.add_css_class("solo");
            b.add_css_class("side");
        }
        l_btn.set_tooltip_text(Some("Solo left side: hear L in both ears on the headphone/speaker buses (stream buses untouched; temporary)"));
        r_btn.set_tooltip_text(Some("Solo right side: hear R in both ears on the headphone/speaker buses (stream buses untouched; temporary)"));
        l_btn.set_active(cell.get() == Side::Left);
        r_btn.set_active(cell.get() == Side::Right);
        let guard = Rc::new(Cell::new(false));
        for (me, other, mine, theirs) in [(&l_btn, &r_btn, Side::Left, Side::Right), (&r_btn, &l_btn, Side::Right, Side::Left)] {
            let (other2, cell2, guard2, app) = (other.clone(), cell.clone(), guard.clone(), Rc::downgrade(self));
            me.connect_toggled(move |b| {
                if guard2.get() {
                    return;
                }
                if b.is_active() {
                    guard2.set(true);
                    other2.set_active(false);
                    guard2.set(false);
                }
                cell2.set(if b.is_active() { mine } else if other2.is_active() { theirs } else { Side::Stereo });
                if let Some(app) = app.upgrade() {
                    let g = app.graph.borrow();
                    app.apply_sides(&g);
                }
            });
        }
        row.append(&l_btn);
        row.append(&r_btn);
        self.side_btns.borrow_mut().push(l_btn);
        self.side_btns.borrow_mut().push(r_btn);
        row
    }

    /// Desired (out node, in node, side) for every input x hardware bus. A bus solo overrides the channel solo for that bus.
    fn desired_sides(&self) -> Vec<(String, String, Side)> {
        let (ins, buses) = {
            let c = self.cfg.borrow();
            (c.inputs.clone(), c.buses.clone())
        };
        let mut out = vec![];
        for i in &ins {
            for b in &buses {
                if !matches!(b.kind, BusKind::Hardware { .. }) {
                    continue;
                }
                let bs = self.side_cell(&format!("bus:{}", b.id)).get();
                let side = if bs != Side::Stereo { bs } else { self.side_cell(&format!("in:{}", i.id)).get() };
                out.push((names::strip(&i.id), names::route_in(&i.id, &b.id), side));
            }
        }
        out
    }

    /// Rewire the hardware-bus links whose side changed. Links only: no node, no latency. Off the GTK thread.
    fn apply_sides(&self, g: &Graph) {
        let mut work = vec![];
        for (o, i, s) in self.desired_sides() {
            let (Some(a), Some(b)) = (g.node(&o), g.node(&i)) else { continue };
            let (a_id, b_id) = (a.id, b.id);
            let cur = self.side_applied.borrow().get(&(o.clone(), i.clone())).copied();
            let want = (s, a_id, b_id);
            if needs_relink(cur, want) {
                self.side_applied.borrow_mut().insert((o.clone(), i.clone()), want);
                work.push((o, i, s));
            }
        }
        if !work.is_empty() {
            self.side_q.push(Box::new(move || {
                for (o, i, s) in work {
                    pw::link_side(&o, &i, s);
                }
            }));
        }
        let any = self.sides.borrow().values().any(|c| c.get() != Side::Stereo);
        if let Some(btn) = self.side_clear.borrow().as_ref() {
            btn.set_visible(any);
        }
    }

    /// Apply solo: while anything is soloed, every other input is muted live (config untouched).
    fn apply_solo(&self) {
        let solo = self.solo.borrow().clone();
        let inputs: Vec<(String, bool)> = self.cfg.borrow().inputs.iter().map(|i| (i.id.clone(), i.mute)).collect();
        for (id, muted) in inputs {
            let m = crate::stemcfg::effective_mute(muted, &id, &solo, self.stems.replaced.borrow().as_deref());
            if let Some(nid) = self.node_id(&names::strip_in(&id)) {
                pw::set_mute(nid, m);
            }
        }
    }

    /// Fader for a node name (input strip or bus).
    fn fader_for(&self, node: &str) -> Option<Rc<Fader>> {
        self.faders.borrow().iter().find(|f| *f.node.borrow() == node).cloned()
    }

    /// Pickup: a physical control only takes over once it reaches/crosses the on-screen
    /// value, so a fader parked elsewhere can't make the level jump.
    fn pick(&self, cc: u8, incoming: f64, current: f64) -> bool {
        let mut p = self.pickup.borrow_mut();
        let e = p.entry(cc).or_insert((false, incoming, current));
        if e.0 && (current - e.2).abs() > 0.02 {
            e.0 = false; // moved on screen since: pick up again
        }
        if !e.0 && ((incoming - current).abs() < 0.03 || (e.1 - current) * (incoming - current) <= 0.0) {
            e.0 = true;
        }
        e.1 = incoming;
        if e.0 {
            e.2 = incoming;
        }
        e.0
    }

    /// SL MkIII default template → mixer.
    fn midi_cc(self: &Rc<Self>, cc: u8, val: u8) {
        let v = val as f64 / 127.0;
        let (inputs, buses): (Vec<String>, Vec<config::Bus>) = {
            let c = self.cfg.borrow();
            (c.inputs.iter().map(|i| i.id.clone()).collect(), c.buses.clone())
        };
        // 8 strip targets: the inputs, then the first buses
        let target = |k: usize| -> Option<String> {
            if k < inputs.len().min(6) { Some(names::strip_in(&inputs[k])) } else { buses.get(k - inputs.len().min(6)).map(bus_fader_node) }
        };
        match cc {
            41..=48 => {
                if let Some(f) = target((cc - 41) as usize).and_then(|n| self.fader_for(&n)) {
                    if self.pick(cc, v, f.scale.value()) {
                        f.scale.set_value(v);
                    }
                }
            }
            51..=58 if val > 63 => {
                if let Some(m) = target((cc - 51) as usize).and_then(|n| self.fader_for(&n)).and_then(|f| f.mute.clone()) {
                    m.set_active(!m.is_active());
                }
            }
            71..=76 if val > 63 => {
                if let Some(id) = inputs.get((cc - 71) as usize) {
                    if let Some(b) = self.solo_btns.borrow().get(id).cloned() {
                        b.set_active(!b.is_active());
                    }
                }
            }
            21..=26 => {
                let (Some(id), Some(a1)) = (inputs.get((cc - 21) as usize), buses.first()) else { return };
                let k = self.send_knobs.borrow().get(&(id.clone(), a1.id.clone())).cloned();
                if let Some(k) = k {
                    if self.pick(cc, v, k.frac()) {
                        k.turn_to(v);
                    }
                }
            }
            27 | 28 => {
                if let Some(f) = buses.get((cc - 25) as usize).map(bus_fader_node).and_then(|n| self.fader_for(&n)) {
                    if self.pick(cc, v, f.scale.value()) {
                        f.scale.set_value(v);
                    }
                }
            }
            _ => {}
        }
    }

    fn mark_dirty(&self) {
        self.dirty.set(true);
    }

    /// Save without clobbering newer on-disk edits (the engine's remote server writes config.json too, #26):
    /// fields this window changed since it last synced win, everything else comes from the file; except
    /// that auto-normalize gain moves are not his edits, so a remote gain beats them (B36-8). A failed
    /// merge writes nothing.
    fn save_cfg(self: &Rc<Self>) {
        let before = self.cfg.borrow().clone();
        let auto = self.auto_gain.borrow().clone();
        let won = RefCell::new(vec![]);
        let prefer = |b: &Config, d: &Config, m: &mut Config| *won.borrow_mut() = config::their_gains_win(b, d, m, &auto);
        let r = {
            let (mut c, mut b) = (self.cfg.borrow_mut(), self.synced.borrow_mut());
            c.save_merged_with(&mut b, &Config::path(), &prefer)
        };
        match r {
            Ok(true) => self.apply_external(&before, &won.into_inner()),
            Ok(false) => {}
            Err(e) => {
                eprintln!("wavelink: {e}");
                self.dirty.set(true); // retry on a later tick, merging again
            }
        }
    }

    /// Pick up edits another writer saved since this window last synced; this window's unsaved edits
    /// stay (auto-normalize moves excepted, as in save_cfg).
    fn pull_cfg(self: &Rc<Self>) {
        let before = self.cfg.borrow().clone();
        let auto = self.auto_gain.borrow().clone();
        let won = RefCell::new(vec![]);
        let prefer = |b: &Config, d: &Config, m: &mut Config| *won.borrow_mut() = config::their_gains_win(b, d, m, &auto);
        let disk = {
            let (mut c, mut b) = (self.cfg.borrow_mut(), self.synced.borrow_mut());
            c.pull_merged_with(&mut b, &Config::path(), &prefer)
        };
        if disk.is_some() {
            self.apply_external(&before, &won.into_inner());
        }
    }

    /// Show what changed in `self.cfg` relative to `before` after a merge. Levels update in place (input
    /// gains are pushed to their strips again, bus faders show the new saved gain and #35's tracker is told
    /// it was just written, so it never pushes the old value back); anything beyond levels (EQ, rack,
    /// names, colours, channels) rebuilds the window. `won` = inputs where another writer's gain beat
    /// an auto-normalize move.
    fn apply_external(self: &Rc<Self>, before: &Config, won: &[String]) {
        for id in won {
            self.auto_gain.borrow_mut().remove(id);
        }
        let now = self.cfg.borrow().clone();
        if now == *before {
            return;
        }
        // levels-only? copy the remote-controllable fields back and compare the rest
        let mut masked = now.clone();
        for (m, b) in masked.inputs.iter_mut().zip(&before.inputs) {
            if m.id == b.id {
                m.gain_db = b.gain_db;
                m.mute = b.mute;
                m.routes = b.routes.clone();
                m.normalize = b.normalize;
                m.norm_base = b.norm_base;
                m.norm_out = b.norm_out;
            }
        }
        for (m, b) in masked.buses.iter_mut().zip(&before.buses) {
            if m.id == b.id {
                m.gain_db = b.gain_db;
                m.mute = b.mute;
                m.max_db = b.max_db;
            }
        }
        if masked != *before || now.inputs.len() != before.inputs.len() || now.buses.len() != before.buses.len() {
            let app = self.clone();
            glib::timeout_add_local_once(Duration::from_millis(50), move || app.rebuild());
            return;
        }
        for (n, b) in now.inputs.iter().zip(&before.inputs) {
            if n.gain_db != b.gain_db {
                // a remote fader move is his move for auto-normalize too; and it is what the strip plays
                self.fader_manual(&names::strip_in(&n.id), n.gain_db);
                if let Some(id) = self.node_id(&names::strip_in(&n.id)) {
                    pw::set_volume(id, db_to_vol(n.gain_db));
                }
            } else if n.norm_out != b.norm_out {
                if let (Some(o), Some(c)) = (n.norm_out, self.norm.borrow_mut().get_mut(&n.id)) {
                    c.out = o;
                    c.pending = false;
                }
            }
            if n.normalize != b.normalize {
                let btn = self.norm_btns.borrow().get(&n.id).cloned();
                if let Some(btn) = btn {
                    btn.set_active(n.normalize); // its handler records the same value and pauses auto when off
                }
            }
            for (bus, r) in &n.routes {
                if b.route(bus).level_db != r.level_db {
                    if let Some(k) = self.send_knobs.borrow().get(&(n.id.clone(), bus.clone())) {
                        k.set_value(db_to_pos(r.level_db));
                    }
                }
            }
        }
        // bus faders show the config (#35): refresh the ones that changed and tell the tracker the new
        // saved gain is what the device was just given (the writer applied it), so drift right after is
        // re-applied as the NEW value and the old one is never pushed back
        let t = glib::monotonic_time() as f64 / 1e6;
        for (n, b) in now.buses.iter().zip(&before.buses) {
            if n.gain_db == b.gain_db && n.mute == b.mute && n.max_db == b.max_db {
                continue;
            }
            if let Some(tr) = self.bus_guard.borrow_mut().get_mut(&n.id) {
                tr.wrote(t);
            }
            for f in self.faders.borrow().iter() {
                if f.bus.borrow().as_deref() == Some(n.id.as_str()) {
                    f.max_db.set(n.ceiling_db());
                    f.show(n.effective_db(), n.mute);
                }
            }
        }
    }

    /// The saved gain behind an input strip or virtual bus fader node.
    fn saved_gain(&self, node: &str) -> Option<f64> {
        let c = self.cfg.borrow();
        c.inputs.iter().find(|i| names::strip_in(&i.id) == node).map(|i| i.gain_db)
            .or_else(|| c.buses.iter().find(|b| matches!(b.kind, BusKind::Virtual { .. }) && bus_fader_node(b) == node).map(|b| b.effective_db()))
    }

    /// A hardware bus fader's saved gain, capped at its ceiling (None for other faders).
    fn bus_effective_db(&self, f: &Fader) -> Option<f64> {
        let id = f.bus.borrow().clone()?;
        self.cfg.borrow().buses.iter().find(|b| b.id == id).map(|b| b.effective_db())
    }

    /// His fader / mute move on a physical bus just went to the device: it is settling, not drifting.
    fn bus_wrote(&self, f: &Fader) {
        if let Some(id) = f.bus.borrow().as_ref() {
            if let Some(t) = self.bus_guard.borrow_mut().get_mut(id) {
                t.wrote(glib::monotonic_time() as f64 / 1e6);
            }
        }
    }

    /// One graph poll for a physical bus fader (#33): the saved gain is the truth. Re-apply it when the
    /// device (re)appears or drifts right after; take a device value only when it is a deliberate
    /// external change (crate::busvol::decide). The fader always shows the config.
    fn sync_bus(&self, f: &Fader, g: &Graph, id: &str) {
        let Some(mut b) = self.cfg.borrow().buses.iter().find(|b| b.id == id).cloned() else { return };
        let node = g.node(&bus_fader_node(&b));
        // an AirPlay destination (by name or node props) gets its ceiling written into the config once
        if b.persist_raop_ceiling(node.map_or(false, |n| n.raop)) {
            if let Some(bu) = self.cfg.borrow_mut().buses.iter_mut().find(|x| x.id == id) {
                bu.max_db = b.max_db;
            }
            self.mark_dirty();
            eprintln!("wavelink: bus {id}: AirPlay destination, ceiling {} saved", fmt_db(b.ceiling_db()));
            f.max_db.set(b.ceiling_db());
            f.scale.add_mark(db_to_pos(b.ceiling_db()), gtk::PositionType::Right, Some("max"));
        }
        let seen = node.map(|n| (g.instance(n), n.has_vol.then_some(crate::busvol::Reading { vol: n.vol, mute: n.mute })));
        let now = glib::monotonic_time() as f64 / 1e6;
        let ceil = b.ceiling_db();
        let saved = b.effective_db();
        let step = {
            let mut guards = self.bus_guard.borrow_mut();
            let t = guards.entry(id.to_string()).or_default();
            t.set_ceiling(ceil);
            t.observe(now, seen, saved, b.mute)
        };
        match step {
            crate::busvol::Step::Nothing => {}
            crate::busvol::Step::Push => {
                if let Some(n) = node {
                    if n.has_vol && (n.mute != b.mute || (config::vol_to_db(n.vol) - saved).abs() > crate::busvol::TOL_DB) {
                        eprintln!("wavelink: bus {id}: device {} reads {}{}, re-applying saved {}{}", n.name, fmt_db(config::vol_to_db(n.vol)),
                                  if n.mute { " muted" } else { "" }, fmt_db(saved), if b.mute { " muted" } else { "" });
                    }
                    pw::set_volume_mute(n.id, db_to_vol(saved), b.mute);
                }
            }
            crate::busvol::Step::Adopt { gain_db, mute } => {
                eprintln!("wavelink: bus {id}: external change kept: gain {:?} mute {:?} (was {} {})", gain_db, mute, fmt_db(b.gain_db), b.mute);
                if let Some(bu) = self.cfg.borrow_mut().buses.iter_mut().find(|x| x.id == id) {
                    if let Some(d) = gain_db {
                        bu.gain_db = bu.clamp_db(d);
                    }
                    if let Some(m) = mute {
                        bu.mute = m;
                    }
                }
                self.mark_dirty();
            }
        }
        if f.busy.get() > 0 {
            // his drag is in flight: leave the slider alone
            f.busy.set(f.busy.get() - 1);
            return;
        }
        let (db, mute) = self.cfg.borrow().buses.iter().find(|x| x.id == id).map(|x| (x.effective_db(), x.mute)).unwrap_or((saved, b.mute));
        f.show(db, mute);
    }
    fn node_id(&self, name: &str) -> Option<u64> {
        self.graph.borrow().node(name).map(|n| n.id)
    }
    /// Save + rebuild after a structural change (engine picks the file up within a second).
    fn structural(self: &Rc<Self>) {
        self.save_cfg();
        self.dirty.set(false);
        let app = self.clone();
        glib::timeout_add_local_once(Duration::from_millis(50), move || app.rebuild());
    }

    /// Generic dB fader. `apply` receives dB and updates config; volume is pushed to `node`.
    fn fader(self: &Rc<Self>, node: String, height: i32, with_mute: bool, apply: impl Fn(&Self, f64) + 'static, apply_mute: impl Fn(&Self, bool) + 'static) -> (gtk::Box, Rc<Fader>) {
        let b = gtk::Box::new(gtk::Orientation::Vertical, 6);
        // position 0..1 on a console taper; values convert to dB at the edges
        let scale = gtk::Scale::with_range(gtk::Orientation::Vertical, 0.0, 1.0, 0.001);
        scale.set_inverted(true);
        scale.set_draw_value(false);
        scale.set_vexpand(true);
        scale.set_size_request(-1, height);
        scale.add_css_class("console");
        if height >= 200 {
            for (d, t) in [(12.0, "+12"), (6.0, "+6"), (0.0, "0"), (-5.0, "5"), (-10.0, "10"), (-20.0, "20"), (-30.0, "30"), (-40.0, "40"), (-60.0, "∞")] {
                scale.add_mark(db_to_pos(d), gtk::PositionType::Left, Some(t));
            }
        } else {
            scale.add_mark(db_to_pos(0.0), gtk::PositionType::Left, None);
        }
        let lab = label("--", "db");
        lab.set_width_chars(8); // fixed width: "-10.0 dB" must not resize the strip
        let mute = with_mute.then(|| {
            let m = gtk::ToggleButton::with_label("M");
            m.add_css_class("mute");
            m.set_halign(gtk::Align::Center);
            m.set_tooltip_text(Some("Mute"));
            m
        });
        b.append(&scale);
        b.append(&lab);
        let btns = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        btns.set_halign(gtk::Align::Center);
        b.append(&btns);
        if let Some(m) = &mute {
            btns.append(m);
        }
        let f = Rc::new(Fader { node: RefCell::new(node), scale, label: lab, mute, btns, updating: Cell::new(false), busy: Cell::new(0), auto: Cell::new(false), bus: RefCell::new(None), max_db: Cell::new(config::MAX_DB) });
        // double-click the dB label area resets to 0 dB
        let gest = gtk::GestureClick::new();
        let fw = Rc::downgrade(&f);
        gest.connect_pressed(move |_, n, _, _| {
            if n == 2 {
                if let Some(f) = fw.upgrade() {
                    f.scale.set_value(db_to_pos(0.0));
                }
            }
        });
        f.label.add_controller(gest);
        // double-click anywhere on the fader snaps to 0 dB
        let dbl = gtk::GestureClick::new();
        dbl.set_propagation_phase(gtk::PropagationPhase::Capture);
        let fw2 = Rc::downgrade(&f);
        dbl.connect_pressed(move |g, n, _, _| {
            if n == 2 {
                if let Some(f) = fw2.upgrade() {
                    f.scale.set_value(db_to_pos(0.0));
                    g.set_state(gtk::EventSequenceState::Claimed);
                }
            }
        });
        f.scale.add_controller(dbl);

        let (fw, app) = (Rc::downgrade(&f), self.clone());
        f.scale.connect_value_changed(move |s| {
            let Some(f) = fw.upgrade() else { return };
            // snap to unity, then the bus ceiling (#33: drag, scroll, double-click 0 dB and the SL MkIII all land here)
            let db = crate::busvol::write_db(pos_to_db(s.value()), f.max_db.get());
            f.label.set_label(&fmt_db(db));
            if f.updating.get() {
                return;
            }
            if let Some(p) = crate::busvol::snap_back(s.value(), db, f.max_db.get()) {
                // above the ceiling: the cap stops at it (rounding / the unity snap never move it back)
                f.updating.set(true);
                s.set_value(p);
                f.updating.set(false);
            }
            f.busy.set(6);
            apply(&app, db);
            let node = f.node.borrow().clone();
            if f.auto.get() {
                // auto moves are not his: saved at most once a minute (W8)
                app.norm_moved.set(true);
                if let Some(i) = app.cfg.borrow().inputs.iter().find(|i| names::strip_in(&i.id) == node) {
                    app.auto_gain.borrow_mut().insert(i.id.clone());
                }
            } else {
                app.mark_dirty();
                app.fader_manual(&node, db);
            }
            if let Some(id) = app.node_id(&node) {
                pw::set_volume(id, db_to_vol(db));
            }
            app.bus_wrote(&f);
        });
        if let Some(m) = &f.mute {
            let (fw, app) = (Rc::downgrade(&f), self.clone());
            m.connect_toggled(move |b| {
                let Some(f) = fw.upgrade() else { return };
                if f.updating.get() {
                    return;
                }
                f.busy.set(6);
                apply_mute(&app, b.is_active());
                app.mark_dirty();
                let node = f.node.borrow().clone();
                if let Some(id) = app.node_id(&node) {
                    match app.bus_effective_db(&f) {
                        // hardware bus: the saved (capped) volume goes in before the unmute (#33)
                        Some(db) => pw::set_volume_mute(id, db_to_vol(db), b.is_active()),
                        None => pw::set_mute(id, b.is_active()),
                    }
                }
                app.bus_wrote(&f);
            });
        }
        self.faders.borrow_mut().push(f.clone());
        (b, f)
    }

    fn colour_css(&self, cls: &str, colour: &str, out: &mut String) {
        out.push_str(&format!(
            ".{cls} .colorbar {{ background: {colour}; }}\n\
             .{cls} button.route:checked {{ background-color: {colour}; background-image: none; color: #111; box-shadow: none; }}\n"
        ));
    }

    fn input_strip(self: &Rc<Self>, idx: usize, css: &mut String) -> gtk::Box {
        let inp = self.cfg.borrow().inputs[idx].clone();
        let buses = self.cfg.borrow().buses.clone();
        let id = inp.id.clone();
        let strip = gtk::Box::new(gtk::Orientation::Vertical, 8);
        strip.add_css_class("strip");
        let cls = format!("in-{id}");
        strip.add_css_class(&cls);
        self.colour_css(&cls, &inp.color, css);
        strip.append(&screw_row());
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        bar.add_css_class("colorbar");
        strip.append(&bar);

        // name + settings popover
        let menu = gtk::MenuButton::builder().label(&inp.name).build();
        menu.add_css_class("flat");
        menu.add_css_class("flatname");
        menu.set_child(Some(&label(&inp.name, "name")));
        menu.set_popover(Some(&self.input_menu(idx)));
        strip.append(&menu);
        let sub = match &inp.kind {
            InputKind::Virtual => format!("FX · {}", inp.fx),
            InputKind::Source { .. } => format!("DEVICE · FX {}", inp.fx),
        };
        if id == crate::engine::PIANO_ID {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            row.set_halign(gtk::Align::Center);
            let names_: Vec<&str> = config::INSTRUMENTS.iter().map(|i| i.0).collect();
            let dd = gtk::DropDown::from_strings(&names_);
            dd.add_css_class("instrument");
            dd.set_tooltip_text(Some("Instrument your keyboard plays"));
            let cur = self.cfg.borrow().piano.clone();
            dd.set_selected(config::INSTRUMENTS.iter().position(|i| i.1 == cur.program).unwrap_or(0) as u32);
            let app = self.clone();
            dd.connect_selected_notify(move |d| {
                if let Some((_, prog)) = config::INSTRUMENTS.get(d.selected() as usize) {
                    app.cfg.borrow_mut().piano.program = *prog;
                    app.save_cfg(); // the engine picks it up within a second
                }
            });
            row.append(&dd);
            let app = self.clone();
            let rev = Knob::ranged(24, cur.reverb, 0.0, 1.0, 0.4, 0.01, move |v| {
                app.cfg.borrow_mut().piano.reverb = v;
                app.mark_dirty();
            });
            rev.area.set_tooltip_text(Some("Piano reverb (room) · double-click resets"));
            row.append(&rev.area);
            self.knobs.borrow_mut().push(rev);
            strip.append(&row);
        } else {
            strip.append(&label(&sub.to_uppercase(), "small"));
        }

        // 12-band EQ in a popover of knurled knobs
        let eq_btn = gtk::MenuButton::builder().label(eq_label(&inp.eq)).build();
        eq_btn.set_tooltip_text(Some("12-band EQ"));
        eq_btn.add_css_class("eqbtn");
        eq_btn.set_child(Some(&label(&eq_label(&inp.eq), "btnlabel")));
        {
            let (w, id2) = (Rc::downgrade(self), id.clone());
            eq_btn.set_create_popup_func(move |mb| {
                if let Some(app) = w.upgrade() {
                    let (name, eq) = app.cfg.borrow().inputs.iter().find(|i| i.id == id2).map(|i| (i.name.clone(), i.eq.clone())).unwrap_or_default();
                    mb.set_popover(Some(&app.eq_popover(&id2, &name, &eq, mb)));
                }
            });
        }
        let rack_btn = gtk::MenuButton::builder().label(rack_label(&inp.rack)).build();
        rack_btn.add_css_class("eqbtn");
        rack_btn.set_child(Some(&label(&rack_label(&inp.rack), "btnlabel")));
        {
            let (w, id2, name) = (Rc::downgrade(self), id.clone(), inp.name.clone());
            rack_btn.set_create_popup_func(move |mb| {
                if let Some(app) = w.upgrade() {
                    mb.set_popover(Some(&app.rack_popover(&id2, &name, mb)));
                }
            });
        }
        let gear = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        gear.set_homogeneous(true);
        gear.append(&eq_btn);
        gear.append(&rack_btn);
        strip.append(&gear);

        // rack status lights: PRE GATE COMP EXC (click to open the rack)
        let lights = gtk::Box::new(gtk::Orientation::Horizontal, 3);
        lights.set_halign(gtk::Align::Center);
        let leds: Vec<gtk::Label> = ["PRE", "GATE", "COMP", "EXC"].iter().map(|t| {
            let l = label(t, "led");
            lights.append(&l);
            l
        }).collect();
        let lights_btn = gtk::Button::builder().child(&lights).build();
        lights_btn.add_css_class("flat");
        lights_btn.add_css_class("lights");
        lights_btn.set_tooltip_text(Some("Outboard rack: lit = on. Click to open."));
        {
            let rb = rack_btn.clone();
            lights_btn.connect_clicked(move |_| rb.popup());
        }
        strip.append(&lights_btn);
        {
            let (w, id2, last) = (Rc::downgrade(self), id.clone(), Cell::new(255u8));
            let bits_of = |r: &config::Rack| [r.pre_on, r.gate_on, r.comp_on, r.exc_on].iter().enumerate().fold(0u8, |b, (k, on)| b | ((*on as u8) << k));
            self.console_ticks.borrow_mut().push(Box::new(move |_| {
                let Some(app) = w.upgrade() else { return };
                // read the flags in place: no Rack clone on every frame
                let bits = app.cfg.borrow().inputs.iter().find(|i| i.id == id2).map(|i| bits_of(&i.rack)).unwrap_or_else(|| bits_of(&Default::default()));
                if bits != last.get() {
                    last.set(bits);
                    for (k, l) in leds.iter().enumerate() {
                        if bits & (1 << k) != 0 { l.add_css_class("on") } else { l.remove_css_class("on") }
                    }
                }
            }));
        }

        let vu = Vu::new(104);
        strip.append(&vu.area);
        self.keep.borrow_mut().push(vu.clone());

        // IN tap: what the app/device sends before any processing
        let in_led = LedMeter::new();
        self.keep.borrow_mut().push(in_led.clone());
        let (pre_node, pre_sink) = match &inp.kind {
            InputKind::Virtual => (names::ch(&id), true),
            InputKind::Source { target } => (target.clone(), false),
        };
        if let Some(m) = self.taps_on.get().then(|| meter(&format!("meter_pre_{id}"), Some((&pre_node, pre_sink)), Some(in_led.clone()), None, self.side_cell(&format!("in:{id}")))).flatten() {
            self.levels.borrow_mut().insert(format!("pre:{id}"), m.out.clone());
            self.spectra.borrow_mut().insert(format!("pre:{id}"), m.bands.clone());
            self.meters.borrow_mut().push(m);
        }

        // meter + gain fader
        let led = LedMeter::new();
        self.keep.borrow_mut().push(led.clone());
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        row.set_halign(gtk::Align::Center);
        row.set_vexpand(true);
        let meter_name = format!("meter_{id}");
        if let Some(m) = self.taps_on.get().then(|| meter(&meter_name, None, Some(led.clone()), Some(vu.clone()), self.side_cell(&format!("in:{id}")))).flatten() {
            self.levels.borrow_mut().insert(format!("post:{id}"), m.out.clone());
            self.spectra.borrow_mut().insert(format!("post:{id}"), m.bands.clone());
            self.meter_links.borrow_mut().push((names::strip(&id), m.node.clone()));
            self.meters.borrow_mut().push(m);
        }
        // compressor GR needle + gate lamp, then the mini before/after analyser
        let gr_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        gr_row.set_halign(gtk::Align::Center);
        gr_row.append(&self.gauge_into(&id, "WL-3", true, 58));
        gr_row.append(&self.gauge_into(&id, "WL-2", true, 52));
        strip.append(&gr_row);
        if !self.rack_visible.get() {
            let (pre, post) = self.tap_spectra(&id);
            let (w, id2) = (Rc::downgrade(self), id.clone());
            let ana = Analyzer::new(pre, post, hex_rgb(&inp.color), move || {
                w.upgrade().and_then(|a| a.cfg.borrow().inputs.iter().find(|i| i.id == id2).map(|i| i.eq.clone())).unwrap_or_default()
            }, config::EQ_BANDS);
            ana.area.set_content_height(32);
            ana.area.add_css_class("mini-ana");
            strip.append(&ana.area);
            self.console_ticks.borrow_mut().push(Box::new(move |dt| ana.step(dt)));
        }

        let (i1, i2) = (id.clone(), id.clone());
        let (fb, fader_rc) = self.fader(
            names::strip_in(&id),
            100,
            true,
            move |a, db| a.cfg.borrow_mut().inputs.iter_mut().find(|i| i.id == i1).unwrap().gain_db = db,
            move |a, m| a.cfg.borrow_mut().inputs.iter_mut().find(|i| i.id == i2).unwrap().mute = m,
        );
        // auto-normalize: the IN tap's pre-fader RMS drives the real slider (no extra tap)
        // auto-normalize: one channel per input for the whole session (N only gates `step`), resumed from the saved state
        if !self.norm.borrow().contains_key(&id) {
            self.norm.borrow_mut().insert(id.clone(), crate::norm::Channel::resume(inp.gain_db, inp.norm_base, inp.norm_out));
            self.norm_store(&id);
        }
        if let Some(pre) = self.levels.borrow().get(&format!("pre:{id}")).cloned() {
            let (w, id2, fw) = (Rc::downgrade(self), id.clone(), Rc::downgrade(&fader_rc));
            self.console_ticks.borrow_mut().push(Box::new(move |dt| {
                let (Some(app), Some(f)) = (w.upgrade(), fw.upgrade()) else { return };
                let on = app.cfg.borrow().inputs.iter().any(|i| i.id == id2 && i.normalize);
                let (g, anchored) = match app.norm.borrow_mut().get_mut(&id2) {
                    Some(c) if on => {
                        let was = c.pending;
                        let g = c.step(pre.get().1, dt);
                        (g, was && !c.pending)
                    }
                    _ => (None, false),
                };
                if anchored {
                    app.norm_store(&id2);
                    app.norm_moved.set(true);
                }
                if let Some(g) = g {
                    f.set_auto(g);
                }
            }));
        }
        // IN meter | fader | OUT meter, each meter labelled underneath and kept
        // clear of the dB readout + mute button below the fader slot
        let meter_col = |m: &Rc<LedMeter>, t: &str| {
            let col = gtk::Box::new(gtk::Orientation::Vertical, 2);
            col.set_margin_top(8);
            col.set_margin_bottom(52);
            m.area.set_vexpand(true);
            // centred: the OUT label is wider than the 18 px meter, and a filling child would widen its bars
            m.area.set_halign(gtk::Align::Center);
            col.append(&m.area);
            col.append(&label(t, "small"));
            col
        };
        row.append(&meter_col(&in_led, "IN"));
        row.append(&fb);
        row.append(&meter_col(&led, "OUT"));
        strip.append(&row);
        strip.append(&self.side_row(format!("in:{id}")));

        // sends: one boxed cell per bus — on/off button on top, its level knob, then the level
        let grid = gtk::Grid::builder().column_spacing(2).row_spacing(0).halign(gtk::Align::Center).build();
        for (n, b) in buses.iter().enumerate() {
            let r = inp.route(&b.id);
            let cell = gtk::Box::new(gtk::Orientation::Vertical, 2);
            cell.add_css_class("sendcell");
            cell.set_tooltip_text(Some(&format!("{} → {} {}", inp.name, b.id.to_uppercase(), b.name)));
            let t = gtk::ToggleButton::with_label(&b.id.to_uppercase());
            t.add_css_class("route");
            t.set_tooltip_text(Some(&format!("Send {} to {} (on/off)", inp.name, b.name)));
            t.set_active(r.on);
            cell.append(&t);
            let val = label("", "sendval");
            val.set_width_chars(4);
            let show = {
                let val = val.clone();
                move |on: bool, db: f64| val.set_label(&if !on { "off".to_string() } else if db <= config::MIN_DB { "-∞".to_string() } else if db.abs() >= 10.0 { format!("{db:+.0}") } else { format!("{db:+.1}") })
            };
            show(r.on, r.level_db);
            let (app, iid, bid, show2, t2) = (self.clone(), id.clone(), b.id.clone(), show.clone(), t.clone());
            let knob = Knob::ranged(28, db_to_pos(r.level_db), 0.0, 1.0, db_to_pos(0.0), 0.005, move |p| {
                let db = pos_to_db(p);
                let db = if db.abs() < 0.25 { 0.0 } else { (db * 10.0).round() / 10.0 };
                {
                    let mut c = app.cfg.borrow_mut();
                    let i = c.inputs.iter_mut().find(|i| i.id == iid).unwrap();
                    i.routes.entry(bid.clone()).or_insert(Route { on: false, level_db: 0.0 }).level_db = db;
                }
                show2(t2.is_active(), db);
                app.mark_dirty();
                if let Some(nid) = app.node_id(&names::route(&iid, &bid)) {
                    pw::set_volume(nid, db_to_vol(db));
                }
            });
            knob.area.set_tooltip_text(Some(&format!("Send level to {} · drag/scroll · double-click = 0 dB", b.name)));
            knob.area.set_opacity(if r.on { 1.0 } else { 0.35 });
            knob.enable_halo();
            cell.append(&knob.area);
            cell.append(&val);
            // visuals follow the switch however it changes (click or live sync)
            {
                let (ka, app, iid, bid, show3) = (knob.area.clone(), Rc::downgrade(self), id.clone(), b.id.clone(), show.clone());
                t.connect_toggled(move |t| {
                    let on = t.is_active();
                    ka.set_opacity(if on { 1.0 } else { 0.35 });
                    let db = app.upgrade().and_then(|a| a.cfg.borrow().inputs.iter().find(|i| i.id == iid).map(|i| i.route(&bid).level_db)).unwrap_or(0.0);
                    show3(on, db);
                });
            }
            self.send_knobs.borrow_mut().insert((id.clone(), b.id.clone()), knob.clone());
            // send halo: the post-strip peak plus this send's level, on the frame tick
            if let Some(post) = self.levels.borrow().get(&format!("post:{id}")).cloned() {
                let (w, iid, bid, k) = (Rc::downgrade(self), id.clone(), b.id.clone(), Rc::downgrade(&knob));
                self.console_ticks.borrow_mut().push(Box::new(move |dt| {
                    let (Some(app), Some(k)) = (w.upgrade(), k.upgrade()) else { return };
                    let r = app.cfg.borrow().inputs.iter().find(|i| i.id == iid).map(|i| i.route(&bid));
                    if let Some(r) = r {
                        k.halo_step(crate::dash::send_db(post.get().0, r.on, r.level_db), dt);
                    }
                }));
            }
            self.knobs.borrow_mut().push(knob);
            let guard = Rc::new(Cell::new(false));
            let (app, iid, bid, g2) = (self.clone(), id.clone(), b.id.clone(), guard.clone());
            t.connect_toggled(move |t| {
                if g2.get() {
                    return;
                }
                let on = t.is_active();
                {
                    let mut c = app.cfg.borrow_mut();
                    let i = c.inputs.iter_mut().find(|i| i.id == iid).unwrap();
                    i.routes.entry(bid.clone()).or_insert(Route { on, level_db: 0.0 }).on = on;
                }
                app.mark_dirty();
                if let Some(nid) = app.node_id(&names::route(&iid, &bid)) {
                    pw::set_mute(nid, !on);
                }
            });
            self.routes.borrow_mut().push((names::route(&id, &b.id), t.clone(), guard));
            grid.attach(&cell, n as i32, 0, 1, 1);
        }
        strip.append(&grid);
        // S (solo) next to M
        {
            let solo_btn = gtk::ToggleButton::with_label("S");
            solo_btn.add_css_class("solo");
            solo_btn.set_tooltip_text(Some("Solo: hear only this channel (temporary, not saved)"));
            solo_btn.set_active(self.solo.borrow().contains(&id));
            let (app, id2) = (self.clone(), id.clone());
            solo_btn.connect_toggled(move |b| {
                {
                    let mut s = app.solo.borrow_mut();
                    if b.is_active() { s.insert(id2.clone()); } else { s.remove(&id2); }
                }
                app.apply_solo();
            });
            fader_rc.btns.append(&solo_btn);
            self.solo_btns.borrow_mut().insert(id.clone(), solo_btn.clone());
            // N (auto-normalize): on = the slider drifts to even loudness; saved with the input
            let n_btn = gtk::ToggleButton::with_label("N");
            n_btn.add_css_class("norm");
            n_btn.set_tooltip_text(Some("Auto-normalize: moves this slider to keep the channel at an even loudness (saved)"));
            n_btn.set_active(inp.normalize);
            {
                let (app, id2) = (self.clone(), id.clone());
                n_btn.connect_toggled(move |b| {
                    let on = b.is_active();
                    {
                        let mut c = app.cfg.borrow_mut();
                        if let Some(i) = c.inputs.iter_mut().find(|i| i.id == id2) {
                            i.normalize = on;
                        }
                    }
                    app.mark_dirty();
                    if !on {
                        // N off: keep the channel (his position, his level, a running hold); forget only the loudness
                        if let Some(c) = app.norm.borrow_mut().get_mut(&id2) {
                            c.pause();
                        }
                    }
                });
            }
            self.norm_btns.borrow_mut().insert(id.clone(), n_btn.clone());
            fader_rc.btns.append(&n_btn);
        }
        strip.append(&screw_row());
        strip.set_size_request(140, -1);
        strip
    }

    fn input_menu(self: &Rc<Self>, idx: usize) -> gtk::Popover {
        let inp = self.cfg.borrow().inputs[idx].clone();
        let pop = gtk::Popover::new();
        let b = gtk::Box::new(gtk::Orientation::Vertical, 8);
        for m in ["top", "bottom", "start", "end"] {
            match m {
                "top" => b.set_margin_top(10),
                "bottom" => b.set_margin_bottom(10),
                "start" => b.set_margin_start(10),
                _ => b.set_margin_end(10),
            }
        }
        b.append(&label("NAME", "small"));
        let entry = gtk::Entry::builder().text(&inp.name).build();
        b.append(&entry);
        b.append(&label("EFFECTS PRESET", "small"));
        let fx = gtk::DropDown::from_strings(FX_PRESETS);
        fx.set_selected(FX_PRESETS.iter().position(|p| *p == inp.fx).unwrap_or(0) as u32);
        b.append(&fx);
        let mut src_dd = None;
        if let InputKind::Source { target } = &inp.kind {
            b.append(&label("CAPTURE FROM", "small"));
            let srcs: Vec<(String, String)> = self.graph.borrow().sources().iter().map(|n| (n.name.clone(), if n.desc.is_empty() { n.name.clone() } else { n.desc.clone() })).collect();
            let mut list: Vec<(String, String)> = srcs;
            if !list.iter().any(|s| &s.0 == target) {
                list.insert(0, (target.clone(), target.clone()));
            }
            let labels: Vec<&str> = list.iter().map(|s| s.1.as_str()).collect();
            let dd = gtk::DropDown::from_strings(&labels);
            dd.set_selected(list.iter().position(|s| &s.0 == target).unwrap_or(0) as u32);
            b.append(&dd);
            src_dd = Some((dd, list));
        }
        let colours = ["#8e8e93", "#1ed760", "#5865f2", "#ff9500", "#ff2d55", "#bf5af2", "#64d2ff", "#ffd60a", "#30d158", "#ff453a"];
        b.append(&label("COLOUR", "small"));
        let crow = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        let chosen = Rc::new(RefCell::new(inp.color.clone()));
        for c in colours {
            let btn = gtk::Button::new();
            btn.set_size_request(20, 20);
            let p = gtk::CssProvider::new();
            p.load_from_string(&format!("button {{ background: {c}; min-width: 18px; min-height: 18px; padding: 0; border-radius: 9px; }}"));
            #[allow(deprecated)]
            btn.style_context().add_provider(&p, gtk::STYLE_PROVIDER_PRIORITY_USER + 1);
            let ch = chosen.clone();
            btn.connect_clicked(move |_| *ch.borrow_mut() = c.to_string());
            crow.append(&btn);
        }
        b.append(&crow);
        let apply = gtk::Button::with_label("Apply");
        apply.add_css_class("suggested-action");
        let remove = gtk::Button::with_label("Remove input");
        remove.add_css_class("destructive-action");
        b.append(&apply);
        if !inp.default_sink {
            b.append(&remove);
        }
        pop.set_child(Some(&b));

        let (app, p2, id) = (self.clone(), pop.clone(), inp.id.clone());
        apply.connect_clicked(move |_| {
            {
                let mut c = app.cfg.borrow_mut();
                let i = c.inputs.iter_mut().find(|i| i.id == id).unwrap();
                let name = entry.text().trim().to_string();
                if !name.is_empty() {
                    i.name = name;
                }
                i.fx = FX_PRESETS[fx.selected() as usize].to_string();
                i.color = chosen.borrow().clone();
                if let Some((dd, list)) = &src_dd {
                    i.kind = InputKind::Source { target: list[dd.selected() as usize].0.clone() };
                }
            }
            p2.popdown();
            app.structural();
        });
        let (app, p2, id) = (self.clone(), pop.clone(), inp.id.clone());
        remove.connect_clicked(move |_| {
            app.cfg.borrow_mut().inputs.retain(|i| i.id != id);
            app.norm.borrow_mut().remove(&id);
            p2.popdown();
            app.structural();
        });
        pop
    }

    fn bus_strip(self: &Rc<Self>, idx: usize, css: &mut String) -> gtk::Box {
        let bus = self.cfg.borrow().buses[idx].clone();
        let strip = gtk::Box::new(gtk::Orientation::Vertical, 8);
        strip.add_css_class("strip");
        strip.add_css_class("bus");
        let cls = format!("bus-{}", bus.id);
        strip.add_css_class(&cls);
        self.colour_css(&cls, &bus.color, css);
        strip.append(&screw_row());
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        bar.add_css_class("colorbar");
        strip.append(&bar);
        let menu = gtk::MenuButton::new();
        menu.add_css_class("flat");
        menu.add_css_class("flatname");
        menu.set_child(Some(&label(&format!("{} · {}", bus.id.to_uppercase(), bus.name), "name")));
        menu.set_popover(Some(&self.bus_menu(idx)));
        strip.append(&menu);
        let sub = match &bus.kind {
            BusKind::Hardware { target } => {
                let g = self.graph.borrow();
                let d = g.node(target).map(|n| n.desc.clone()).unwrap_or_else(|| "device not connected".into());
                d.chars().take(22).collect::<String>()
            }
            BusKind::Virtual { description, .. } => format!("mic: {description}"),
        };
        let s = label(&sub, "small");
        s.set_ellipsize(gtk::pango::EllipsizeMode::End);
        s.set_max_width_chars(18);
        strip.append(&s);
        let vu = Vu::new(100);
        strip.append(&vu.area);
        self.keep.borrow_mut().push(vu.clone());

        let led = LedMeter::new();
        self.keep.borrow_mut().push(led.clone());
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        row.set_halign(gtk::Align::Center);
        row.set_vexpand(true);
        if let Some(m) = meter(&format!("meter_bus_{}", bus.id), Some((&names::bus(&bus.id), true)), Some(led.clone()), Some(vu.clone()), self.side_cell(&format!("bus:{}", bus.id))) {
            // the bottom spectrum reads this tap's bands (bus taps run on every page)
            self.spectra.borrow_mut().insert(format!("bus:{}", names::bus(&bus.id)), m.bands.clone());
            self.meters.borrow_mut().push(m);
        }
        let (b1, b2) = (bus.id.clone(), bus.id.clone());
        let (fb, bus_fader) = self.fader(
            bus_fader_node(&bus),
            220,
            true,
            move |a, db| a.cfg.borrow_mut().buses.iter_mut().find(|b| b.id == b1).unwrap().gain_db = db,
            move |a, m| a.cfg.borrow_mut().buses.iter_mut().find(|b| b.id == b2).unwrap().mute = m,
        );
        if matches!(bus.kind, BusKind::Hardware { .. }) {
            // the fader is the device volume: config is the source of truth (#33)
            *bus_fader.bus.borrow_mut() = Some(bus.id.clone());
        }
        let ceil = bus.ceiling_db();
        bus_fader.max_db.set(ceil);
        if ceil < config::MAX_DB {
            bus_fader.scale.add_mark(db_to_pos(ceil), gtk::PositionType::Right, Some("max"));
            bus_fader.scale.set_tooltip_text(Some(&format!("Ceiling {} (this output never goes louder)", fmt_db(ceil))));
        }
        row.append(&fb);
        // meter sits beside the fader slot (clear of the dB readout + mute below it)
        led.area.set_margin_top(8);
        led.area.set_margin_bottom(66);
        row.append(&led.area);
        strip.append(&row);
        // L/R solo only on the hardware outputs (headphones, speakers); the stream buses never change
        if matches!(bus.kind, BusKind::Hardware { .. }) {
            strip.append(&self.side_row(format!("bus:{}", bus.id)));
        }
        strip.append(&screw_row());
        strip.set_size_request(130, -1);
        strip
    }

    fn bus_menu(self: &Rc<Self>, idx: usize) -> gtk::Popover {
        let bus = self.cfg.borrow().buses[idx].clone();
        let pop = gtk::Popover::new();
        let b = gtk::Box::new(gtk::Orientation::Vertical, 8);
        b.set_margin_top(10);
        b.set_margin_bottom(10);
        b.set_margin_start(10);
        b.set_margin_end(10);
        b.append(&label("NAME", "small"));
        let entry = gtk::Entry::builder().text(&bus.name).build();
        b.append(&entry);
        let dev;
        match &bus.kind {
            BusKind::Hardware { target } => {
                b.append(&label("OUTPUT DEVICE", "small"));
                let mut list: Vec<(String, String)> = self.graph.borrow().hardware_sinks().iter().map(|n| (n.name.clone(), n.desc.clone())).collect();
                if !list.iter().any(|s| &s.0 == target) {
                    list.insert(0, (target.clone(), format!("{target} (not connected)")));
                }
                let labels: Vec<&str> = list.iter().map(|s| s.1.as_str()).collect();
                let dd = gtk::DropDown::from_strings(&labels);
                dd.set_selected(list.iter().position(|s| &s.0 == target).unwrap_or(0) as u32);
                b.append(&dd);
                dev = Some((dd, list));
            }
            BusKind::Virtual { description, .. } => {
                b.append(&label("VIRTUAL MIC NAME (shown in OBS / Discord)", "small"));
                let e2 = gtk::Entry::builder().text(description).build();
                b.append(&e2);
                dev = None;
                let (app, id) = (self.clone(), bus.id.clone());
                e2.connect_activate(move |e| {
                    if let Some(bu) = app.cfg.borrow_mut().buses.iter_mut().find(|b| b.id == id) {
                        if let BusKind::Virtual { description, .. } = &mut bu.kind {
                            *description = e.text().to_string();
                        }
                    }
                    app.structural();
                });
            }
        }
        let apply = gtk::Button::with_label("Apply");
        apply.add_css_class("suggested-action");
        let remove = gtk::Button::with_label("Remove bus");
        remove.add_css_class("destructive-action");
        b.append(&apply);
        b.append(&remove);
        pop.set_child(Some(&b));
        let (app, p2, id) = (self.clone(), pop.clone(), bus.id.clone());
        apply.connect_clicked(move |_| {
            {
                let mut c = app.cfg.borrow_mut();
                let bu = c.buses.iter_mut().find(|b| b.id == id).unwrap();
                let name = entry.text().trim().to_string();
                if !name.is_empty() {
                    bu.name = name;
                }
                if let Some((dd, list)) = &dev {
                    bu.kind = BusKind::Hardware { target: list[dd.selected() as usize].0.clone() };
                }
            }
            p2.popdown();
            app.structural();
        });
        let (app, p2, id) = (self.clone(), pop.clone(), bus.id.clone());
        remove.connect_clicked(move |_| {
            {
                let mut c = app.cfg.borrow_mut();
                c.buses.retain(|b| b.id != id);
                for i in &mut c.inputs {
                    i.routes.remove(&id);
                }
            }
            p2.popdown();
            app.structural();
        });
        pop
    }

    fn add_input_popover(self: &Rc<Self>) -> gtk::Popover {
        let pop = gtk::Popover::new();
        let b = gtk::Box::new(gtk::Orientation::Vertical, 8);
        b.set_margin_top(10);
        b.set_margin_bottom(10);
        b.set_margin_start(10);
        b.set_margin_end(10);
        b.append(&label("NEW INPUT", "small"));
        let entry = gtk::Entry::builder().placeholder_text("Name, e.g. Music 2").build();
        b.append(&entry);
        let opts = Rc::new(RefCell::new(Vec::<(String, String)>::new()));
        let dd = gtk::DropDown::from_strings(&["Virtual input (apps play into it)"]);
        b.append(&dd);
        let add = gtk::Button::with_label("Add input");
        add.add_css_class("suggested-action");
        b.append(&add);
        pop.set_child(Some(&b));
        let (app, o2, dd2) = (self.clone(), opts.clone(), dd.clone());
        pop.connect_show(move |_| {
            let mut list = vec![(String::new(), "Virtual input (apps play into it)".to_string())];
            for n in app.graph.borrow().sources() {
                list.push((n.name.clone(), format!("Device: {}", if n.desc.is_empty() { &n.name } else { &n.desc })));
            }
            let labels: Vec<&str> = list.iter().map(|s| s.1.as_str()).collect();
            dd2.set_model(Some(&gtk::StringList::new(&labels)));
            *o2.borrow_mut() = list;
        });
        let (app, p2) = (self.clone(), pop.clone());
        add.connect_clicked(move |_| {
            let name = entry.text().trim().to_string();
            let name = if name.is_empty() { "Input".to_string() } else { name };
            let sel = opts.borrow().get(dd.selected() as usize).map(|s| s.0.clone()).unwrap_or_default();
            {
                let mut c = app.cfg.borrow_mut();
                let id = c.new_id(&name);
                let mut routes = std::collections::BTreeMap::new();
                if let Some(first) = c.buses.first() {
                    routes.insert(first.id.clone(), Route { on: true, level_db: 0.0 });
                }
                c.inputs.push(Input {
                    id,
                    name,
                    color: "#64d2ff".into(),
                    kind: if sel.is_empty() { InputKind::Virtual } else { InputKind::Source { target: sel } },
                    fx: "clean".into(),
                    eq: config::flat_eq(),
                    gain_db: 0.0,
                    mute: false,
                    routes,
                    default_sink: false,
                    rack: config::Rack::default(),
                    normalize: false,
                    norm_base: None,
                    norm_out: None,
                });
            }
            entry.set_text("");
            p2.popdown();
            app.structural();
        });
        pop
    }

    fn add_bus_popover(self: &Rc<Self>) -> gtk::Popover {
        let pop = gtk::Popover::new();
        let b = gtk::Box::new(gtk::Orientation::Vertical, 8);
        b.set_margin_top(10);
        b.set_margin_bottom(10);
        b.set_margin_start(10);
        b.set_margin_end(10);
        b.append(&label("NEW BUS", "small"));
        let entry = gtk::Entry::builder().placeholder_text("Name, e.g. Speakers").build();
        b.append(&entry);
        let opts = Rc::new(RefCell::new(Vec::<(String, String)>::new()));
        let dd = gtk::DropDown::from_strings(&["Virtual mic (for OBS / Discord)"]);
        b.append(&dd);
        let add = gtk::Button::with_label("Add bus");
        add.add_css_class("suggested-action");
        b.append(&add);
        pop.set_child(Some(&b));
        let (app, o2, dd2) = (self.clone(), opts.clone(), dd.clone());
        pop.connect_show(move |_| {
            let mut list = vec![(String::new(), "Virtual mic (for OBS / Discord)".to_string())];
            for n in app.graph.borrow().hardware_sinks() {
                list.push((n.name.clone(), format!("Output: {}", n.desc)));
            }
            let labels: Vec<&str> = list.iter().map(|s| s.1.as_str()).collect();
            dd2.set_model(Some(&gtk::StringList::new(&labels)));
            *o2.borrow_mut() = list;
        });
        let (app, p2) = (self.clone(), pop.clone());
        add.connect_clicked(move |_| {
            let sel = opts.borrow().get(dd.selected() as usize).map(|s| s.0.clone()).unwrap_or_default();
            {
                let mut c = app.cfg.borrow_mut();
                let hw = !sel.is_empty();
                let prefix = if hw { "a" } else { "b" };
                let n = (1..).find(|n| !c.buses.iter().any(|b| b.id == format!("{prefix}{n}"))).unwrap();
                let id = format!("{prefix}{n}");
                let name = entry.text().trim().to_string();
                let name = if name.is_empty() { id.to_uppercase() } else { name };
                let kind = if hw {
                    BusKind::Hardware { target: sel }
                } else {
                    BusKind::Virtual { node: format!("wavelink_{id}"), description: name.clone() }
                };
                c.buses.push(Bus::new(id, name, if hw { "#64d2ff".into() } else { "#ffd60a".into() }, kind));
                c.buses.sort_by(|a, b| a.id.cmp(&b.id));
            }
            entry.set_text("");
            p2.popdown();
            app.structural();
        });
        pop
    }

    fn rebuild(self: &Rc<Self>) {
        while let Some(c) = self.content.first_child() {
            self.content.remove(&c);
        }
        self.faders.borrow_mut().clear();
        self.routes.borrow_mut().clear();
        self.meters.borrow_mut().clear();
        self.knobs.borrow_mut().clear();
        self.keep.borrow_mut().clear();
        self.solo_btns.borrow_mut().clear();
        self.send_knobs.borrow_mut().clear();
        self.norm_btns.borrow_mut().clear();
        self.levels.borrow_mut().clear();
        self.spectra.borrow_mut().clear();
        self.console_ticks.borrow_mut().clear();
        self.meter_links.borrow_mut().clear();
        self.side_btns.borrow_mut().clear();
        self.side_clear.borrow_mut().take();
        // auto-normalize's save, once a minute (W8): auto moves never write config per tick
        {
            let w = Rc::downgrade(self);
            self.console_ticks.borrow_mut().push(Box::new(move |dt| {
                let Some(app) = w.upgrade() else { return };
                app.norm_save.set(app.norm_save.get() + dt);
                if app.norm_save.get() >= 60.0 {
                    app.norm_save.set(0.0);
                    if app.norm_moved.replace(false) {
                        app.mark_dirty();
                    }
                }
            }));
        }
        let mut css = String::new();

        let ins = gtk::Box::new(gtk::Orientation::Vertical, 6);
        ins.append(&{
            let l = label("INPUTS", "section");
            l.set_xalign(0.0);
            l
        });
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.set_vexpand(true);
        let n_in = self.cfg.borrow().inputs.len();
        for i in 0..n_in {
            row.append(&self.input_strip(i, &mut css));
        }
        ins.append(&row);
        let sep = gtk::Separator::new(gtk::Orientation::Vertical);
        sep.set_margin_start(6);
        sep.set_margin_end(6);
        let outs = gtk::Box::new(gtk::Orientation::Vertical, 6);
        // OUTPUTS header with the one-click clear for every L/R solo (shown only while one is set)
        let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        head.append(&{
            let l = label("OUTPUTS", "section");
            l.set_xalign(0.0);
            l
        });
        let clr = gtk::Button::with_label("L/R SOLO OFF");
        clr.add_css_class("solo");
        let any_side = self.sides.borrow().values().any(|c| c.get() != Side::Stereo);
        clr.set_visible(any_side);
        {
            let w = Rc::downgrade(self);
            clr.connect_clicked(move |_| {
                let Some(app) = w.upgrade() else { return };
                for c in app.sides.borrow().values() {
                    c.set(Side::Stereo);
                }
                for b in app.side_btns.borrow().iter() {
                    b.set_active(false);
                }
                app.apply_sides(&app.graph.borrow());
            });
        }
        head.append(&clr);
        *self.side_clear.borrow_mut() = Some(clr);
        outs.append(&head);
        let row2 = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row2.set_vexpand(true);
        let n_b = self.cfg.borrow().buses.len();
        for i in 0..n_b {
            row2.append(&self.bus_strip(i, &mut css));
        }
        outs.append(&row2);
        // system gauges under the output strips (one panel, re-homed on every rebuild)
        if let Some(parent) = self.sys_panel.parent() {
            if let Ok(b) = parent.downcast::<gtk::Box>() {
                b.remove(&self.sys_panel);
            }
        }
        outs.append(&self.sys_panel);
        let all = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        all.append(&ins);
        all.append(&sep);
        all.append(&outs);
        let sc = gtk::ScrolledWindow::builder().child(&all).hexpand(true).vexpand(true).vscrollbar_policy(gtk::PolicyType::Automatic).build();
        self.content.append(&sc);
        self.css.load_from_string(&css);
        self.rebuild_rack();
        let g = self.graph.borrow().clone();
        self.sync(&g);
        crate::leak::EXPECTED_TAPS.store(self.meters.borrow().len() as u64, std::sync::atomic::Ordering::Relaxed);
    }

    /// One 19" rack row per input: nameplate, preamp, gate, compressor, exciter, 12-band EQ.
    fn rebuild_rack(self: &Rc<Self>) {
        while let Some(c) = self.rack_page.first_child() {
            self.rack_page.remove(&c);
        }
        self.rack_ticks.borrow_mut().clear();
        self.rack_meters.borrow_mut().clear();
        let live = self.rack_visible.get();
        let inputs: Vec<(String, String)> = self.cfg.borrow().inputs.iter().map(|i| (i.id.clone(), i.name.clone())).collect();
        for (id, name) in inputs {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            row.add_css_class("rackrow");
            // nameplate in the channel colour
            let plate = gtk::Box::new(gtk::Orientation::Vertical, 4);
            plate.add_css_class("nameplate");
            plate.add_css_class(&format!("in-{id}"));
            plate.set_size_request(86, -1);
            let bar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            bar.add_css_class("colorbar");
            plate.append(&bar);
            let n = label(&name, "name");
            n.set_xalign(0.0);
            n.set_vexpand(true);
            n.set_valign(gtk::Align::Center);
            plate.append(&n);
            let st = Rc::downgrade(self);
            let preset = self.rack_preset_menu(&id, move || {
                if let Some(st) = st.upgrade() {
                    glib::idle_add_local_once(move || st.rebuild());
                }
            });
            preset.set_tooltip_text(Some("Load a whole-rack preset"));
            plate.append(&preset);
            row.append(&plate);
            row.append(&self.rack_units(&id, None, gtk::Orientation::Horizontal, live));
            let eq = self.cfg.borrow().inputs.iter().find(|i| i.id == id).map(|i| i.eq.clone()).unwrap_or_default();
            let eqp = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            eqp.add_css_class("rackunit");
            let eq_box = self.eq_panel(&id, &name, &eq, None);
            if live {
                // before/after: what comes in vs what leaves the channel, with the EQ curve
                let colour = self.cfg.borrow().inputs.iter().find(|i| i.id == id).map(|i| hex_rgb(&i.color)).unwrap_or((0.8, 0.8, 0.8));
                let (pre, post) = self.tap_spectra(&id);
                let (w, id2) = (Rc::downgrade(self), id.clone());
                let ana = Analyzer::new(pre, post, colour, move || {
                    w.upgrade().and_then(|a| a.cfg.borrow().inputs.iter().find(|i| i.id == id2).map(|i| i.eq.clone())).unwrap_or_default()
                }, config::EQ_BANDS);
                eq_box.append(&ana.area);
                self.rack_ticks.borrow_mut().push(Box::new(move |dt| ana.step(dt)));
            }
            eqp.append(&eq_box);
            row.append(&eqp);
            self.rack_page.append(&row);
        }
    }

    fn sync(&self, g: &Graph) {
        for f in self.faders.borrow().iter() {
            let bus = f.bus.borrow().clone();
            match bus {
                Some(id) => self.sync_bus(f, g, &id),
                None => {
                    // inputs and virtual buses: the slider shows the saved gain too, so a click/scroll never
                    // saves a device artefact (#33); mute still shows the node's (solo / stems mute it)
                    let saved = self.saved_gain(&f.node.borrow());
                    match saved {
                        Some(db) => f.sync_saved(g, db),
                        None => f.sync(g),
                    }
                }
            }
        }
        for (node, t, guard) in self.routes.borrow().iter() {
            if let Some(n) = g.node(node) {
                if t.is_active() == n.mute {
                    guard.set(true);
                    t.set_active(!n.mute);
                    guard.set(false);
                }
            }
        }
        // meters whose strip and tap both exist but are not linked: the leak watch warns after two scans
        let mut unlinked = 0u64;
        for (out, inp) in self.meter_links.borrow().iter() {
            if g.has(out) && g.has(inp) && !g.linked(out, inp) {
                unlinked += 1;
                let (o, i) = (out.clone(), inp.clone());
                std::thread::spawn(move || pw::link(&o, &i));
            }
        }
        crate::leak::UNLINKED.store(unlinked, std::sync::atomic::Ordering::Relaxed);
        self.apply_sides(g);
        self.sync_apps(g);
        self.sync_patch(g);
    }

    /// Feed the patch bay from config (routes) and the live graph (which app plays where).
    fn sync_patch(&self, g: &Graph) {
        let cfg = self.cfg.borrow();
        let ours = |n: &str| ["route_", "strip_", "bus_", "meter_", "wavelink", "mix_", "voice_chat", "ee_", "easyeffects", "send_", "spectrum"].iter().any(|p| n.starts_with(p));
        let mut apps = vec![];
        for n in g.nodes.iter().filter(|n| n.class == "Stream/Output/Audio" && !ours(&n.name) && !n.app.is_empty() && n.app != "pipewire" && n.app != "pw-loopback") {
            for t in g.targets_of(n.id) {
                if let Some(k) = cfg.inputs.iter().position(|i| names::ch(&i.id) == t.name) {
                    if !apps.iter().any(|(a, kk, _): &(String, usize, u64)| a == &n.app && *kk == k) {
                        apps.push((n.app.clone(), k, n.serial));
                    }
                }
            }
        }
        let buses: Vec<(String, String)> = cfg
            .buses
            .iter()
            .map(|b| {
                let dest = match &b.kind {
                    BusKind::Hardware { target } => g.node(target).map(|n| n.desc.clone()).unwrap_or_else(|| "(not connected)".into()),
                    BusKind::Virtual { description, .. } => format!("{description} (virtual mic)"),
                };
                (format!("{} · {}", b.id.to_uppercase(), b.name), dest)
            })
            .collect();
        let mut routes = vec![];
        for (i, inp) in cfg.inputs.iter().enumerate() {
            for (b, bus) in cfg.buses.iter().enumerate() {
                let r = inp.route(&bus.id);
                routes.push((i, b, r.on && !inp.mute, r.level_db));
            }
        }
        let inputs = cfg.inputs.iter().map(|i| (i.name.clone(), hex_rgb(&i.color))).collect();
        let chains = cfg
            .inputs
            .iter()
            .map(|i| {
                let r = &i.rack;
                vec![
                    ("PRE".to_string(), r.pre_on),
                    ("GATE".to_string(), r.gate_on),
                    ("COMP".to_string(), r.comp_on),
                    ("EXC".to_string(), r.exc_on),
                    (i.fx.to_uppercase(), i.fx != "clean"),
                    ("EQ".to_string(), i.eq.iter().any(|g| g.abs() > 0.01)),
                    ("ECHO".to_string(), r.echo_on),
                    ("FADER".to_string(), !i.mute),
                ]
            })
            .collect();
        self.patch.set(Patch { apps, inputs, chains, buses, routes });
    }

    fn sync_apps(&self, g: &Graph) {
        let ours = |n: &str| ["route_", "strip_", "bus_", "meter_", "wavelink", "mix_", "voice_chat", "ee_", "easyeffects", "send_"].iter().any(|p| n.starts_with(p));
        let inputs: Vec<(String, String)> = self.cfg.borrow().inputs.iter().filter(|i| i.kind == InputKind::Virtual).map(|i| (names::ch(&i.id), i.name.clone())).collect();
        let streams: Vec<_> = g
            .nodes
            .iter()
            .filter(|n| n.class == "Stream/Output/Audio" && !ours(&n.name) && !n.app.is_empty() && n.app != "pipewire" && n.app != "pw-loopback")
            .map(|n| {
                let tgt = g.targets_of(n.id).iter().find_map(|t| inputs.iter().position(|i| i.0 == t.name));
                (n.clone(), tgt)
            })
            .collect();
        let sig = format!("{:?}{:?}", streams.iter().map(|(n, t)| (n.id, &n.media, *t)).collect::<Vec<_>>(), inputs);
        if *self.apps_sig.borrow() == sig {
            return;
        }
        *self.apps_sig.borrow_mut() = sig;
        while let Some(c) = self.apps.first_child() {
            self.apps.remove(&c);
        }
        if streams.is_empty() {
            self.apps.append(&label("Nothing is playing", "sub"));
        }
        let labels: Vec<&str> = inputs.iter().map(|i| i.1.as_str()).collect();
        for (n, tgt) in streams {
            let row = gtk::Box::new(gtk::Orientation::Vertical, 4);
            let t = label(&n.app, "name");
            t.set_xalign(0.0);
            let m = label(&n.media, "sub");
            m.set_xalign(0.0);
            m.set_ellipsize(gtk::pango::EllipsizeMode::End);
            m.set_max_width_chars(28);
            let dd = gtk::DropDown::from_strings(&labels);
            dd.set_selected(tgt.map(|t| t as u32).unwrap_or(gtk::INVALID_LIST_POSITION));
            let (serial, names_) = (n.serial, inputs.iter().map(|i| i.0.clone()).collect::<Vec<_>>());
            dd.connect_selected_notify(move |d| {
                if let Some(sink) = names_.get(d.selected() as usize) {
                    pw::move_stream(serial, sink);
                }
            });
            row.append(&t);
            row.append(&m);
            row.append(&dd);
            self.apps.append(&row);
        }
    }
}

/// Restart the audio engine (or the whole PipeWire stack when `full`), then rebuild the
/// window so meters reconnect to the fresh graph.
fn restart_audio(st: &Rc<App>, btn: &adw::SplitButton, full: bool) {
    if st.dirty.get() {
        st.save_cfg();
        st.dirty.set(false);
    }
    // Engine restart; if PipeWire itself is unresponsive, escalate to a full restart.
    let full_cmd = "systemctl --user restart pipewire.service pipewire-pulse.service wireplumber.service; \
                    systemctl --user restart wavelink-engine.service wavelink-personal.service";
    let script = if full {
        full_cmd.to_string()
    } else {
        format!("if timeout 3 pw-cli info 0 >/dev/null 2>&1; then systemctl --user restart wavelink-engine.service; else {full_cmd}; fi")
    };
    let child = std::process::Command::new("timeout")
        .args(["60", "sh", "-c", &script])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    let Ok(mut child) = child else { return };
    btn.set_sensitive(false);
    btn.set_label("Restarting…");
    let (st, btn) = (st.clone(), btn.clone());
    let mut settle = 0u32;
    glib::timeout_add_local(Duration::from_millis(250), move || {
        if settle == 0 {
            if matches!(child.try_wait(), Ok(None)) {
                return glib::ControlFlow::Continue;
            }
            settle = 1;
        }
        // give the engine a moment to recreate its nodes before reattaching meters
        settle += 1;
        if settle < 12 {
            return glib::ControlFlow::Continue;
        }
        // the graph worker thread refreshes st.graph; rebuild just reattaches meters
        st.rebuild();
        btn.set_label("Restart audio");
        btn.set_sensitive(true);
        glib::ControlFlow::Break
    });
}

pub fn activate(app: &adw::Application) {
    if let Some(w) = app.active_window() {
        w.present();
        return;
    }
    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
    let display = gdk::Display::default().expect("no display");
    let base = gtk::CssProvider::new();
    base.load_from_string(CSS);
    gtk::style_context_add_provider_for_display(&display, &base, gtk::STYLE_PROVIDER_PRIORITY_USER);
    let dynamic = gtk::CssProvider::new();
    gtk::style_context_add_provider_for_display(&display, &dynamic, gtk::STYLE_PROVIDER_PRIORITY_USER + 1);

    let win = adw::ApplicationWindow::builder().application(app).title("Mixer").default_width(1920).default_height(1000).build();
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    outer.add_css_class("main");
    let hb = adw::HeaderBar::new();
    let pages = gtk::Stack::builder().transition_type(gtk::StackTransitionType::SlideLeftRight).vexpand(true).build();
    let page_switch = gtk::StackSwitcher::builder().stack(&pages).build();
    hb.set_title_widget(Some(&page_switch));
    let rack_page = gtk::Box::new(gtk::Orientation::Vertical, 8);
    rack_page.set_margin_top(12);
    rack_page.set_margin_bottom(12);
    rack_page.set_margin_start(12);
    rack_page.set_margin_end(12);
    outer.append(&hb);
    let body = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    body.set_margin_top(12);
    body.set_margin_bottom(12);
    body.set_margin_start(12);
    body.set_margin_end(12);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.set_hexpand(true);
    body.append(&content);

    let apps = gtk::ListBox::new();
    apps.set_selection_mode(gtk::SelectionMode::None);
    let apps_panel = gtk::Box::new(gtk::Orientation::Vertical, 8);
    apps_panel.add_css_class("apps");
    apps_panel.set_size_request(240, -1);
    apps_panel.append(&{
        let l = label("APPS", "section");
        l.set_xalign(0.0);
        l
    });
    apps_panel.append(&label("Pick which input each app plays into. It's remembered per app.", "sub"));
    let asw = gtk::ScrolledWindow::builder().child(&apps).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build();
    apps_panel.append(&asw);
    // Apps panel starts hidden so every strip fits; the header "Apps" button opens it
    let rev = gtk::Revealer::builder().transition_type(gtk::RevealerTransitionType::SlideLeft).reveal_child(false).child(&apps_panel).build();
    body.append(&rev);
    pages.add_titled(&body, Some("console"), "Console");
    let rack_scroll = gtk::ScrolledWindow::builder().child(&rack_page).hexpand(true).vexpand(true).build();
    pages.add_titled(&rack_scroll, Some("rack"), "Rack");
    let player = crate::player::Player::new();
    pages.add_titled(&player.root, Some("player"), "Player");
    // console/rack on top, spectrum/patch bay below, with a draggable divider
    let split = gtk::Paned::builder()
        .orientation(gtk::Orientation::Vertical)
        .start_child(&pages)
        .resize_start_child(true)
        .shrink_start_child(true)
        .resize_end_child(false)
        .shrink_end_child(true)
        .wide_handle(true)
        .vexpand(true)
        .build();
    outer.append(&split);

    // RGB spectrum analyser across the bottom, tapping a chosen bus
    let spec_buses: Vec<(String, String)> = Config::load().buses.iter().map(|b| (names::bus(&b.id), b.name.clone())).collect();
    let spectrum = Spectrum::new(spec_buses.first().map(|b| b.0.as_str()).unwrap_or("bus_a1"));
    let spec_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
    spec_box.set_margin_start(12);
    spec_box.set_margin_end(12);
    spec_box.set_margin_bottom(12);
    let spec_head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let sl = label("SPECTRUM", "section");
    sl.set_xalign(0.0);
    sl.set_hexpand(true);
    spec_head.append(&sl);
    let names_: Vec<&str> = spec_buses.iter().map(|b| b.1.as_str()).collect();
    let dd = gtk::DropDown::from_strings(&names_);
    dd.set_tooltip_text(Some("Which output to analyse"));
    {
        let (sp, sb) = (spectrum.clone(), spec_buses.clone());
        dd.connect_selected_notify(move |d| {
            if let Some(b) = sb.get(d.selected() as usize) {
                sp.set_target(&b.0);
            }
        });
    }
    let patch = PatchBay::new();
    let stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).build();
    let spec_panel = gtk::Box::new(gtk::Orientation::Vertical, 0);
    spec_panel.add_css_class("spectrum");
    spec_panel.append(&spectrum.area);
    let patch_panel = gtk::Box::new(gtk::Orientation::Vertical, 0);
    patch_panel.add_css_class("spectrum");
    patch_panel.append(&patch.area);
    stack.add_titled(&spec_panel, Some("spectrum"), "Spectrum");
    stack.add_titled(&patch_panel, Some("patch"), "Patch Bay");
    let stems_v = stems_view::View::new();
    let stems_panel = gtk::Box::new(gtk::Orientation::Vertical, 0);
    stems_panel.add_css_class("spectrum");
    stems_panel.append(&stems_v.root);
    stack.add_titled(&stems_panel, Some("stems"), "Stems");
    // System monitor: read-only health of the audio system (lives under the output strips)
    let sys_panel = gtk::Box::new(gtk::Orientation::Vertical, 6);
    sys_panel.add_css_class("syspanel");
    sys_panel.append(&label_x("SYSTEM"));
    let sys_top = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let cpu_dial = crate::dash::Dial::new(124, "AUDIO CPU", 100.0, 70.0);
    cpu_dial.area.set_tooltip_text(Some(&format!("pipewire + pipewire-pulse + wireplumber + mixer engine + its pipewire, as % of ONE CPU core (100% = one full core; this PC has {} cores). The mixer window itself is under LEAK WATCH.", std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1))));
    let fd_dial = crate::dash::Dial::new(124, "PIPEWIRE FILES", 1024.0, 820.0);
    sys_top.append(&cpu_dial.area);
    sys_top.append(&fd_dial.area);
    let sys_info = gtk::Box::new(gtk::Orientation::Vertical, 4);
    sys_info.set_valign(gtk::Align::Center);
    let clock_lbl = label("", "sysbig");
    clock_lbl.set_xalign(0.0);
    let warn_lbl = label("", "sysbig");
    warn_lbl.set_xalign(0.0);
    let lights_row = gtk::FlowBox::builder().selection_mode(gtk::SelectionMode::None).max_children_per_line(8).column_spacing(4).row_spacing(4).build();
    sys_info.append(&label_x("BUFFER · RATE · LATENCY"));
    sys_info.append(&clock_lbl);
    sys_info.append(&label_x("DROPOUT WARNINGS"));
    sys_info.append(&warn_lbl);
    let leak_lbl = label("", "sysbig");
    leak_lbl.set_xalign(0.0);
    sys_info.append(&label_x("LEAK WATCH"));
    sys_info.append(&leak_lbl);
    sys_top.append(&sys_info);
    sys_panel.append(&sys_top);
    sys_panel.append(&lights_row);
    {
        let (stx, srx) = std::sync::mpsc::channel::<crate::sysmon::Stats>();
        crate::sysmon::spawn(stx);
        let (cd, fdd, cl, wl, lr, lk) = (cpu_dial.clone(), fd_dial.clone(), clock_lbl.clone(), warn_lbl.clone(), lights_row.clone(), leak_lbl.clone());
        // needles animate on the frame clock (never starves redraws); data arrives every 2 s
        {
            let (cd2, fd2) = (cpu_dial.clone(), fd_dial.clone());
            let last = Cell::new(0i64);
            let cap = crate::dash::FrameCap::new();
            cpu_dial.area.add_tick_callback(move |_, clock| {
                let now = clock.frame_time();
                let prev = last.replace(now);
                let raw = if prev == 0 { 0.016 } else { ((now - prev) as f64 / 1e6).clamp(0.001, 0.1) };
                let Some(dt) = cap.tick(raw) else { return glib::ControlFlow::Continue };
                let dt = dt.clamp(0.001, 0.1);
                cd2.step(dt);
                fd2.step(dt);
                glib::ControlFlow::Continue
            });
        }
        // the services row is rebuilt only when a service changes state (it was rebuilt every tick)
        let last_svc: RefCell<Vec<(&'static str, bool)>> = RefCell::new(vec![]);
        glib::timeout_add_local(Duration::from_millis(250), move || {
            while let Ok(st) = srx.try_recv() {
                cd.set(st.cpu, &format!("{:.0}% of 1 core", st.cpu));
                fdd.set(st.fds as f64 * 1024.0 / st.fd_limit.max(1) as f64, &format!("{} / {}", st.fds, st.fd_limit));
                cl.set_label(&crate::sysmon::cycle_label(st.quantum, st.rate));
                wl.set_label(&format!("{}", st.warnings));
                if st.warnings > 0 { wl.add_css_class("warn") } else { wl.remove_css_class("warn") }
                lk.set_label(&format!("{}/{} taps · {} zombies\n{} MB · {:.0}% of 1 core", st.gui.taps, st.expected_taps, st.gui.zombies + st.engine.zombies, st.gui.rss_kb / 1024, st.gui_cpu));
                if st.leaks.is_empty() { lk.remove_css_class("warn") } else { lk.add_css_class("warn") }
                lk.set_tooltip_text(Some(&format!(
                    "Window: {:.0}% of 1 core · {} MB · {} files · {} threads · {} zombies · {}/{} taps\nEngine: {} MB · {} files · {} threads · {} zombies\n{}",
                    st.gui_cpu, st.gui.rss_kb / 1024, st.gui.fds, st.gui.threads, st.gui.zombies, st.gui.taps, st.expected_taps,
                    st.engine.rss_kb / 1024, st.engine.fds, st.engine.threads, st.engine.zombies,
                    if st.leaks.is_empty() { "No leaks seen".to_string() } else { st.leaks.join("\n") })));
                if *last_svc.borrow() != st.services {
                    *last_svc.borrow_mut() = st.services.clone();
                    while let Some(c) = lr.first_child() {
                        lr.remove(&c);
                    }
                    for (name, ok) in &st.services {
                        let l = label(&format!("● {name}"), "svc");
                        l.add_css_class(if *ok { "ok" } else { "bad" });
                        lr.append(&l);
                    }
                }
            }
            glib::ControlFlow::Continue
        });
    }
    let switcher = gtk::StackSwitcher::builder().stack(&stack).build();
    spec_head.prepend(&switcher);
    sl.set_visible(false);
    {
        let dd2 = dd.clone();
        stack.connect_visible_child_name_notify(move |s| dd2.set_visible(s.visible_child_name().as_deref() == Some("spectrum")));
    }
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    spec_head.append(&spacer);
    spec_head.append(&dd);
    spec_box.append(&spec_head);
    spec_box.append(&stack);
    if let Ok(page) = std::env::var("MIXER_PAGE") {
        stack.set_visible_child_name(&page);
    }
    split.set_end_child(Some(&spec_box));
    // remember where the divider was left (per-viewer convenience)
    let pos_file = config::glib_home().join(".local/state/wavelink-mixer/divider");
    let saved = std::fs::read_to_string(&pos_file).ok().and_then(|t| t.trim().parse::<i32>().ok());
    split.set_position(saved.unwrap_or(780));
    split.connect_position_notify(move |p| {
        let _ = std::fs::write(&pos_file, p.position().to_string());
    });
    win.set_content(Some(&outer));

    let st = Rc::new(App {
        cfg: RefCell::new(Config::load()),
        synced: RefCell::new(Config::default_config()), // set to the loaded config right below (one load, B36-9)
        norm_btns: RefCell::new(HashMap::new()),
        auto_gain: RefCell::new(Default::default()),
        graph: RefCell::new(Graph::read().unwrap_or_default()),
        dirty: Cell::new(false),
        content,
        _player: player.clone(),
        rack_page: rack_page.clone(),
        sys_panel: sys_panel.clone(),
        faders: RefCell::new(vec![]),
        bus_guard: RefCell::new(HashMap::new()),
        routes: RefCell::new(vec![]),
        meters: RefCell::new(vec![]),
        rack_meters: RefCell::new(vec![]),
        rack_ticks: RefCell::new(vec![]),
        console_ticks: RefCell::new(vec![]),
        rack_visible: Cell::new(false),
        taps_on: Cell::new(true),
        levels: RefCell::new(HashMap::new()),
        spectra: RefCell::new(HashMap::new()),
        patch: patch.clone(),
        knobs: RefCell::new(vec![]),
        keep: RefCell::new(vec![]),
        solo: RefCell::new(Default::default()),
        solo_btns: RefCell::new(HashMap::new()),
        send_knobs: RefCell::new(HashMap::new()),
        pickup: RefCell::new(HashMap::new()),
        meter_links: RefCell::new(vec![]),
        norm: RefCell::new(HashMap::new()),
        norm_moved: Cell::new(false),
        norm_save: Cell::new(0.0),
        sides: RefCell::new(HashMap::new()),
        side_btns: RefCell::new(vec![]),
        side_applied: RefCell::new(HashMap::new()),
        side_clear: RefCell::new(None),
        side_q: Serial::new(),
        apps,
        apps_sig: RefCell::new(String::new()),
        css: dynamic,
        stems: stems_view::State::new(),
    });
    *st.synced.borrow_mut() = st.cfg.borrow().clone();
    stems_view::wire(&st, stems_v.clone());

    let add_in = gtk::MenuButton::builder().label("+ Input").popover(&st.add_input_popover()).build();
    let add_bus = gtk::MenuButton::builder().label("+ Output").popover(&st.add_bus_popover()).build();
    hb.pack_start(&add_in);
    hb.pack_start(&add_bus);
    let apps_btn = gtk::ToggleButton::builder().label("Apps").active(false).build();
    let r2 = rev.clone();
    apps_btn.connect_toggled(move |b| r2.set_reveal_child(b.is_active()));
    hb.pack_end(&apps_btn);

    // Restart audio: click = restart the mixer engine; dropdown = full PipeWire restart.
    let restart = adw::SplitButton::builder()
        .label("Restart audio")
        .tooltip_text("Restart the mixer engine (rebuilds strips, routes and outputs)")
        .build();
    let full = gtk::Button::with_label("Full restart (PipeWire + engine)");
    full.add_css_class("flat");
    let pop = gtk::Popover::new();
    pop.set_child(Some(&full));
    restart.set_popover(Some(&pop));
    {
        let (st, b) = (st.clone(), restart.clone());
        restart.connect_clicked(move |_| restart_audio(&st, &b, false));
    }
    {
        let (st, b, p) = (st.clone(), restart.clone(), pop.clone());
        full.connect_clicked(move |_| {
            p.popdown();
            restart_audio(&st, &b, true);
        });
    }
    hb.pack_end(&restart);
    let midi_lbl = gtk::Label::new(Some("● SL MkIII"));
    midi_lbl.add_css_class("midi");
    midi_lbl.set_tooltip_text(Some(
        "Novation SL MkIII control surface\n\
         Faders 1-8: System · Spotify · Discord · Browser · Game · Mic · Headphones · Monitor\n\
         Buttons under faders: mute those 8\n\
         Second button row 1-6: solo the inputs\n\
         Knobs 1-6: each input's Headphones (A1) send · Knob 7: Stream · Knob 8: Voice Chat\n\
         Pickup: a control takes over once it passes the on-screen value (no jumps)",
    ));
    hb.pack_end(&midi_lbl);
    {
        let w = Rc::downgrade(&st);
        st.patch.on_click(move |hit, x, y| {
            if let Some(st) = w.upgrade() {
                st.patch_clicked(hit, x, y);
            }
        });
    }
    st.rebuild();
    {
        let (mtx, mrx) = std::sync::mpsc::channel::<crate::midi::Midi>();
        crate::midi::spawn(mtx);
        let (w, ml) = (Rc::downgrade(&st), midi_lbl.clone());
        glib::timeout_add_local(Duration::from_millis(20), move || {
            let Some(st) = w.upgrade() else { return glib::ControlFlow::Break };
            while let Ok(m) = mrx.try_recv() {
                match m {
                    crate::midi::Midi::Connected(on) => {
                        if on { ml.add_css_class("on") } else { ml.remove_css_class("on") }
                    }
                    crate::midi::Midi::Cc(c, v) => st.midi_cc(c, v),
                }
            }
            glib::ControlFlow::Continue
        });
    }
    {
        // re-read everything when switching pages so both views show the same values
        let st = st.clone();
        pages.connect_visible_child_name_notify(move |p| {
            st.rack_visible.set(p.visible_child_name().as_deref() == Some("rack"));
            st.taps_on.set(p.visible_child_name().as_deref() != Some("player"));
            let st = st.clone();
            glib::idle_add_local_once(move || st.rebuild());
        });
    }
    if let Ok(v) = std::env::var("MIXER_VIEW") {
        st.rack_visible.set(v == "rack");
        st.taps_on.set(v != "player");
        pages.set_visible_child_name(&v);
        st.rebuild();
    }

    // graph polling in a worker thread
    let (tx, rx) = mpsc::channel::<Graph>();
    std::thread::spawn(move || loop {
        if let Some(g) = Graph::read() {
            if tx.send(g).is_err() {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    });
    // animate needles and spectrum on the frame clock (never starves redraws, pauses when hidden)
    let (st3, sp3) = (st.clone(), spectrum.clone());
    let last_frame = Cell::new(0i64);
    // meters and the spectrum animate at 30 Hz, not at the refresh rate
    let cap = crate::dash::FrameCap::new();
    win.add_tick_callback(move |_, clock| {
        let now = clock.frame_time();
        let prev = last_frame.replace(now);
        let raw = if prev == 0 { 0.016 } else { ((now - prev) as f64 / 1e6).clamp(0.001, 0.1) };
        let Some(dt) = cap.tick(raw) else { return glib::ControlFlow::Continue };
        let dt = dt.clamp(0.001, 0.05);
        for m in st3.meters.borrow().iter() {
            m.tick(dt);
        }
        for m in st3.rack_meters.borrow().iter() {
            m.tick(dt);
        }
        for t in st3.rack_ticks.borrow().iter() {
            t(dt);
        }
        for t in st3.console_ticks.borrow().iter() {
            t(dt);
        }
        // the bottom spectrum reads the bus tap's bands (no pw-record of its own)
        let src = st3.spectra.borrow().get(&format!("bus:{}", sp3.target())).cloned();
        sp3.step(dt, src.as_ref());
        stems_view::step(&st3, dt);
        glib::ControlFlow::Continue
    });
    let (st2, ww) = (st.clone(), win.downgrade());
    let mut save_tick = 0u32;
    glib::timeout_add_local(Duration::from_millis(100), move || {
        if ww.upgrade().is_none() {
            stems_view::shutdown(&st2);
            // never leave channels muted by a solo when the window closes
            if !st2.solo.borrow().is_empty() {
                st2.solo.borrow_mut().clear();
                st2.apply_solo();
            }
            // L/R solo: put both ears back before exit (synchronous, a few pw-link runs), so a clean close never leaves the headphones mono
            for c in st2.sides.borrow().values() {
                c.set(Side::Stereo);
            }
            let mut restore = vec![];
            for (o, i, s) in st2.desired_sides() {
                if st2.side_applied.borrow().get(&(o.clone(), i.clone())).map(|r| r.0).unwrap_or(Side::Stereo) != Side::Stereo {
                    restore.push((o, i, s));
                }
            }
            st2.side_q.push(Box::new(move || {
                for (o, i, s) in restore {
                    pw::link_side(&o, &i, s);
                }
            }));
            st2.side_q.flush(Duration::from_secs(5));
            st2.meters.borrow_mut().clear();
            crate::leak::EXPECTED_TAPS.store(0, std::sync::atomic::Ordering::Relaxed);
            if st2.norm_moved.get() {
                st2.dirty.set(true);
            }
            if st2.dirty.get() {
                st2.save_cfg();
            }
            return glib::ControlFlow::Break;
        }
        // stems housekeeping runs here, not in the frame tick: the tick stops while the display sleeps
        let mut remute = false;
        if let Some(g) = rx.try_iter().last() {
            // fresh config first: #35's bus tracker must never re-apply a gain the remote just replaced
            st2.pull_cfg();
            st2.sync(&g);
            *st2.graph.borrow_mut() = g;
            remute = true;
        }
        save_tick += 1;
        stems_view::housekeep(&st2, remute, save_tick % 10 == 0);
        if save_tick % 5 == 0 && st2.dirty.get() {
            st2.save_cfg();
            st2.dirty.set(false);
        } else if save_tick % 10 == 0 {
            // edits the engine's remote server (or another writer) saved: merge them in (#26)
            st2.pull_cfg();
        }
        glib::ControlFlow::Continue
    });

    if let Ok(path) = std::env::var("MIXER_SCREENSHOT") {
        let w = win.clone();
        glib::timeout_add_local_once(Duration::from_millis(std::env::var("MIXER_SCREENSHOT_DELAY").ok().and_then(|v| v.parse().ok()).unwrap_or(3000)), move || {
            let p = gtk::WidgetPaintable::new(Some(&w));
            let (wd, ht) = (w.width(), w.height());
            let snap = gtk::Snapshot::new();
            p.snapshot(&snap, wd as f64, ht as f64);
            let node = snap.to_node();
            if node.is_none() {
                eprintln!("screenshot: window had nothing rendered ({wd}x{ht})");
            }
            if let (Some(node), Some(r)) = (node, w.renderer()) {
                let tex = r.render_texture(&node, Some(&gtk::graphene::Rect::new(0.0, 0.0, wd as f32, ht as f32)));
                let _ = tex.save_to_png(&path);
            }
            if let Some(app) = w.application() {
                app.quit();
            }
        });
    }
    if std::env::var_os("MIXER_SCREENSHOT").is_none() {
        win.maximize();
    }
    win.present();
}

/// Whether a (out, in) pair's links must be (re)applied: first sight of the pair (Stereo included), or a side or node-id change.
fn needs_relink(cur: Option<(Side, u64, u64)>, want: (Side, u64, u64)) -> bool {
    cur != Some(want)
}

/// FIFO job queue on one long-lived named thread, off the GTK thread: jobs run strictly in push order.
pub struct Serial {
    tx: mpsc::Sender<Box<dyn FnOnce() + Send>>,
}

impl Serial {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel::<Box<dyn FnOnce() + Send>>();
        let _ = std::thread::Builder::new().name("side-links".into()).spawn(move || {
            for job in rx {
                job();
            }
        });
        Serial { tx }
    }
    pub fn push(&self, job: Box<dyn FnOnce() + Send>) {
        let _ = self.tx.send(job);
    }
    /// Wait until every job pushed before this call has run; false if `timeout` passes first.
    pub fn flush(&self, timeout: Duration) -> bool {
        let (done_tx, done_rx) = mpsc::channel::<()>();
        self.push(Box::new(move || {
            let _ = done_tx.send(());
        }));
        done_rx.recv_timeout(timeout).is_ok()
    }
}

#[cfg(test)]
mod serial_tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    #[test]
    fn needs_relink_first_sight_and_id_change() {
        assert!(needs_relink(None, (Side::Stereo, 1, 2)));
        assert!(!needs_relink(Some((Side::Stereo, 1, 2)), (Side::Stereo, 1, 2)));
        assert!(needs_relink(Some((Side::Stereo, 1, 2)), (Side::Stereo, 1, 3)));
    }

    #[test]
    fn serial_runs_jobs_in_push_order() {
        let q = Serial::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        for k in 0..300usize {
            let seen = seen.clone();
            q.push(Box::new(move || {
                std::thread::sleep(Duration::from_millis((k % 3) as u64));
                seen.lock().unwrap().push(k);
            }));
        }
        assert!(q.flush(Duration::from_secs(5)));
        assert_eq!(*seen.lock().unwrap(), (0..300).collect::<Vec<usize>>());
    }

    #[test]
    fn serial_flush_times_out() {
        let q = Serial::new();
        q.push(Box::new(|| std::thread::sleep(Duration::from_secs(2))));
        let t = Instant::now();
        assert!(!q.flush(Duration::from_millis(100)));
        assert!(t.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn apply_sides_uses_queue_and_no_startup_shortcut() {
        let s = include_str!("ui.rs");
        assert!(s.contains(concat!("self.side_q", ".push(")));
        assert!(!s.contains(concat!("cur.is_none() && s ", "== Side::Stereo")));
    }
}
