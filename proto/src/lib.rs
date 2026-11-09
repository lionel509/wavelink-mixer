//! Wire protocol between the wavelink-mixer engine (server) and its LAN clients.
//!
//! Transport: WebSocket, one JSON text frame per message.
//!
//! ```text
//! client                                server
//!   Hello{protocol, token, client} --->
//!                                 <---  Welcome{protocol, server}   (or Error{auth|protocol} + close)
//!                                 <---  State{state}                (full snapshot)
//!   Command{id, cmd}               --->
//!                                 <---  Ack{id, ok, error?, note?}
//!                                 <---  Delta{rev, changes}         (to every client, incl. the sender)
//!   Subscribe{meters: true}        --->
//!                                 <---  Meters{meters}              (~15 per second while subscribed)
//! ```
//!
//! The model is open-ended on purpose: inputs and buses are lists keyed by id (no fixed bus set), a
//! bus feeds a list of `outputs` (later: several speakers, possibly on other hosts), and `zones`
//! group buses so several outputs can be adjusted together later. Unknown fields are ignored by
//! serde, so a newer server can add fields without breaking older clients of the same protocol.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Bumped on any incompatible change. The server refuses a Hello with a different number.
pub const PROTOCOL_VERSION: u32 = 1;
pub const DEFAULT_PORT: u16 = 8790;
/// DNS-SD service type the engine advertises (`<name>._wavelink._tcp.local.`).
pub const MDNS_SERVICE: &str = "_wavelink._tcp";
pub const METER_FPS: u32 = 15;
/// Fader range, dB. At or below MIN_DB means silent (-inf).
pub const MIN_DB: f64 = -60.0;
pub const MAX_DB: f64 = 12.0;
/// Range a normalize target (wanted output loudness, dBFS) is clamped to.
pub const NORM_TARGET_MIN_DB: f64 = -50.0;
pub const NORM_TARGET_MAX_DB: f64 = -6.0;

// ---------------------------------------------------------------- state

/// A full snapshot of the mixer.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct State {
    /// Increases on every change; deltas carry the rev they produce.
    pub rev: u64,
    /// Human name of the engine (hostname).
    pub server: String,
    pub inputs: Vec<Input>,
    pub buses: Vec<Bus>,
    /// Groups of buses adjusted together (empty in protocol 1 servers; reserved for multi-speaker rooms).
    #[serde(default)]
    pub zones: Vec<Zone>,
    /// Computers that dialled in and carry their own system volume (MacBook). Additive: older clients ignore it.
    #[serde(default)]
    pub hosts: Vec<Host>,
}

/// A computer whose own output volume the mixer drives (its system volume, not a PipeWire node).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Host {
    /// Sanitised hostname (`console::sanitize_host_id`), the key.
    pub id: String,
    pub label: String,
    /// 0..=100, the host's own output volume.
    pub volume: u8,
    pub mute: bool,
    /// False while the host's console is not connected.
    pub online: bool,
}

/// What a host sends in Hello to register itself with its current volume.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct HostHello {
    pub id: String,
    pub label: String,
    pub volume: u8,
    pub mute: bool,
}

