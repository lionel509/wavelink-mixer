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
        Self::load_from(&Self::path())
    }

    /// `load` for an explicit path (the remote server's tests use a temp dir).
    pub fn load_from(path: &std::path::Path) -> Config {
        let path = path.to_path_buf();
        let fresh = || {
            let c = Config::default_config();
            c.save_to(&path);
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
        self.save_to(&Self::path());
    }

    /// Atomic write: a per-process temp file renamed over `p` (two writers never share a temp name).
    pub fn save_to(&self, p: &std::path::Path) -> bool {
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        let tmp = p.with_extension(format!("json.tmp.{}", std::process::id()));
        if std::fs::write(&tmp, serde_json::to_vec_pretty(self).unwrap()).is_ok() && std::fs::rename(&tmp, p).is_ok() {
            return true;
        }
        let _ = std::fs::remove_file(&tmp);
        false
    }

    /// Read the file as it is now without the load() side effects (no defaults written, no backup); None if missing or unparsable.
    pub fn peek(p: &std::path::Path) -> Option<Config> {
        serde_json::from_slice(&std::fs::read(p).ok()?).ok()
    }

    /// Save without clobbering another writer (the engine's remote server, #26): under the config lock,
    /// if the file changed since `base` (what this process last read or wrote), merge field by field
    /// (our edits since `base` win, everything else comes from the file) before writing.
    /// `self` becomes the merged config and `base` the written one. True when the file changed under us.
    pub fn save_merged(&mut self, base: &mut Config, p: &std::path::Path) -> bool {
        let _lock = lock_config(p);
        let disk = Config::peek(p);
        let mut changed = false;
        if let Some(d) = disk.filter(|d| d != base) {
            if let Some(m) = merge3(base, self, &d) {
                *self = m;
            }
            changed = true;
        }
        if self.save_to(p) {
            *base = self.clone();
        }
        changed
    }

    /// Pull in edits another writer saved since `base`; our unsaved edits since `base` stay.
    /// Returns the file's config when it differed from `base` (`self` is then merged, `base` = file).
    pub fn pull_merged(&mut self, base: &mut Config, p: &std::path::Path) -> Option<Config> {
        let d = Config::peek(p)?;
        if &d == base {
            return None;
        }
        if let Some(m) = merge3(base, self, &d) {
            *self = m;
        }
        *base = d.clone();
        Some(d)
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
        // CHANNEL_ID belongs to the Stems feature: a user channel named "Stems" gets a suffixed id.
        let taken = |x: &str| x == crate::stemcfg::CHANNEL_ID || self.inputs.iter().any(|i| i.id == x) || self.buses.iter().any(|b| b.id == x);
        let mut id = s.clone();
        let mut n = 2;
        while taken(&id) {
            id = format!("{s}{n}");
            n += 1;
        }
        id
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

/// Exclusive advisory lock on `<config dir>/config.lock` for a read-modify-write of `p`; released on drop.
/// Best effort: if the lock file can't be opened the write goes ahead unlocked.
pub fn lock_config(p: &std::path::Path) -> Option<std::fs::File> {
    let lp = p.with_extension("lock");
    let _ = std::fs::create_dir_all(lp.parent()?);
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(lp).ok()?;
    f.lock().ok()?;
    Some(f)
}

/// Three-way merge of JSON-serialised configs: for each leaf, `mine` wins where it differs from
/// `base`, otherwise `theirs`. Arrays of objects with an "id" (inputs, buses) merge per id; other arrays
/// (EQ bands) are leaves. None if the merge doesn't deserialize back (then the caller keeps `mine`).
pub fn merge3(base: &Config, mine: &Config, theirs: &Config) -> Option<Config> {
    use serde_json::Value;
    fn ids(v: &Value) -> Option<Vec<String>> {
        v.as_array()?.iter().map(|x| x.get("id").and_then(|i| i.as_str()).map(String::from)).collect()
    }
    fn m(base: &Value, mine: &Value, theirs: &Value) -> Value {
        if mine == base {
            return theirs.clone();
        }
        if theirs == base || theirs == mine {
            return mine.clone();
        }
        match (base, mine, theirs) {
            (Value::Object(b), Value::Object(mi), Value::Object(th)) => {
                let mut out = serde_json::Map::new();
                let keys: std::collections::BTreeSet<&String> = b.keys().chain(mi.keys()).chain(th.keys()).collect();
                for k in keys {
                    match (b.get(k), mi.get(k), th.get(k)) {
                        (_, Some(x), Some(y)) => {
                            out.insert(k.clone(), m(b.get(k).unwrap_or(&Value::Null), x, y));
                        }
                        // removed on one side: keep the removal unless the other side edited it
                        (Some(bv), Some(x), None) => {
                            if x != bv {
                                out.insert(k.clone(), x.clone());
                            }
                        }
                        (Some(bv), None, Some(y)) => {
                            if y != bv {
                                out.insert(k.clone(), y.clone());
                            }
                        }
                        (None, Some(x), None) => {
                            out.insert(k.clone(), x.clone());
                        }
                        (None, None, Some(y)) => {
                            out.insert(k.clone(), y.clone());
                        }
                        _ => {}
                    }
                }
                Value::Object(out)
            }
            (Value::Array(b), Value::Array(mi), Value::Array(th)) => match (ids(base), ids(mine), ids(theirs)) {
                (Some(bi), Some(mii), Some(thi)) => {
                    // order: mine's order if mine reordered/added/removed, else theirs'; per-id merge
                    let order = if mii != bi { mii.clone() } else { thi.clone() };
                    let get = |arr: &Vec<Value>, idv: &Vec<String>, id: &str| idv.iter().position(|x| x == id).map(|i| arr[i].clone());
                    let mut out = vec![];
                    let mut seen = std::collections::HashSet::new();
                    for id in order.iter().chain(thi.iter()).chain(mii.iter()) {
                        if !seen.insert(id.clone()) {
                            continue;
                        }
                        let (bv, mv, tv) = (get(b, &bi, id), get(mi, &mii, id), get(th, &thi, id));
                        match (bv, mv, tv) {
                            (bv, Some(x), Some(y)) => out.push(m(&bv.unwrap_or(Value::Null), &x, &y)),
                            (Some(bv), Some(x), None) if x != bv => out.push(x),
                            (Some(bv), None, Some(y)) if y != bv => out.push(y),
                            (None, Some(x), None) => out.push(x),
                            (None, None, Some(y)) => out.push(y),
                            _ => {}
                        }
                    }
                    Value::Array(out)
                }
                _ => mine.clone(),
            },
            // both sides changed the same leaf: ours wins (the caller's in-memory edit is the newest intent here)
            _ => mine.clone(),
        }
    }
    let (b, mi, th) = (serde_json::to_value(base).ok()?, serde_json::to_value(mine).ok()?, serde_json::to_value(theirs).ok()?);
    serde_json::from_value(m(&b, &mi, &th)).ok()
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
    fn merge3_keeps_both_sides_edits() {
        let base = Config::default_config();
        let mut mine = base.clone();
        let mut theirs = base.clone();
        mine.inputs[0].gain_db = -7.0; // GUI moved System
        mine.inputs[1].eq[3] = 4.0;
        theirs.inputs[1].gain_db = -11.0; // remote moved Spotify
        theirs.buses[0].mute = true;
        theirs.inputs[2].routes.get_mut("a1").unwrap().on = false;
        let m = merge3(&base, &mine, &theirs).unwrap();
        assert_eq!(m.inputs[0].gain_db, -7.0);
        assert_eq!(m.inputs[1].eq[3], 4.0);
        assert_eq!(m.inputs[1].gain_db, -11.0);
        assert!(m.buses[0].mute);
        assert!(!m.inputs[2].routes["a1"].on);
        // same leaf on both sides: ours
        theirs.inputs[0].gain_db = -30.0;
        assert_eq!(merge3(&base, &mine, &theirs).unwrap().inputs[0].gain_db, -7.0);
        // an input added on their side, one removed on ours
        let mut theirs2 = base.clone();
        let mut extra = base.inputs[0].clone();
        extra.id = "new".into();
        theirs2.inputs.push(extra);
        let mut mine2 = base.clone();
        mine2.inputs.retain(|i| i.id != "game");
        let m = merge3(&base, &mine2, &theirs2).unwrap();
        let ids: Vec<_> = m.inputs.iter().map(|i| i.id.as_str()).collect();
        assert!(ids.contains(&"new") && !ids.contains(&"game"), "{ids:?}");
    }

    #[test]
    fn save_merged_never_writes_stale_state_over_newer_file() {
        let dir = std::env::temp_dir().join(format!("wavelink-merge-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = dir.join("config.json");
        let mut base = Config::default_config();
        assert!(base.save_to(&p));
        let mut gui = base.clone();
        // another writer (the remote server) changes the file
        let mut other = Config::peek(&p).unwrap();
        other.buses[1].gain_db = -40.0;
        other.save_to(&p);
        // the GUI saves its own, unrelated edit
        gui.inputs[0].mute = true;
        assert!(gui.save_merged(&mut base, &p));
        let disk = Config::peek(&p).unwrap();
        assert_eq!(disk.buses[1].gain_db, -40.0, "the other writer's edit survived");
        assert!(disk.inputs[0].mute, "the GUI's edit landed");
        assert_eq!(base, disk);
        // pull: the file changes again, the GUI picks it up
        let mut other = disk.clone();
        other.inputs[3].gain_db = -2.5;
        other.save_to(&p);
        assert!(gui.pull_merged(&mut base, &p).is_some());
        assert_eq!(gui.inputs[3].gain_db, -2.5);
        assert!(gui.pull_merged(&mut base, &p).is_none());
        let _ = std::fs::remove_dir_all(&dir);
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
}
