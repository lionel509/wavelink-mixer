//! Mixer configuration (~/.config/wavelink-mixer/config.json), shared by the UI and the engine.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub const SCARLETT: &str = "alsa_output.usb-Focusrite_Scarlett_Solo_USB_P70NH0J0C8A78C-00.HiFi__Line__sink";
pub const HDMI: &str = "alsa_output.pci-0000_01_00.1.hdmi-stereo";
pub const MIN_DB: f64 = -60.0;
pub const MAX_DB: f64 = 12.0;
pub const FX_PRESETS: &[&str] = &["clean", "music", "game", "voice", "voicefix"];
/// Graphic EQ bands: (centre Hz, label). First/last are shelves, the rest peaking.
pub const EQ_BANDS: &[(f64, &str)] = &[
    (25.0, "25"), (45.0, "45"), (80.0, "80"), (145.0, "145"), (260.0, "260"), (470.0, "470"),
    (850.0, "850"), (1500.0, "1.5k"), (2700.0, "2.7k"), (5000.0, "5k"), (9000.0, "9k"), (16000.0, "16k"),
];
pub const EQ_DB: f64 = 12.0;
/// Graphic EQ presets, dB per band (25 45 80 145 260 470 850 1.5k 2.7k 5k 9k 16k).
pub const EQ_PRESETS: &[(&str, [f64; 12])] = &[
    ("Flat",            [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
    ("Bass Boost",      [6.0, 6.0, 5.0, 3.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
    ("V-Shape",         [5.0, 5.0, 4.0, 2.0, 0.0, -2.0, -3.0, -2.0, 0.0, 2.0, 4.0, 4.0]),
    ("Hip-Hop",         [5.0, 6.0, 5.0, 2.0, 0.0, -1.0, -1.0, 0.0, 1.0, 1.0, 1.0, 1.0]),
    ("Electronic",      [5.0, 5.0, 3.0, 0.0, -1.0, 0.0, 0.0, 0.0, 2.0, 3.0, 4.0, 4.0]),
    ("Rock",            [4.0, 4.0, 3.0, 1.0, -1.0, -2.0, -1.0, 1.0, 3.0, 3.0, 3.0, 2.0]),
    ("Acoustic",        [1.0, 1.0, 2.0, 2.0, 1.0, 0.0, 0.0, 1.0, 2.0, 2.0, 2.0, 1.0]),
    ("Warm",            [1.0, 2.0, 3.0, 3.0, 2.0, 0.0, 0.0, -1.0, -1.0, -2.0, -3.0, -3.0]),
    ("Bright / Air",    [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 2.0, 3.0, 4.0, 5.0]),
    ("Vocal Clarity",   [-4.0, -4.0, -2.0, -1.0, -2.0, -1.0, 1.0, 3.0, 4.0, 3.0, 1.0, 0.0]),
    ("Podcast / Voice", [-12.0, -10.0, -4.0, 0.0, -2.0, -1.0, 1.0, 3.0, 3.0, 2.0, 0.0, -2.0]),
    ("FPS Footsteps",   [-6.0, -6.0, -4.0, -2.0, -2.0, -1.0, 0.0, 2.0, 5.0, 4.0, 2.0, 0.0]),
    ("Movie",           [4.0, 4.0, 3.0, 1.0, 0.0, 0.0, 1.0, 2.0, 1.0, 1.0, 2.0, 2.0]),
    ("Late Night",      [-6.0, -5.0, -3.0, -1.0, 0.0, 0.0, 1.0, 2.0, 2.0, 1.0, 0.0, 0.0]),
    // other people's voices: no rumble/boom, less mud, tame the harsh 2.7-5k range and sibilance
    ("Smooth Voices",   [-8.0, -6.0, -2.0, 0.0, -2.0, 0.0, 0.0, 0.0, -2.0, -5.0, -6.0, -6.0]),
    ("Telephone",       [-12.0, -12.0, -12.0, -8.0, -2.0, 2.0, 4.0, 4.0, 0.0, -6.0, -12.0, -12.0]),
];

/// Accepts the old 3-band [lo, mid, hi] and spreads it over the graphic EQ bands.
fn de_eq<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<f64>, D::Error> {
    let v: Vec<f64> = Vec::deserialize(d)?;
    Ok(normalize_eq(v))
}
pub fn normalize_eq(v: Vec<f64>) -> Vec<f64> {
    let n = EQ_BANDS.len();
    if v.len() == 3 && n == 12 {
        let (lo, mid, hi) = (v[0], v[1], v[2]);
        return vec![lo, lo, lo, lo, 0.0, mid / 2.0, mid, mid / 2.0, 0.0, hi / 2.0, hi, hi];
    }
    let mut v = v;
    v.resize(n, 0.0);
    v
}
pub fn flat_eq() -> Vec<f64> {
    vec![0.0; EQ_BANDS.len()]
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Route {
    pub on: bool,
    pub level_db: f64,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum InputKind {
    /// A virtual playback device (apps play into `ch_<id>`).
    Virtual,
    /// An existing capture device or virtual source (mic, EasyEffects output...).
    Source { target: String },
}

/// Outboard gear on an input strip. Units are always in the chain; "off" means
/// neutral parameters, so switching never rebuilds the graph.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Rack {
    pub pre_on: bool,
    pub trim_db: f64,
    pub drive_db: f64,
    pub out_db: f64,
    pub gate_on: bool,
    pub gate_db: f64,
    pub comp_on: bool,
    pub comp_thr_db: f64,
    pub comp_ratio: f64,   // caps "strength" 0..1
    pub comp_attack: f64,  // 0..1
    pub comp_release: f64, // 0..1
    pub comp_makeup_db: f64,
    pub exc_on: bool,
    pub exc_lo: f64, // 0..1
    pub exc_hi: f64, // 0..1
    pub echo_on: bool,
    pub echo_time: f64, // seconds between repeats
    pub echo_fb: f64,   // 0..0.9: how loud each repeat is vs the previous
    pub echo_mix: f64,  // 0..1
}

impl Default for Rack {
    fn default() -> Self {
        Rack {
            pre_on: false, trim_db: 0.0, drive_db: 6.0, out_db: 0.0,
            gate_on: false, gate_db: -45.0,
            comp_on: false, comp_thr_db: -18.0, comp_ratio: 0.5, comp_attack: 0.3, comp_release: 0.5, comp_makeup_db: 3.0,
            exc_on: false, exc_lo: 0.25, exc_hi: 0.25,
            echo_on: false, echo_time: 0.35, echo_fb: 0.5, echo_mix: 0.35,
        }
    }
}

/// Whole-rack presets (all four units at once).
pub fn rack_presets() -> Vec<(&'static str, Rack)> {
    let d = Rack::default();
    vec![
        ("Bypass All", d.clone()),
        ("Broadcast Voice", Rack { pre_on: true, drive_db: 3.0, gate_on: true, gate_db: -50.0, comp_on: true, comp_thr_db: -24.0, comp_ratio: 0.6, comp_attack: 0.2, comp_release: 0.4, comp_makeup_db: 6.0, exc_on: true, exc_lo: 0.2, exc_hi: 0.35, ..d.clone() }),
        ("Podcast", Rack { gate_on: true, gate_db: -45.0, comp_on: true, comp_thr_db: -20.0, comp_ratio: 0.5, comp_makeup_db: 5.0, exc_on: true, exc_lo: 0.0, exc_hi: 0.25, ..d.clone() }),
        ("Warm Tape", Rack { pre_on: true, drive_db: 9.0, out_db: -3.0, comp_on: true, comp_thr_db: -12.0, comp_ratio: 0.3, comp_makeup_db: 2.0, ..d.clone() }),
        ("Punchy Music", Rack { comp_on: true, comp_thr_db: -16.0, comp_ratio: 0.4, comp_attack: 0.5, comp_release: 0.3, comp_makeup_db: 3.0, exc_on: true, exc_lo: 0.35, exc_hi: 0.3, ..d.clone() }),
        ("Night / Loudness", Rack { comp_on: true, comp_thr_db: -28.0, comp_ratio: 0.75, comp_attack: 0.1, comp_release: 0.6, comp_makeup_db: 8.0, ..d.clone() }),
        ("Game Footsteps", Rack { comp_on: true, comp_thr_db: -30.0, comp_ratio: 0.6, comp_attack: 0.1, comp_release: 0.3, comp_makeup_db: 9.0, exc_on: true, exc_lo: 0.0, exc_hi: 0.4, ..d.clone() }),
        ("Voice Leveler", Rack { comp_on: true, comp_thr_db: -24.0, comp_ratio: 0.6, comp_attack: 0.2, comp_release: 0.5, comp_makeup_db: 6.0, ..d.clone() }),
        ("Crunch", Rack { pre_on: true, drive_db: 18.0, out_db: -8.0, exc_on: true, exc_lo: 0.4, exc_hi: 0.4, ..d.clone() }),
        ("Clean Boost", Rack { pre_on: true, trim_db: 6.0, drive_db: 0.0, ..d }),
    ]
}

impl Rack {
    /// (node:control, value) pairs for the filter-chain, honouring bypass.
    pub fn controls(&self) -> Vec<(&'static str, f64)> {
        let lin = |db: f64| 10f64.powf(db / 20.0);
        let pre = self.pre_on;
        vec![
            ("rk_trim:Mult", if pre { lin(self.trim_db) } else { 1.0 }),
            ("rk_sat:mode", if pre && self.drive_db > 0.0 { 1.0 } else { 0.0 }),
            ("rk_sat:gain (dB)", if pre { self.drive_db } else { 0.0 }),
            ("rk_out:Mult", if pre { lin(self.out_db) } else { 1.0 }),
            ("rk_gate:open (dB)", if self.gate_on { self.gate_db } else { -60.0 }),
            ("rk_gate:close (dB)", if self.gate_on { (self.gate_db - 12.0).max(-80.0) } else { -80.0 }),
            ("rk_comp:threshold", lin(self.comp_thr_db).clamp(0.0, 1.0)),
            ("rk_comp:strength", if self.comp_on { self.comp_ratio } else { 0.0 }),
            ("rk_comp:attack", self.comp_attack),
            ("rk_comp:release", self.comp_release),
            ("rk_comp:gain (dB)", if self.comp_on { self.comp_makeup_db } else { 0.0 }),
            ("rk_exc:lo.gain", if self.exc_on { self.exc_lo } else { 0.0 }),
            ("rk_exc:hi.gain", if self.exc_on { self.exc_hi } else { 0.0 }),
            // echo: three chained delays mixed back at mix, mix*fb, mix*fb² (repeats fade out)
            ("rk_d1:Delay (Seconds)", self.echo_time.clamp(0.02, 1.6)),
            ("rk_d2:Delay (Seconds)", self.echo_time.clamp(0.02, 1.6)),
            ("rk_d3:Delay (Seconds)", self.echo_time.clamp(0.02, 1.6)),
            ("rk_d1:Dry/Wet Balance", 1.0),
            ("rk_d2:Dry/Wet Balance", 1.0),
            ("rk_d3:Dry/Wet Balance", 1.0),
            ("rk_echo:Gain 1", 1.0),
            ("rk_echo:Gain 2", if self.echo_on { self.echo_mix } else { 0.0 }),
            ("rk_echo:Gain 3", if self.echo_on { self.echo_mix * self.echo_fb } else { 0.0 }),
            ("rk_echo:Gain 4", if self.echo_on { self.echo_mix * self.echo_fb * self.echo_fb } else { 0.0 }),
        ]
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Input {
    pub id: String,
    pub name: String,
    pub color: String,
    pub kind: InputKind,
    pub fx: String,
    #[serde(deserialize_with = "de_eq", default = "flat_eq")]
    pub eq: Vec<f64>,
    pub gain_db: f64,
    pub mute: bool,
    pub routes: BTreeMap<String, Route>,
    #[serde(default)]
    pub default_sink: bool,
    #[serde(default)]
    pub rack: Rack,
    #[serde(default)]
    pub normalize: bool,
    /// Auto-normalize: his last manual slider position. None = a config saved before #7 round 4.
    #[serde(default)]
    pub norm_base: Option<f64>,
    /// Auto-normalize: his wanted output level (dBFS) anchored from that position; None while pending.
    #[serde(default)]
    pub norm_out: Option<f64>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum BusKind {
    /// Physical output; the fader controls the device volume.
    Hardware { target: String },
    /// Virtual microphone other apps can record (OBS, Discord).
    Virtual { node: String, description: String },
    /// Streams over the network (DLNA renderer and/or HTTP URL, #15).
    Network,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Bus {
    pub id: String,
    pub name: String,
    pub color: String,
    pub kind: BusKind,
    pub gain_db: f64,
    pub mute: bool,
}

/// A DLNA media renderer picked for a Network bus (#15).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Renderer {
    pub udn: String,
    pub name: String,
}

/// Piano synth settings (FluidSynth playing your MIDI keyboard into the Piano channel).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct PianoCfg {
    pub program: u8, // General MIDI instrument
    pub reverb: f64, // 0..1
}

impl Default for PianoCfg {
    fn default() -> Self {
        PianoCfg { program: 0, reverb: 0.4 }
    }
}

/// Instruments offered in the Piano strip (General MIDI program numbers).
pub const INSTRUMENTS: &[(&str, u8)] = &[
    ("Grand Piano", 0), ("Bright Piano", 1), ("Honky-tonk", 3), ("Electric Piano", 4), ("FM Piano", 5),
    ("Harpsichord", 6), ("Clavinet", 7), ("Celesta", 8), ("Glockenspiel", 9), ("Music Box", 10),
    ("Vibraphone", 11), ("Marimba", 12), ("Tubular Bells", 14), ("Jazz Organ", 17), ("Rock Organ", 18),
    ("Church Organ", 19), ("Accordion", 21), ("Nylon Guitar", 24), ("Steel Guitar", 25), ("Clean Guitar", 27),
    ("Overdrive Guitar", 29), ("Distortion Guitar", 30), ("Acoustic Bass", 32), ("Finger Bass", 33),
    ("Slap Bass", 36), ("Synth Bass", 38), ("Violin", 40), ("Cello", 42), ("Pizzicato", 45), ("Harp", 46),
    ("Strings", 48), ("Slow Strings", 49), ("Synth Strings", 50), ("Choir Aahs", 52), ("Voice Oohs", 53),
    ("Orchestra Hit", 55), ("Trumpet", 56), ("Trombone", 57), ("French Horn", 60), ("Brass Section", 61),
    ("Synth Brass", 62), ("Alto Sax", 65), ("Tenor Sax", 66), ("Clarinet", 71), ("Flute", 73),
    ("Pan Flute", 75), ("Square Lead", 80), ("Saw Lead", 81), ("Chiff Lead", 83), ("Bass + Lead", 87),
    ("Warm Pad", 89), ("Polysynth", 90), ("Choir Pad", 91), ("Halo Pad", 94), ("Sweep Pad", 95),
    ("Rain FX", 96), ("Crystal FX", 98), ("Atmosphere", 99), ("Sci-fi", 103), ("Sitar", 104),
    ("Kalimba", 108), ("Steel Drums", 114), ("Synth Drum", 118),
];

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Config {
    pub inputs: Vec<Input>,
    pub buses: Vec<Bus>,
    #[serde(default)]
    pub piano: PianoCfg,
    /// Graph quantum forced by the engine (`pw-metadata clock.force-quantum`); 0 = don't force.
    #[serde(default = "default_quantum", deserialize_with = "lenient_quantum")]
    pub quantum: u32,
    /// Network bus id -> DLNA renderer it plays on (#15). Not part of `structure()`: changing it never restarts the graph.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub net_out: BTreeMap<String, Renderer>,
    /// Stream input id -> URL it pulls audio from (#15). Not part of `structure()`: changing it never restarts the graph.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub net_in: BTreeMap<String, String>,
}

fn default_quantum() -> u32 {
    256
}

/// Graph quantum from config: 0 = don't force, otherwise a power of two in 32..=2048.
/// Anything else is normalised (or 256 with a message) so a bad value can't fail the whole load.
pub fn normalize_quantum(v: &serde_json::Value) -> u32 {
    let n = v
        .as_u64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
        .or_else(|| v.as_f64().filter(|f| *f >= 0.0 && f.fract() == 0.0).map(|f| f as u64));
    match n {
        Some(0) => 0,
        Some(n) => (n.clamp(32, 2048) as u32).next_power_of_two(),
        None => {
            eprintln!("wavelink: config quantum {v} is invalid, using 256");
            256
        }
    }
}

fn lenient_quantum<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(normalize_quantum(&v))
}

pub fn db_to_vol(db: f64) -> f64 {
    if db <= MIN_DB { 0.0 } else { 10f64.powf(db / 60.0) } // wpctl volumes are cubic
}
pub fn vol_to_db(v: f64) -> f64 {
    if v <= 0.0 { MIN_DB } else { (60.0 * v.log10()).clamp(MIN_DB, MAX_DB) }
}
pub fn fmt_db(db: f64) -> String {
    if db <= MIN_DB { "-∞".into() } else { format!("{:+.1} dB", db) }
}

/// Node names used in the PipeWire graph.
pub mod names {
    pub fn ch(id: &str) -> String { format!("ch_{id}") }
    pub fn strip_in(id: &str) -> String { format!("strip_{id}.in") }
    pub fn strip(id: &str) -> String { format!("strip_{id}") }
    pub fn route(i: &str, b: &str) -> String { format!("route_{i}_{b}") }
    pub fn route_in(i: &str, b: &str) -> String { format!("route_{i}_{b}.in") }
    pub fn bus(id: &str) -> String { format!("bus_{id}") }
    pub fn bus_cap(id: &str) -> String { format!("bus_{id}.cap") }
    pub fn bus_out(id: &str) -> String { format!("bus_{id}.out") }
}

impl Input {
    pub fn route(&self, bus: &str) -> Route {
        self.routes.get(bus).cloned().unwrap_or(Route { on: false, level_db: 0.0 })
    }
}

impl Config {
    pub fn path() -> PathBuf {
        glib_home().join(".config/wavelink-mixer/config.json")
    }

    pub fn load() -> Config {
        let path = Self::path();
        let fresh = || {
            let c = Config::default_config();
            c.save();
            c
        };
        let Ok(bytes) = std::fs::read(&path) else {
            return fresh();
        };
        match serde_json::from_slice(&bytes) {
            Ok(c) => c,
            Err(_) => {
                // keep the user's bytes before defaults overwrite the file; never overwrite an earlier backup
                let mut bad = path.with_extension("json.bad");
                let mut n = 1;
                while bad.exists() {
                    bad = path.with_extension(format!("json.bad.{n}"));
                    n += 1;
                }
                match std::fs::write(&bad, &bytes) {
                    Ok(()) => {
                        eprintln!("wavelink: {} is unreadable, saved a copy to {}", path.display(), bad.display());
                        fresh()
                    }
                    Err(we) => {
                        eprintln!(
                            "wavelink: {} is unreadable and the copy to {} failed ({we}); using defaults without saving",
                            path.display(),
                            bad.display()
                        );
                        Config::default_config()
                    }
                }
            }
        }
    }

    pub fn save(&self) {
        let p = Self::path();
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        let tmp = p.with_extension("json.tmp");
        if std::fs::write(&tmp, serde_json::to_vec_pretty(self).unwrap()).is_ok() {
            let _ = std::fs::rename(tmp, p);
        }
    }

    /// Everything that needs the engine to rebuild the graph (not levels/mutes/EQ).
    pub fn structure(&self) -> String {
        let ins: Vec<_> = self.inputs.iter().map(|i| (&i.id, &i.name, &i.kind, &i.fx, i.default_sink)).collect();
        let bus: Vec<_> = self.buses.iter().map(|b| (&b.id, &b.name, &b.kind)).collect();
        serde_json::to_string(&(ins, bus)).unwrap()
    }

    pub fn new_id(&self, base: &str) -> String {
        let mut s: String = base.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
        if s.is_empty() || s.chars().next().unwrap().is_ascii_digit() {
            s = format!("x{s}");
        }
        let taken = |x: &str| self.inputs.iter().any(|i| i.id == x) || self.buses.iter().any(|b| b.id == x);
        let mut id = s.clone();
        let mut n = 2;
        while taken(&id) {
            id = format!("{s}{n}");
            n += 1;
        }
        id
    }

    /// Adds a Network bus (#15) that starts as a copy of the main mix. Returns its id.
    pub fn add_network_bus(&mut self, name: &str) -> String {
        let id = (1..).map(|k| format!("n{k}")).find(|id| !self.buses.iter().any(|b| &b.id == id)).unwrap();
        if let Some(main) = self.buses.iter().find(|b| matches!(b.kind, BusKind::Hardware { .. })).map(|b| b.id.clone()) {
            for i in &mut self.inputs {
                if let Some(r) = i.routes.get(&main).cloned() {
                    i.routes.insert(id.clone(), r);
                }
            }
        }
        self.net_out.remove(&id);
        let name = name.trim();
        let name = if name.is_empty() { "Network" } else { name };
        self.buses.push(Bus {
            id: id.clone(),
            name: name.into(),
            color: "#30d158".into(),
            kind: BusKind::Network,
            gain_db: 0.0,
            mute: false,
        });
        self.buses.sort_by(|a, b| a.id.cmp(&b.id));
        id
    }

    /// Adds a Virtual input that pulls `url` (#15). Returns its id.
    pub fn add_stream_input(&mut self, name: &str, url: &str) -> String {
        let name = name.trim();
        let name = if name.is_empty() { "Stream" } else { name };
        let id = self.new_id(name);
        let mut routes = BTreeMap::new();
        if let Some(first) = self.buses.first() {
            routes.insert(first.id.clone(), Route { on: true, level_db: 0.0 });
        }
        self.inputs.push(Input {
            id: id.clone(),
            name: name.into(),
            color: "#30d158".into(),
            kind: InputKind::Virtual,
            fx: "clean".into(),
            eq: flat_eq(),
            gain_db: 0.0,
            mute: false,
            routes,
            default_sink: false,
            rack: Rack::default(),
            normalize: false,
            norm_base: None,
            norm_out: None,
        });
        self.net_in.insert(id.clone(), url.trim().to_string());
        id
    }

    /// Removes an input or bus and every reference to it, including the #15 maps.
    pub fn remove_id(&mut self, id: &str) -> bool {
        let before = self.inputs.len() + self.buses.len();
        self.inputs.retain(|i| i.id != id);
        self.buses.retain(|b| b.id != id);
        for i in &mut self.inputs {
            i.routes.remove(id);
        }
        self.net_in.remove(id);
        self.net_out.remove(id);
        self.inputs.len() + self.buses.len() != before
    }

    pub fn default_config() -> Config {
        let r = |pairs: &[(&str, bool, f64)]| -> BTreeMap<String, Route> {
            pairs.iter().map(|&(b, on, db)| (b.to_string(), Route { on, level_db: db })).collect()
        };
        let input = |id: &str, name: &str, color: &str, fx: &str, routes| Input {
            id: id.into(), name: name.into(), color: color.into(), kind: InputKind::Virtual, fx: fx.into(),
            eq: flat_eq(), gain_db: 0.0, mute: false, routes, default_sink: id == "system", rack: Rack::default(), normalize: false, norm_base: None, norm_out: None,
        };
        Config {
            piano: PianoCfg::default(),
            quantum: 256,
            net_out: BTreeMap::new(),
            net_in: BTreeMap::new(),
            inputs: vec![
                input("system", "System", "#8e8e93", "clean", r(&[("a1", true, 0.0), ("b1", true, -13.3)])),
                input("spotify", "Spotify", "#1ed760", "music", r(&[("a1", true, -4.2), ("b1", true, -36.0)])),
                input("discord", "Discord", "#5865f2", "clean", r(&[("a1", true, -1.3), ("b1", true, -7.5)])),
                input("browser", "Browser", "#ff9500", "music", r(&[("a1", true, 0.0), ("b1", true, -13.3)])),
                input("game", "Game", "#ff2d55", "game", r(&[("a1", true, -2.7), ("b1", true, -2.7)])),
                Input {
                    id: "mic".into(), name: "Mic".into(), color: "#bf5af2".into(),
                    kind: InputKind::Source { target: "easyeffects_source".into() },
                    fx: "clean".into(), eq: flat_eq(), gain_db: 0.0, mute: false,
                    routes: r(&[("b1", true, 0.0), ("b2", true, 0.0)]), default_sink: false, rack: Rack::default(), normalize: false, norm_base: None, norm_out: None,
                },
            ],
            buses: vec![
                Bus { id: "a1".into(), name: "Headphones".into(), color: "#64d2ff".into(),
                      kind: BusKind::Hardware { target: SCARLETT.into() }, gain_db: -4.2, mute: false },
                Bus { id: "a2".into(), name: "Monitor".into(), color: "#30d158".into(),
                      kind: BusKind::Hardware { target: HDMI.into() }, gain_db: -23.9, mute: false },
                Bus { id: "b1".into(), name: "Stream".into(), color: "#ff453a".into(),
                      kind: BusKind::Virtual { node: "mix_recording.src".into(), description: "Recording Mix".into() },
                      gain_db: 0.0, mute: false },
                Bus { id: "b2".into(), name: "Voice Chat".into(), color: "#ffd60a".into(),
                      kind: BusKind::Virtual { node: "voice_chat_mix".into(), description: "Voice Chat Mix".into() },
                      gain_db: 0.0, mute: false },
            ],
        }
    }
}

pub fn glib_home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_quantum_defaults_to_256() {
        let mut v = serde_json::to_value(Config::default_config()).unwrap();
        v.as_object_mut().unwrap().remove("quantum");
        let c: Config = serde_json::from_value(v).unwrap();
        assert_eq!(c.quantum, 256);
        let mut v = serde_json::to_value(Config::default_config()).unwrap();
        v["quantum"] = serde_json::json!(512);
        let c: Config = serde_json::from_value(v).unwrap();
        assert_eq!(c.quantum, 512);
    }

    fn quantum_of(v: serde_json::Value) -> Result<u32, serde_json::Error> {
        let mut j = serde_json::to_value(Config::default_config()).unwrap();
        j["quantum"] = v;
        serde_json::from_value::<Config>(j).map(|c| c.quantum)
    }

    #[test]
    fn quantum_bad_values_normalised() {
        use serde_json::json;
        let cases = [
            (json!("256"), 256), (json!(-1), 256), (json!(null), 256), (json!(1.5), 256),
            (json!(0), 0), (json!(1), 32), (json!(16), 32), (json!(300), 512),
            (json!(512), 512), (json!(2048), 2048), (json!(4096), 2048),
            (json!("0"), 0), (json!(0.0), 0), (json!(" 512 "), 512), (json!(256.0), 256), (json!("abc"), 256),
        ];
        for (v, want) in cases {
            let got = quantum_of(v.clone()).unwrap_or_else(|e| panic!("{v} failed to parse: {e}"));
            assert_eq!(got, want, "quantum {v}");
        }
        let mut j = serde_json::to_value(Config::default_config()).unwrap();
        j["quantum"] = json!("256");
        let c: Config = serde_json::from_value(j).unwrap();
        assert_eq!(c.buses.len(), Config::default_config().buses.len());
    }

    #[test]
    fn load_backs_up_unparseable_config() {
        let home = std::env::temp_dir().join(format!("wavelink-load-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let dir = home.join(".config/wavelink-mixer");
        std::fs::create_dir_all(&dir).unwrap();
        let truncated: &[u8] = b"{\"buses\": [";
        std::fs::write(dir.join("config.json"), truncated).unwrap();
        let old = std::env::var_os("HOME");
        std::env::set_var("HOME", &home);
        let _ = Config::load();
        // a second bad save must not overwrite the first backup
        let second: &[u8] = b"{oops";
        std::fs::write(dir.join("config.json"), second).unwrap();
        let _ = Config::load();
        match old {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        let bad = std::fs::read(dir.join("config.json.bad"));
        let bad1 = std::fs::read(dir.join("config.json.bad.1"));
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(bad.unwrap(), truncated);
        assert_eq!(bad1.unwrap(), second);
    }

    #[test]
    fn normalize_defaults_off_and_round_trips() {
        let i = Input {
            id: "x".into(),
            name: "x".into(),
            color: "#fff".into(),
            kind: InputKind::Virtual,
            fx: "clean".into(),
            eq: flat_eq(),
            gain_db: 0.0,
            mute: false,
            routes: Default::default(),
            default_sink: false,
            rack: Rack::default(),
            normalize: true,
            norm_base: None,
            norm_out: None,
        };
        let s = serde_json::to_string(&i).unwrap();
        let back: Input = serde_json::from_str(&s).unwrap();
        assert!(back.normalize);
        let mut v: serde_json::Value = serde_json::from_str(&s).unwrap();
        v.as_object_mut().unwrap().remove("normalize");
        let old: Input = serde_json::from_value(v).unwrap();
        assert!(!old.normalize);
    }

    #[test]
    fn norm_state_loads_old_configs_and_round_trips() {
        let i = Input {
            id: "x".into(),
            name: "x".into(),
            color: "#fff".into(),
            kind: InputKind::Virtual,
            fx: "clean".into(),
            eq: flat_eq(),
            gain_db: -40.0,
            mute: false,
            routes: Default::default(),
            default_sink: false,
            rack: Rack::default(),
            normalize: true,
            norm_base: Some(-18.0),
            norm_out: Some(-26.0),
        };
        let s = serde_json::to_string(&i).unwrap();
        let back: Input = serde_json::from_str(&s).unwrap();
        assert_eq!(back, i);
        let mut v: serde_json::Value = serde_json::from_str(&s).unwrap();
        v.as_object_mut().unwrap().remove("norm_base");
        v.as_object_mut().unwrap().remove("norm_out");
        let old: Input = serde_json::from_value(v).unwrap();
        assert_eq!((old.norm_base, old.norm_out), (None, None));
        assert_eq!(old.gain_db, -40.0);
    }

    #[test]
    fn old_config_without_net_maps_loads() {
        let mut v = serde_json::to_value(Config::default_config()).unwrap();
        v.as_object_mut().unwrap().remove("net_out");
        v.as_object_mut().unwrap().remove("net_in");
        let c: Config = serde_json::from_value(v).unwrap();
        assert!(c.net_out.is_empty() && c.net_in.is_empty());
    }

    #[test]
    fn network_bus_kind_roundtrips() {
        let k: BusKind = serde_json::from_str(r#"{"type":"network"}"#).unwrap();
        assert_eq!(k, BusKind::Network);
        assert_eq!(serde_json::to_string(&BusKind::Network).unwrap(), r#"{"type":"network"}"#);
    }

    #[test]
    fn add_network_bus_copies_main_mix_routes() {
        let mut c = Config::default_config();
        let id = c.add_network_bus("Cast");
        assert_eq!(id, "n1");
        let b = c.buses.iter().find(|b| b.id == "n1").unwrap();
        assert_eq!(b.kind, BusKind::Network);
        assert_eq!(b.name, "Cast");
        for i in &c.inputs {
            assert_eq!(i.routes.get("n1"), i.routes.get("a1"), "input {}", i.id);
        }
        assert!(!c.net_out.contains_key("n1"));
        assert_eq!(c.add_network_bus("Other"), "n2");
        // a renderer left under a reused id must not survive the new bus
        let mut c = Config::default_config();
        c.net_out.insert("n1".into(), Renderer { udn: "uuid:old".into(), name: "Old".into() });
        c.add_network_bus("");
        assert!(!c.net_out.contains_key("n1"));
        assert_eq!(c.buses.iter().find(|b| b.id == "n1").unwrap().name, "Network");
    }

    #[test]
    fn add_stream_input_sets_url_and_route() {
        let mut c = Config::default_config();
        let id = c.add_stream_input("  Tone ", " http://10.0.0.1:5959/stream/n1.wav ");
        let i = c.inputs.iter().find(|i| i.id == id).unwrap();
        assert_eq!(i.name, "Tone");
        assert_eq!(i.kind, InputKind::Virtual);
        assert_eq!(c.net_in.get(&id).map(String::as_str), Some("http://10.0.0.1:5959/stream/n1.wav"));
        assert!(i.route("a1").on);
    }

    #[test]
    fn remove_id_clears_maps_and_routes() {
        let mut c = Config::default_config();
        let inp = c.add_stream_input("Tone", "http://10.0.0.1:5959/s.wav");
        let out = c.add_network_bus("Cast");
        c.net_out.insert(out.clone(), Renderer { udn: "uuid:x".into(), name: "TV".into() });
        assert!(c.remove_id(&out));
        assert!(!c.net_out.contains_key(&out));
        assert!(!c.buses.iter().any(|b| b.id == out));
        assert!(c.inputs.iter().all(|i| !i.routes.contains_key(&out)));
        assert!(c.remove_id(&inp));
        assert!(!c.net_in.contains_key(&inp));
        assert!(!c.inputs.iter().any(|i| i.id == inp));
        assert!(!c.remove_id("nope"));
    }

    #[test]
    fn net_maps_do_not_change_structure() {
        let mut c = Config::default_config();
        let before = c.structure();
        c.net_out.insert("a1".into(), Renderer { udn: "uuid:x".into(), name: "TV".into() });
        c.net_in.insert("system".into(), "http://10.0.0.1:5959/s.wav".into());
        assert_eq!(c.structure(), before);
    }
}