/// One channel strip.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Input {
    pub id: String,
    pub name: String,
    /// `#rrggbb`
    pub color: String,
    pub source: InputSource,
    pub gain_db: f64,
    pub mute: bool,
    pub normalize: bool,
    /// Wanted output loudness (dBFS) auto-normalize aims for; None = not anchored yet (default target).
    #[serde(default)]
    pub norm_target_db: Option<f64>,
    /// Sends, keyed by bus id. A bus missing here is an off send at 0 dB.
    pub routes: BTreeMap<String, Route>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputSource {
    /// Apps play into it (a virtual sink).
    App,
    /// A capture device or virtual source.
    Device { target: String },
    #[serde(other)]
    Other,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct Route {
    pub on: bool,
    pub level_db: f64,
}

impl Default for Route {
    fn default() -> Self {
        Route { on: false, level_db: 0.0 }
    }
}

/// An output mix.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Bus {
    pub id: String,
    pub name: String,
    pub color: String,
    pub kind: BusKind,
    /// For physical buses this is the device volume itself.
    pub gain_db: f64,
    pub mute: bool,
    /// Where the bus plays. Usually one entry today; a list so a bus can later feed several speakers.
    #[serde(default)]
    pub outputs: Vec<Output>,
    /// Soft safety cap: the server refuses to raise the gain above this unless this connection holds
    /// an unlock for the bus (`ClientMsg::Unlock`, 60 s). Never above `ceiling_db`.
    #[serde(default)]
    pub max_db: Option<f64>,
    /// Hard ceiling (the mixer's own per-bus `max_db`, e.g. -20 dB for an AirPlay speaker): no writer,
    /// unlocked or not, ever sets the bus louder. None = the fader's top (MAX_DB).
    #[serde(default)]
    pub ceiling_db: Option<f64>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BusKind {
    /// Real speakers/headphones: gain raises are limited per command (see `safety`).
    Physical,
    /// A virtual microphone/mix other apps record.
    Virtual,
    #[serde(other)]
    Other,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Output {
    /// PipeWire node (or device) name.
    pub node: String,
    /// Display label.
    #[serde(default)]
    pub label: String,
    /// Host that owns the device; None = the engine's own machine.
    #[serde(default)]
    pub host: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Zone {
    pub id: String,
    pub name: String,
    pub buses: Vec<String>,
}

/// A host's output volume is 0..=100. Every place that writes one (diff, apply, commands) clamps through here
/// so the client and the server always agree on the value.
fn clamp_host_volume(v: u8) -> u8 {
    v.min(100)
}

impl State {
    pub fn input(&self, id: &str) -> Option<&Input> {
        self.inputs.iter().find(|i| i.id == id)
    }
    pub fn bus(&self, id: &str) -> Option<&Bus> {
        self.buses.iter().find(|b| b.id == id)
    }
    fn input_mut(&mut self, id: &str) -> Option<&mut Input> {
        self.inputs.iter_mut().find(|i| i.id == id)
    }
    fn bus_mut(&mut self, id: &str) -> Option<&mut Bus> {
        self.buses.iter_mut().find(|b| b.id == id)
    }
    pub fn host(&self, id: &str) -> Option<&Host> {
        self.hosts.iter().find(|h| h.id == id)
    }
    fn host_mut(&mut self, id: &str) -> Option<&mut Host> {
        self.hosts.iter_mut().find(|h| h.id == id)
    }

    /// Apply one delta (client side). False if it names something this state does not have.
    pub fn apply(&mut self, d: &Delta) -> bool {
        match d {
            Delta::InputGain { input, db } => self.input_mut(input).map(|i| i.gain_db = *db).is_some(),
            Delta::InputMute { input, mute } => self.input_mut(input).map(|i| i.mute = *mute).is_some(),
            Delta::Route { input, bus, route } => self.input_mut(input).map(|i| i.routes.insert(bus.clone(), *route)).is_some(),
            Delta::Normalize { input, on, target_db } => self
                .input_mut(input)
                .map(|i| {
                    i.normalize = *on;
                    i.norm_target_db = *target_db;
                })
                .is_some(),
            Delta::BusGain { bus, db } => self.bus_mut(bus).map(|b| b.gain_db = *db).is_some(),
            Delta::BusMute { bus, mute } => self.bus_mut(bus).map(|b| b.mute = *mute).is_some(),
            Delta::HostVolume { host, volume } => self.host_mut(host).map(|h| h.volume = clamp_host_volume(*volume)).is_some(),
            Delta::HostMute { host, mute } => self.host_mut(host).map(|h| h.mute = *mute).is_some(),
            Delta::HostOnline { host, online } => self.host_mut(host).map(|h| h.online = *online).is_some(),
        }
    }

    /// Same channels and buses (ids, names, colours, kinds, caps, outputs, zones): only levels differ.
    pub fn same_structure(&self, o: &State) -> bool {
        let ins = |s: &State| s.inputs.iter().map(|i| (i.id.clone(), i.name.clone(), i.color.clone(), i.source.clone())).collect::<Vec<_>>();
        let bus = |s: &State| s.buses.iter().map(|b| (b.id.clone(), b.name.clone(), b.color.clone(), b.kind, b.outputs.clone(), b.max_db, b.ceiling_db)).collect::<Vec<_>>();
        let hosts = |s: &State| s.hosts.iter().map(|h| (h.id.clone(), h.label.clone())).collect::<Vec<_>>();
        ins(self) == ins(o) && bus(self) == bus(o) && self.zones == o.zones && self.server == o.server && hosts(self) == hosts(o)
    }

    /// Deltas that turn `self` into `new`; None when the structure changed (send a full State instead).
    pub fn diff(&self, new: &State) -> Option<Vec<Delta>> {
        if !self.same_structure(new) {
            return None;
        }
        let mut v = vec![];
        for (a, b) in self.inputs.iter().zip(&new.inputs) {
            if a.gain_db != b.gain_db {
                v.push(Delta::InputGain { input: b.id.clone(), db: b.gain_db });
            }
            if a.mute != b.mute {
                v.push(Delta::InputMute { input: b.id.clone(), mute: b.mute });
            }
            if a.normalize != b.normalize || a.norm_target_db != b.norm_target_db {
                v.push(Delta::Normalize { input: b.id.clone(), on: b.normalize, target_db: b.norm_target_db });
            }
            let keys: std::collections::BTreeSet<&String> = a.routes.keys().chain(b.routes.keys()).collect();
            for k in keys {
                let (ra, rb) = (a.routes.get(k).copied().unwrap_or_default(), b.routes.get(k).copied().unwrap_or_default());
                if ra != rb {
                    v.push(Delta::Route { input: b.id.clone(), bus: k.clone(), route: rb });
                }
            }
        }
        for (a, b) in self.buses.iter().zip(&new.buses) {
            if a.gain_db != b.gain_db {
                v.push(Delta::BusGain { bus: b.id.clone(), db: b.gain_db });
            }
            if a.mute != b.mute {
                v.push(Delta::BusMute { bus: b.id.clone(), mute: b.mute });
            }
        }
        for (a, b) in self.hosts.iter().zip(&new.hosts) {
            if clamp_host_volume(a.volume) != clamp_host_volume(b.volume) {
                v.push(Delta::HostVolume { host: b.id.clone(), volume: clamp_host_volume(b.volume) });
            }
            if a.mute != b.mute {
                v.push(Delta::HostMute { host: b.id.clone(), mute: b.mute });
            }
            if a.online != b.online {
                v.push(Delta::HostOnline { host: b.id.clone(), online: b.online });
            }
        }
        Some(v)
    }
}

impl Input {
    pub fn route(&self, bus: &str) -> Route {
        self.routes.get(bus).copied().unwrap_or_default()
    }
}

impl Bus {
    pub fn physical(&self) -> bool {
        self.kind == BusKind::Physical
    }
}

/// One level change. Structural changes (a channel added, renamed, recoloured...) are sent as a full State.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "what", rename_all = "snake_case")]
pub enum Delta {
    InputGain { input: String, db: f64 },
    InputMute { input: String, mute: bool },
    Route { input: String, bus: String, route: Route },
    Normalize { input: String, on: bool, target_db: Option<f64> },
    BusGain { bus: String, db: f64 },
    BusMute { bus: String, mute: bool },
    HostVolume { host: String, volume: u8 },
    HostMute { host: String, mute: bool },
    HostOnline { host: String, online: bool },
}

// ---------------------------------------------------------------- commands

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Command {
    SetInputGain { input: String, db: f64 },
    SetInputMute { input: String, mute: bool },
    /// Change a send; either field may be left out.
    SetRoute {
        input: String,
        bus: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        on: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        level_db: Option<f64>,
    },
    /// Auto-normalize on/off and/or its target loudness (dBFS).
    SetNormalize {
        input: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        on: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target_db: Option<f64>,
    },
    /// `unlock` is IGNORED by the server (a per-command flag any client could set; breaker #36 B36-1).
    /// Unlocking is per connection and time-limited: send `ClientMsg::Unlock`. Kept for wire compatibility.
    SetBusGain {
        bus: String,
        db: f64,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        unlock: bool,
    },
    SetBusMute { bus: String, mute: bool },
    /// A host's own output volume (0..=100). Raises are limited to `safety::MAX_HOST_STEP` per command.
    SetHostVolume { host: String, volume: u8 },
    SetHostMute { host: String, mute: bool },
}

// ---------------------------------------------------------------- meters

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Meters {
    pub inputs: Vec<Level>,
    pub buses: Vec<Level>,
}

/// Post-fader level of one input or bus over the last frame, dBFS (-90 = silence).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Level {
    pub id: String,
    pub peak_db: f32,
    pub rms_db: f32,
}

// ---------------------------------------------------------------- messages

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Hello {
    pub protocol: u32,
    pub token: String,
    /// Free-form client name, e.g. "Wavelink Remote 0.1 (macos)".
    #[serde(default)]
    pub client: String,
    /// Start meters right away.
    #[serde(default)]
    pub meters: bool,
    /// Set by a computer that owns a host (its system volume). Absent for plain clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostHello>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello(Hello),
    Command { id: u64, cmd: Command },
    Subscribe { meters: bool },
    Ping { t: u64 },
    /// Lift (`on`) or restore a bus's soft cap for THIS connection only. The server expires it after
    /// `safety::UNLOCK_S` seconds and on disconnect, and logs it. The hard ceiling always stays.
    Unlock { bus: String, on: bool },
    /// A host reports its system volume/mute after a change it saw itself (volume keys). Host connections only.
    HostState { volume: u8, mute: bool },
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Wrong or missing token.
    Auth,
    /// Protocol version mismatch.
    Protocol,
    /// Unparseable or out-of-order message.
    BadRequest,
    /// The server is shutting down or overloaded.
    Unavailable,
    #[serde(other)]
    Other,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Welcome { protocol: u32, server: String, version: String },
    Error { code: ErrorCode, message: String },
    State { state: State },
    Delta { rev: u64, changes: Vec<Delta> },
    /// Result of one Command. `note` explains a clamp (e.g. "capped at -20 dB").
    Ack {
        id: u64,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    Meters { meters: Meters },
    Pong { t: u64 },
    /// This connection's unlock state for a bus changed: `seconds` left (0 = locked again).
    Unlock { bus: String, seconds: u32 },
}

impl ClientMsg {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("serialisable")
    }
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }
}

impl ServerMsg {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("serialisable")
    }
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }
}

