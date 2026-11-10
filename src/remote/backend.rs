//! The "apply to the live graph" layer behind a trait, so the server runs in tests without PipeWire.
use crate::config::{db_to_vol, names, Config};
use crate::pw::{self, Graph};
use std::collections::HashMap;
use std::io::Read as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use wavelink_proto as p;

pub trait Backend: Send + Sync + 'static {
    /// Push one applied command into the graph; `cfg` is the config after the change.
    fn apply(&self, cfg: &Config, cmd: &p::Command);
    /// Called ~15 times a second while at least one client wants meters (`want` = true), and once with
    /// false when the last one leaves. Returns the levels since the previous call.
    fn meters(&self, cfg: &Config, want: bool) -> Option<p::Meters>;
    /// Whether the node's props say it is an AirPlay/RAOP sink (#35), whatever its name.
    fn node_is_raop(&self, _node: &str) -> bool {
        false
    }
}

/// The real thing: wpctl/pw-cli on the same nodes the GUI drives, meters from pw-record taps.
pub struct PwBackend {
    graph: Mutex<(Instant, Option<Graph>)>,
    taps: Mutex<Taps>,
}

impl Default for PwBackend {
    fn default() -> Self {
        PwBackend { graph: Mutex::new((Instant::now() - Duration::from_secs(60), None)), taps: Mutex::new(Taps::default()) }
    }
}

impl PwBackend {
    /// Node id by name from a graph at most 2 s old (re-read once when the name is missing).
    fn node_id(&self, name: &str) -> Option<u64> {
        let mut g = self.graph.lock().unwrap_or_else(|e| e.into_inner());
        for attempt in 0..2 {
            if attempt == 1 || g.0.elapsed() > Duration::from_secs(2) || g.1.is_none() {
                *g = (Instant::now(), Graph::read());
            }
            if let Some(id) = g.1.as_ref().and_then(|gr| gr.node(name)).map(|n| n.id) {
                return Some(id);
            }
        }
        None
    }

    /// Node id and whether its props say AirPlay (for the #35 ceiling).
    fn node(&self, name: &str) -> Option<(u64, bool)> {
        let id = self.node_id(name)?;
        let g = self.graph.lock().unwrap_or_else(|e| e.into_inner());
        Some((id, g.1.as_ref().and_then(|gr| gr.node(name)).map_or(false, |n| n.raop)))
    }

    fn graph(&self) -> Option<Graph> {
        let mut g = self.graph.lock().unwrap_or_else(|e| e.into_inner());
        if g.0.elapsed() > Duration::from_secs(2) || g.1.is_none() {
            *g = (Instant::now(), Graph::read());
        }
        g.1.clone()
    }
}

impl Backend for PwBackend {
    fn node_is_raop(&self, node: &str) -> bool {
        self.node(node).is_some_and(|(_, raop)| raop)
    }

    fn apply(&self, cfg: &Config, cmd: &p::Command) {
        match cmd {
            // scaffold (U2 todo): host commands bypass the graph backend
            p::Command::SetHostVolume { .. } | p::Command::SetHostMute { .. } => {}
            p::Command::SetInputGain { input, db } => {
                if let Some(id) = self.node_id(&names::strip_in(input)) {
                    pw::set_volume(id, db_to_vol(*db));
                }
            }
            p::Command::SetInputMute { input, mute } => {
                if let Some(id) = self.node_id(&names::strip_in(input)) {
                    pw::set_mute(id, *mute);
                }
            }
            p::Command::SetRoute { input, bus, .. } => {
                let Some(r) = cfg.inputs.iter().find(|i| &i.id == input).map(|i| i.route(bus)) else { return };
                if let Some(id) = self.node_id(&names::route(input, bus)) {
                    pw::set_volume(id, db_to_vol(r.level_db));
                    pw::set_mute(id, !r.on);
                }
            }
            // auto-normalize runs in the GUI (it owns the IN meter taps); the flag/target travel through config.json
            p::Command::SetNormalize { .. } => {}
            // buses: never above the ceiling, including one only the node's props reveal (#35);
            // volume before any unmute, in one ordered child
            p::Command::SetBusGain { bus, db, .. } => {
                if let Some(b) = cfg.buses.iter().find(|b| &b.id == bus) {
                    if let Some((id, raop)) = self.node(&crate::engine::bus_fader_node(b)) {
                        pw::set_volume(id, db_to_vol(db.min(b.ceiling_for(raop))));
                    }
                }
            }
            p::Command::SetBusMute { bus, mute } => {
                if let Some(b) = cfg.buses.iter().find(|b| &b.id == bus) {
                    if let Some((id, raop)) = self.node(&crate::engine::bus_fader_node(b)) {
                        match &b.kind {
                            crate::config::BusKind::Hardware { .. } => pw::set_volume_mute(id, db_to_vol(b.gain_db.min(b.ceiling_for(raop))), *mute),
                            _ => pw::set_mute(id, *mute),
                        }
                    }
                }
            }
        }
    }

    fn meters(&self, cfg: &Config, want: bool) -> Option<p::Meters> {
        let mut t = self.taps.lock().unwrap_or_else(|e| e.into_inner());
        if !want {
            t.stop();
            return None;
        }
        let keys: Vec<(String, bool)> =
            cfg.inputs.iter().map(|i| (i.id.clone(), false)).chain(cfg.buses.iter().map(|b| (b.id.clone(), true))).collect();
        if t.keys != keys {
            t.stop();
            for (id, is_bus) in &keys {
                if let Some(tap) = Tap::spawn(id, *is_bus) {
                    t.taps.insert((id.clone(), *is_bus), tap);
                }
            }
            t.keys = keys;
            t.linked = None;
        }
        // input taps are unconnected streams: link strip_<id> -> tap (like the GUI's OUT meter), re-checked every 2 s
        if t.linked.map(|l| l.elapsed() > Duration::from_secs(2)).unwrap_or(true) {
            t.linked = Some(Instant::now());
            if let Some(g) = self.graph() {
                for ((id, is_bus), tap) in t.taps.iter() {
                    if !*is_bus {
                        let s = names::strip(id);
                        if g.has(&s) && g.has(&tap.node) && !g.linked(&s, &tap.node) {
                            let (o, i) = (s.clone(), tap.node.clone());
                            std::thread::spawn(move || pw::link(&o, &i));
                        }
                    }
                }
            }
        }
        let mut m = p::Meters::default();
        for ((id, is_bus), tap) in t.taps.iter() {
            let (pk, rms) = tap.take();
            let l = p::Level { id: id.clone(), peak_db: pk as f32, rms_db: rms as f32 };
            if *is_bus {
                m.buses.push(l);
            } else {
                m.inputs.push(l);
            }
        }
        Some(m)
    }
}

#[derive(Default)]
struct Taps {
    keys: Vec<(String, bool)>,
    taps: HashMap<(String, bool), Tap>,
    linked: Option<Instant>,
}

impl Taps {
    fn stop(&mut self) {
        self.taps.clear(); // Drop kills each pw-record
        self.keys.clear();
    }
}

/// A pw-record child (raw f32 stereo on stdout) and the peak/power accumulated since the last take.
/// Named `wlremote_*` so the GUI's leak watch (which counts `meter_*` taps) never mistakes it for its own.
struct Tap {
    child: std::process::Child,
    node: String,
    peak: Arc<AtomicU64>,
    acc: Arc<Mutex<(f64, u64)>>,
}