// ---------------------------------------------------------------- safety

/// Command validation and loudness caps, shared by the server (enforces) and clients (previews).
pub mod safety {
    use super::*;

    /// A physical bus's gain may rise at most this much per command. Lowering is never limited.
    pub const MAX_STEP_DB: f64 = 6.0;
    /// ... and at most this much within any RAISE_WINDOW_S seconds, across all commands and clients
    /// (enforced by the server, which owns the clock; see `RaiseLimiter`).
    pub const MAX_RAISE_PER_WINDOW_DB: f64 = 12.0;
    pub const RAISE_WINDOW_S: f64 = 1.0;
    /// Default soft cap for network speakers (Sonos/AirPlay: their sink volume IS the speaker volume).
    pub const SPEAKER_CAP_DB: f64 = -20.0;
    /// How long a `ClientMsg::Unlock` lasts.
    pub const UNLOCK_S: u64 = 60;
    /// A host's output volume may rise at most this much (percentage points) per SetHostVolume. Lowering is never limited.
    pub const MAX_HOST_STEP: u8 = 25;

    /// Time-based raise limit for one physical bus: the gain may never be more than
    /// MAX_RAISE_PER_WINDOW_DB above the lowest level it had within the last RAISE_WINDOW_S seconds.
    /// Decreases are never limited. Time is passed in (seconds, monotonic) so it can be tested.
    #[derive(Debug, Clone, Default)]
    pub struct RaiseLimiter {
        /// (time, level before, level after) of recent changes
        hist: Vec<(f64, f64, f64)>,
    }