impl Drop for Tap {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Tap {
    fn spawn(id: &str, is_bus: bool) -> Option<Tap> {
        let node = format!("wlremote_{}_{id}.{}", if is_bus { "bus" } else { "in" }, std::process::id());
        let mut props = format!("node.name={node} node.passive=true media.class=Stream/Input/Audio");
        let mut args: Vec<String> = vec![];
        if is_bus {
            props.push_str(" stream.capture.sink=true");
            args.extend(["--target".into(), names::bus(id)]);
        } else {
            props.push_str(" node.autoconnect=false");
        }
        args.extend(
            ["-P", &format!("{{ {props} }}"), "--format", "f32", "--rate", "48000", "--channels", "2", "--latency", "40ms", "--raw", "-"].map(String::from),
        );
        let mut cmd = std::process::Command::new("pw-record");
        // spawned from the server's long-lived meter thread: PDEATHSIG fires when that thread dies
        pw::die_with_parent(&mut cmd);
        let mut child = cmd.args(&args).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).spawn().ok()?;
        let mut out = child.stdout.take()?;
        let peak = Arc::new(AtomicU64::new(0f64.to_bits()));
        let acc = Arc::new(Mutex::new((0.0f64, 0u64)));
        let (pk, ac) = (peak.clone(), acc.clone());
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 8 * 1920];
            let mut carry: Vec<u8> = vec![];
            while let Ok(n) = out.read(&mut buf) {
                if n == 0 {
                    break;
                }
                carry.extend_from_slice(&buf[..n]);
                let samples = carry.len() / 4;
                let (mut mx, mut sq) = (0.0f64, 0.0f64);
                for s in 0..samples {
                    let x = f32::from_le_bytes([carry[s * 4], carry[s * 4 + 1], carry[s * 4 + 2], carry[s * 4 + 3]]) as f64;
                    mx = mx.max(x.abs());
                    sq += x * x;
                }
                carry.drain(..samples * 4);
                if mx > f64::from_bits(pk.load(Ordering::Relaxed)) {
                    pk.store(mx.to_bits(), Ordering::Relaxed);
                }
                let mut a = ac.lock().unwrap_or_else(|e| e.into_inner());
                a.0 += sq;
                a.1 += samples as u64;
            }
        });
        Some(Tap { child, node, peak, acc })
    }

    /// (peak dBFS, rms dBFS) since the last call; -90 for silence.
    fn take(&self) -> (f64, f64) {
        let db = |x: f64| if x > 0.0 { (20.0 * x.log10()).max(-90.0) } else { -90.0 };
        let pk = f64::from_bits(self.peak.swap(0f64.to_bits(), Ordering::Relaxed));
        let mut a = self.acc.lock().unwrap_or_else(|e| e.into_inner());
        let rms = if a.1 > 0 { (a.0 / a.1 as f64).sqrt() } else { 0.0 };
        *a = (0.0, 0);
        (db(pk), db(rms))
    }
}

/// Fake graph: records every applied command, synthesises meters (tests and `fake-engine`, #37).
#[derive(Default)]
pub struct FakeBackend {
    /// node names the fake graph reports as AirPlay sinks by their props
    pub raop_nodes: Mutex<Vec<String>>,
    pub applied: Mutex<Vec<p::Command>>,
    pub meter_calls: AtomicU64,
    pub stopped: AtomicU64,
}

impl Backend for FakeBackend {
    fn node_is_raop(&self, node: &str) -> bool {
        self.raop_nodes.lock().unwrap().iter().any(|n| n == node)
    }
    fn apply(&self, _cfg: &Config, cmd: &p::Command) {
        self.applied.lock().unwrap().push(cmd.clone());
    }
    fn meters(&self, cfg: &Config, want: bool) -> Option<p::Meters> {
        if !want {
            self.stopped.fetch_add(1, Ordering::SeqCst);
            return None;
        }
        let n = self.meter_calls.fetch_add(1, Ordering::SeqCst);
        let lv = |id: &str| p::Level { id: id.into(), peak_db: -6.0 - (n % 10) as f32, rms_db: -20.0 };
        Some(p::Meters { inputs: cfg.inputs.iter().map(|i| lv(&i.id)).collect(), buses: cfg.buses.iter().map(|b| lv(&b.id)).collect() })
    }
}