    impl RaiseLimiter {
        /// Highest gain allowed now for a bus currently at `cur`.
        pub fn allowed(&mut self, now: f64, cur: f64) -> f64 {
            self.hist.retain(|(t, _, _)| now - t < RAISE_WINDOW_S);
            let floor = self.hist.iter().flat_map(|(_, a, b)| [*a, *b]).fold(cur, f64::min);
            floor + MAX_RAISE_PER_WINDOW_DB
        }
        /// Record a change that was applied.
        pub fn record(&mut self, now: f64, before: f64, after: f64) {
            if before != after {
                self.hist.push((now, before, after));
            }
        }
    }

    #[derive(Debug, Clone, PartialEq)]
    pub enum Reject {
        UnknownInput(String),
        UnknownBus(String),
        UnknownHost(String),
        HostOffline(String),
        NotFinite,
        Empty,
    }

    impl std::fmt::Display for Reject {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Reject::UnknownInput(i) => write!(f, "unknown input '{i}'"),
                Reject::UnknownBus(b) => write!(f, "unknown bus '{b}'"),
                Reject::UnknownHost(h) => write!(f, "unknown host '{h}'"),
                Reject::HostOffline(h) => write!(f, "host '{h}' is offline"),
                Reject::NotFinite => write!(f, "value is not a finite number"),
                Reject::Empty => write!(f, "nothing to change"),
            }
        }
    }

    /// What a command really does after clamping: the commands to apply, plus a note when clamped.
    #[derive(Debug, Clone, PartialEq)]
    pub struct Checked {
        pub cmds: Vec<Command>,
        pub note: Option<String>,
    }

    fn finite(x: f64) -> Result<f64, Reject> {
        if x.is_finite() {
            Ok(x)
        } else {
            Err(Reject::NotFinite)
        }
    }

    fn fader(db: f64) -> f64 {
        db.clamp(MIN_DB, MAX_DB)
    }

    /// Validate `cmd` against `state` and clamp it to the safety rules:
    /// - every dB value must be finite; faders clamp to MIN_DB..=MAX_DB, normalize targets to their range;
    /// - a bus is never set above its hard `ceiling_db`, by anyone;
    /// - a bus with `max_db` is never raised above it unless `unlocked(bus)` (the server's per-connection,
    ///   time-limited unlock; the command's own `unlock` field is ignored);
    /// - a physical bus rises at most MAX_STEP_DB per command (from its current gain); the server adds
    ///   the time-based `RaiseLimiter` on top;
    /// - unmuting a capped bus whose gain is above its (effective) cap also brings the gain down to it.
    /// Emitted commands always carry `unlock: false`.
    pub fn check(cmd: &Command, state: &State, unlocked: &dyn Fn(&str) -> bool) -> Result<Checked, Reject> {
        let input = |id: &str| state.input(id).ok_or_else(|| Reject::UnknownInput(id.into()));
        let bus = |id: &str| state.bus(id).ok_or_else(|| Reject::UnknownBus(id.into()));
        let host = |id: &str| match state.host(id) {
            None => Err(Reject::UnknownHost(id.into())),
            Some(h) if !h.online => Err(Reject::HostOffline(id.into())),
            Some(h) => Ok(h),
        };
        let mut note = None;
        let cmds = match cmd {
            Command::SetInputGain { input: i, db } => {
                input(i)?;
                vec![Command::SetInputGain { input: i.clone(), db: fader(finite(*db)?) }]
            }
            Command::SetInputMute { input: i, mute } => {
                input(i)?;
                vec![Command::SetInputMute { input: i.clone(), mute: *mute }]
            }
            Command::SetRoute { input: i, bus: b, on, level_db } => {
                input(i)?;
                bus(b)?;
                if on.is_none() && level_db.is_none() {
                    return Err(Reject::Empty);
                }
                let level_db = match level_db {
                    Some(l) => Some(fader(finite(*l)?)),
                    None => None,
                };
                vec![Command::SetRoute { input: i.clone(), bus: b.clone(), on: *on, level_db }]
            }
            Command::SetNormalize { input: i, on, target_db } => {
                input(i)?;
                if on.is_none() && target_db.is_none() {
                    return Err(Reject::Empty);
                }
                let target_db = match target_db {
                    Some(t) => Some(finite(*t)?.clamp(NORM_TARGET_MIN_DB, NORM_TARGET_MAX_DB)),
                    None => None,
                };
                vec![Command::SetNormalize { input: i.clone(), on: *on, target_db }]
            }
            Command::SetBusGain { bus: b, db, .. } => {
                let bs = bus(b)?;
                let want = fader(finite(*db)?);
                let mut got = want;
                if bs.physical() && got > bs.gain_db + MAX_STEP_DB {
                    got = (bs.gain_db + MAX_STEP_DB).min(MAX_DB);
                    note = Some(format!("raised at most {MAX_STEP_DB} dB per step"));
                }
                if let Some(cap) = bs.max_db.filter(|_| !unlocked(b)) {
                    if got > cap {
                        got = cap;
                        note = Some(format!("capped at {cap} dB (unlock the bus to go higher)"));
                    }
                }
                if let Some(ceil) = bs.ceiling_db {
                    if got > ceil {
                        got = ceil;
                        note = Some(format!("at the bus ceiling {ceil} dB (never louder)"));
                    }
                }
                vec![Command::SetBusGain { bus: b.clone(), db: got, unlock: false }]
            }
            Command::SetBusMute { bus: b, mute } => {
                let bs = bus(b)?;
                let mut v = vec![];
                let soft = bs.max_db.filter(|_| !unlocked(b));
                let cap = match (soft, bs.ceiling_db) {
                    (Some(a), Some(c)) => Some(a.min(c)),
                    (a, c) => a.or(c),
                };
                if let Some(cap) = cap {
                    if !*mute && bs.mute && bs.gain_db > cap {
                        v.push(Command::SetBusGain { bus: b.clone(), db: cap, unlock: false });
                        note = Some(format!("gain brought down to the {cap} dB cap before unmuting"));
                    }
                }
                v.push(Command::SetBusMute { bus: b.clone(), mute: *mute });
                v
            }
            Command::SetHostVolume { host: h, volume } => {
                let hs = host(h)?;
                let mut got = clamp_host_volume(*volume);
                let ceil = hs.volume.saturating_add(MAX_HOST_STEP).min(100);
                if got > hs.volume && got > ceil {
                    got = ceil;
                    note = Some(format!("raised at most {MAX_HOST_STEP} per step"));
                }
                vec![Command::SetHostVolume { host: h.clone(), volume: got }]
            }
            Command::SetHostMute { host: h, mute } => {
                host(h)?;
                vec![Command::SetHostMute { host: h.clone(), mute: *mute }]
            }
        };
        Ok(Checked { cmds, note })
    }
}
