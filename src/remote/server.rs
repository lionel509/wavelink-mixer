//! WebSocket control server: accept threads (one per listen address), one thread per client, a config
//! watcher and a meter pump.
//!
//! The server is the single writer of config.json for remote edits: every command is applied to the
//! freshest file (read under the config lock), saved atomically, pushed into the graph, and broadcast
//! to every client as a delta. Edits other writers make (the GTK window) are picked up by the watcher
//! and broadcast the same way.
//!
//! Safety (breaker #36): every bus write is clamped to the bus's hard ceiling (config `max_db`, #35);
//! a soft cap is lifted only by a per-connection unlock that expires after 60 s and dies with the
//! connection; a physical bus rises at most MAX_STEP_DB per command and MAX_RAISE_PER_WINDOW_DB per
//! RAISE_WINDOW_S across all clients. Pre-auth: small frames, a connection cap and a wall-clock Hello
//! deadline.
use super::backend::Backend;
use super::model;
use crate::config::{self, Config};
use std::collections::{BTreeMap, HashMap};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tungstenite::protocol::WebSocketConfig;
use tungstenite::Message;
use wavelink_proto::{self as p, safety, ClientMsg, ErrorCode, ServerMsg};

pub struct Opts {
    /// Address to listen on, or several separated by commas ("192.168.1.157,127.0.0.1").
    pub bind: String,
    pub port: u16,
    pub token: String,
    pub cfg_path: PathBuf,
    /// Shown to clients (hostname).
    pub name: String,
    pub caps: BTreeMap<String, f64>,
}

/// Messages queued per client before it counts as stalled and is dropped (it reconnects and resyncs).
const QUEUE: usize = 512;
/// From TCP accept to a valid Hello, wall clock, whatever else arrives (pings, a trickled handshake).
pub const HELLO_DEADLINE: Duration = Duration::from_secs(5);
/// A client that sends nothing (not even its ping) for this long is dropped.
const IDLE_TIMEOUT: Duration = Duration::from_secs(45);
const WATCH_EVERY: Duration = Duration::from_millis(250);
/// Largest WebSocket message/frame accepted. A Hello or a command is well under 1 KiB; 128 KiB still
/// lets an oversized (100 KB) token reach the auth check and get a proper `auth` refusal.
pub const MAX_MSG: usize = 128 * 1024;
/// Connections at once, and of those, not yet authenticated (in total and per peer IP; see admit_policy:
/// at the cap the oldest IDLE pending socket is closed instead of refusing the new one, R2-1).
pub const MAX_CONNS: usize = 16;
pub const MAX_PREAUTH: usize = 4;
pub const MAX_PREAUTH_PER_IP: usize = 2;

struct Client {
    id: u64,
    tx: SyncSender<Arc<str>>,
    meters: Arc<AtomicBool>,
    /// Hello.full: wants whole-config and graph snapshots (the full mixer window).
    full: bool,
    /// The last graph JSON queued for this client (full clients only).
    last_graph: Option<Arc<str>>,
}

struct Core {
    cfg: Config,
    state: p::State,
    clients: Vec<Client>,
    /// time-based raise limit per physical bus, shared by every client
    limits: HashMap<String, safety::RaiseLimiter>,
    /// last known gain per limiter key, kept after a bus is deleted so a later re-add can't restart from 'unknown'
    last_gain: HashMap<String, f64>,
    /// the Macs (and other hosts) that report their own output volume (#37)
    hosts: Vec<p::Host>,
    /// host id -> the connection that owns it (the newest registration wins)
    host_owner: HashMap<String, u64>,
}

/// Limiter key: the hardware target a bus plays to (so a new id on the same device shares its history), else the id.
fn limit_key(cfg: &Config, bus: &str) -> String {
    match cfg.buses.iter().find(|b| b.id == bus).map(|b| &b.kind) {
        Some(config::BusKind::Hardware { target }) => format!("target:{target}"),
        _ => bus.to_string(),
    }
}

struct Token {
    value: String,
    file: Option<PathBuf>,
    /// (inode, mtime) of the file when last read: a rotate renames a new file in
    mtime: Option<(u64, SystemTime)>,
}

struct Inner {
    opts: Opts,
    backend: Arc<dyn Backend>,
    core: Mutex<Core>,
    next_id: AtomicU64,
    stop: AtomicBool,
    conns: AtomicUsize,
    preauth: Mutex<Vec<Pending>>,
    next_pre: AtomicU64,
    /// bumped whenever the token in the file changes
    token_gen: AtomicU64,
    token: Mutex<Token>,
    clock: Instant,
}

/// A running server; dropping it stops the accept loops and the pumps (client threads end with their sockets).
pub struct Running {
    pub addr: SocketAddr,
    inner: Arc<Inner>,
    /// addresses from `bind` that could not be bound at start
    pub failed: Vec<String>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::SeqCst);
    }
}

impl Running {
    #[cfg(test)]
    pub fn clients(&self) -> usize {
        self.inner.lock().clients.len()
    }

    /// Listen on one more address (the "auto" bind adds LAN addresses that appear later).
    pub fn listen(&self, bind: &str, port: u16) -> std::io::Result<SocketAddr> {
        listen(&self.inner, bind, port)
    }

    /// Check Hello tokens against this file's current token (re-read when it changes: `remote-token --rotate`).
    pub fn token_file(&self, p: PathBuf) {
        self.inner.token.lock().unwrap_or_else(|e| e.into_inner()).file = Some(p);
    }
}

/// Decrements a counter when dropped.
struct Slot<'a>(&'a AtomicUsize);
impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
/// A pending socket is idle once this long has passed since its WebSocket handshake completed without a
/// Hello: a real client sends Hello right after the handshake, even on a slow link. A socket still in
/// its handshake is never idle (the 5 s Hello deadline bounds it).
pub const PREAUTH_IDLE: Duration = Duration::from_secs(1);

#[derive(Debug, PartialEq)]
pub enum Admit {
    Ok,
    /// close pending socket #i first, then decide again
    Evict(usize),
    Refuse,
}

/// Pre-auth admission (breaker #36 R2-1, R3-1, R3-2). `pending` = (peer IP, idle time) of the
/// unauthenticated sockets, idle time counted from the end of the handshake (zero while handshaking).
/// - An IP at MAX_PREAUTH_PER_IP (loopback exempt) makes room by losing its own oldest socket.
/// - At MAX_PREAUTH overall, the victim comes from the IP holding the most pending sockets (so an IP's
///   only socket is never taken while another IP holds 2 or more), preferring its idle sockets, then its
///   oldest. A newcomer is never refused while some other IP holds 2 or more.
/// - Only when the newcomer's own IP is the biggest holder and none of its sockets are idle (a local
///   loopback flood) does it wait for room: up to 2 x MAX_PREAUTH pending, then it is refused.
pub fn admit_policy(pending: &[(std::net::IpAddr, Duration)], ip: std::net::IpAddr) -> Admit {
    let count = |x: std::net::IpAddr| pending.iter().filter(|(i, _)| *i == x).count();
    // the oldest socket among `pool`, idle ones first
    let pick = |pool: &dyn Fn(std::net::IpAddr) -> bool| {
        pending
            .iter()
            .enumerate()
            .filter(|(_, (i, _))| pool(*i))
            .max_by_key(|(_, (_, age))| (*age >= PREAUTH_IDLE, *age))
            .map(|(k, _)| k)
    };
    if !ip.is_loopback() && count(ip) >= MAX_PREAUTH_PER_IP {
        return pick(&|i| i == ip).map_or(Admit::Refuse, Admit::Evict);
    }
    if pending.len() < MAX_PREAUTH {
        return Admit::Ok;
    }
    let most = pending.iter().map(|(i, _)| count(*i)).max().unwrap_or(0);
    // the biggest holders; among equals the one with the oldest socket
    let big: Vec<std::net::IpAddr> = pending.iter().map(|(i, _)| *i).filter(|i| count(*i) == most).collect();
    let others: Vec<std::net::IpAddr> = big.iter().copied().filter(|i| *i != ip).collect();
    if !others.is_empty() && (most >= 2 || pending.iter().any(|(i, age)| others.contains(i) && *age >= PREAUTH_IDLE)) {
        if let Some(k) = pick(&|i| others.contains(&i)) {
            return Admit::Evict(k);
        }
    }
    // the newcomer's own IP is the biggest holder (or everyone holds one fresh socket): only idle ones go
    if let Some(k) = pending.iter().enumerate().filter(|(_, (_, age))| *age >= PREAUTH_IDLE).max_by_key(|(_, (_, age))| *age).map(|(k, _)| k) {
        if count(pending[k].0) >= count(ip).max(1) {
            return Admit::Evict(k);
        }
    }
    if pending.len() >= 2 * MAX_PREAUTH {
        return Admit::Refuse;
    }
    Admit::Ok
}

/// An unauthenticated connection: who, since when, and a handle to close it if it must make room.
struct Pending {
    id: u64,
    ip: std::net::IpAddr,
    /// when its WebSocket handshake completed (None while still handshaking)
    since: Option<Instant>,
    sock: TcpStream,
}

/// Removes its pending entry when dropped (authenticated, refused, or gone).
struct PreGuard<'a> {
    inner: &'a Inner,
    id: u64,
}
impl PreGuard<'_> {
    /// The handshake is done: idleness counts from now.
    fn handshaken(&self) {
        if let Some(p) = self.inner.preauth.lock().unwrap_or_else(|e| e.into_inner()).iter_mut().find(|p| p.id == self.id) {
            p.since = Some(Instant::now());
        }
    }
}
impl Drop for PreGuard<'_> {
    fn drop(&mut self) {
        self.inner.preauth.lock().unwrap_or_else(|e| e.into_inner()).retain(|p| p.id != self.id);
    }
}

fn take_slot(c: &AtomicUsize, max: usize) -> Option<Slot<'_>> {
    let mut n = c.load(Ordering::SeqCst);
    loop {
        if n >= max {
            return None;
        }
        match c.compare_exchange(n, n + 1, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return Some(Slot(c)),
            Err(x) => n = x,
        }
    }
}

fn listen(inner: &Arc<Inner>, bind: &str, port: u16) -> std::io::Result<SocketAddr> {
    let listener = TcpListener::bind((bind, port))?;
    listener.set_nonblocking(true)?;
    let addr = listener.local_addr()?;
    let inner = inner.clone();
    std::thread::Builder::new().name("remote-accept".into()).spawn(move || {
        while !inner.stop.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((s, peer)) => {
                    let inner = inner.clone();
                    let _ = std::thread::Builder::new().name("remote-client".into()).spawn(move || {
                        let Some(_total) = take_slot(&inner.conns, MAX_CONNS) else {
                            eprintln!("wavelink remote: {peer}: refused, {MAX_CONNS} connections already");
                            return; // dropping the stream closes it
                        };
                        match inner.conn(s, peer) {
                            Err(e) if e.contains("without closing handshake") => {}
                            Err(e) => eprintln!("wavelink remote: {peer}: {e}"),
                            Ok(()) => {}
                        }
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(25)),
                Err(e) => {
                    eprintln!("wavelink remote: accept: {e}");
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        }
    })?;
    Ok(addr)
}

pub fn serve(opts: Opts, backend: Arc<dyn Backend>) -> std::io::Result<Running> {
    let binds: Vec<String> = opts.bind.split(',').map(|b| b.trim().to_string()).filter(|b| !b.is_empty()).collect();
    if binds.is_empty() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "no address to listen on"));
    }
    let cfg = Config::load_from(&opts.cfg_path);
    let state = model::to_state(&cfg, &opts.name, 1, &opts.caps);
    let port = opts.port;
    let token = Token { value: opts.token.clone(), file: None, mtime: None };
    let inner = Arc::new(Inner {
        opts,
        backend,
        core: Mutex::new(Core { cfg, state, clients: vec![], limits: HashMap::new(), last_gain: HashMap::new(), hosts: vec![], host_owner: HashMap::new() }),
        next_id: AtomicU64::new(1),
        stop: AtomicBool::new(false),
        conns: AtomicUsize::new(0),
        preauth: Mutex::new(vec![]),
        next_pre: AtomicU64::new(1),
        token_gen: AtomicU64::new(0),
        token: Mutex::new(token),
        clock: Instant::now(),
    });
    // every address is tried; serve() fails only if none works (127.0.0.1 still comes up when a LAN
    // address is taken). The caller retries the failed ones later (Running::failed).
    let (mut addr, mut failed, mut last_err) = (None, vec![], None);
    for b in &binds {
        match listen(&inner, b, port) {
            Ok(a) => {
                addr.get_or_insert(a);
            }
            Err(e) => {
                eprintln!("wavelink remote: can't listen on {b}:{port}: {e}");
                failed.push(b.clone());
                last_err = Some(e);
            }
        }
    }
    let Some(addr) = addr else { return Err(last_err.unwrap()) };
    let run = Running { addr, inner: inner.clone(), failed };
    {
        let inner = inner.clone();
        std::thread::Builder::new().name("remote-watch".into()).spawn(move || {
            while !inner.stop.load(Ordering::SeqCst) {
                {
                    let mut core = inner.lock();
                    inner.sync_disk(&mut core);
                }
                inner.refresh_token(); // a rotate drops the sessions on the old token (R2-2)
                std::thread::sleep(WATCH_EVERY);
            }
        })?;
    }
    {
        let inner = inner.clone();
        std::thread::Builder::new().name("remote-meters".into()).spawn(move || {
            let period = Duration::from_millis(1000 / p::METER_FPS as u64);
            let mut was_on = false;
            while !inner.stop.load(Ordering::SeqCst) {
                let t0 = Instant::now();
                let (want, cfg) = {
                    let core = inner.lock();
                    (core.clients.iter().any(|c| c.meters.load(Ordering::Relaxed)), core.cfg.clone())
                };
                if want || was_on {
                    let m = inner.backend.meters(&cfg, want);
                    if let (true, Some(m)) = (want, m) {
                        let msg: Arc<str> = ServerMsg::Meters { meters: m }.to_json().into();
                        let core = inner.lock();
                        for c in core.clients.iter().filter(|c| c.meters.load(Ordering::Relaxed)) {
                            let _ = c.tx.try_send(msg.clone()); // a full queue just skips a frame
                        }
                    }
                    was_on = want;
                }
                std::thread::sleep(period.saturating_sub(t0.elapsed()));
            }
            if was_on {
                let cfg = inner.lock().cfg.clone();
                inner.backend.meters(&cfg, false);
            }
        })?;
    }
    {
        let inner = inner.clone();
        std::thread::Builder::new().name("remote-graph".into()).spawn(move || {
            while !inner.stop.load(Ordering::SeqCst) {
                let any_full = inner.lock().clients.iter().any(|c| c.full);
                if any_full {
                    if let Some(g) = inner.backend.graph_wire() {
                        if let Ok(v) = serde_json::to_value(&g) {
                            let json: Arc<str> = ServerMsg::Graph { graph: v }.to_json().into();
                            let mut core = inner.lock();
                            core.clients.retain_mut(|c| {
                                if !c.full || c.last_graph.as_ref() == Some(&json) {
                                    return true;
                                }
                                match c.tx.try_send(json.clone()) {
                                    Ok(()) => {
                                        c.last_graph = Some(json.clone());
                                        true
                                    }
                                    Err(TrySendError::Full(_)) => {
                                        eprintln!("wavelink remote: client {} stalled, dropping it", c.id);
                                        false
                                    }
                                    Err(TrySendError::Disconnected(_)) => false,
                                }
                            });
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        })?;
    }
    Ok(run)
}

fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn io_timeout(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(io) if matches!(io.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut))
}

/// A client-chosen name, safe for one log line: printable, no control characters, at most 64 chars.
/// A host id is what `console::sanitize_host_id` would return for it: non-empty, [a-z0-9] runs joined by single '-'.
fn valid_host_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') && !id.starts_with('-') && !id.ends_with('-') && !id.contains("--")
}

pub fn sanitize_name(s: &str) -> String {
    let t: String = s.chars().filter(|c| !c.is_control()).take(64).collect();
    if t.trim().is_empty() {
        "unnamed".into()
    } else {
        t
    }
}

fn ws_config() -> WebSocketConfig {
    WebSocketConfig::default().max_message_size(Some(MAX_MSG)).max_frame_size(Some(MAX_MSG))
}

impl Inner {
    fn lock(&self) -> std::sync::MutexGuard<'_, Core> {
        self.core.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn now_s(&self) -> f64 {
        self.clock.elapsed().as_secs_f64()
    }

    /// Re-read the token file if it changed; a new token bumps `token_gen`, which ends every session
    /// authenticated under the old one.
    fn refresh_token(&self) {
        let mut t = self.token.lock().unwrap_or_else(|e| e.into_inner());
        let Some(f) = t.file.clone() else { return };
        let m = std::fs::metadata(&f).and_then(|m| Ok((std::os::unix::fs::MetadataExt::ino(&m), m.modified()?))).ok();
        if m == t.mtime {
            return;
        }
        if let Some(v) = super::settings::read_token(&f) {
            if v != t.value {
                eprintln!("wavelink remote: token changed in {}; closing sessions on the old token", f.display());
                t.value = v;
                self.token_gen.fetch_add(1, Ordering::SeqCst);
            }
        }
        t.mtime = m;
    }

    /// Ok(generation) when `got` is the current token.
    fn token_ok(&self, got: &str) -> Option<u64> {
        self.refresh_token();
        let t = self.token.lock().unwrap_or_else(|e| e.into_inner());
        ct_eq(got.trim(), &t.value).then(|| self.token_gen.load(Ordering::SeqCst))
    }

    /// Register an unauthenticated connection, closing the oldest idle pending one(s) to make room.
    fn admit(&self, sock: &TcpStream, peer: SocketAddr) -> Result<PreGuard<'_>, String> {
        let handle = sock.try_clone().map_err(|e| e.to_string())?;
        let id = self.next_pre.fetch_add(1, Ordering::SeqCst);
        let mut v = self.preauth.lock().unwrap_or_else(|e| e.into_inner());
        let ip = peer.ip();
        loop {
            let now = Instant::now();
            let view: Vec<(std::net::IpAddr, Duration)> = v.iter().map(|p| (p.ip, p.since.map_or(Duration::ZERO, |t| now.duration_since(t)))).collect();
            match admit_policy(&view, ip) {
                Admit::Ok => break,
                Admit::Refuse => return Err(format!("refused: too many unauthenticated connections from {ip}")),
                Admit::Evict(i) => {
                    let p = v.remove(i);
                    eprintln!("wavelink remote: {}: idle pre-auth connection closed to make room for a new one", p.ip);
                    let _ = p.sock.shutdown(std::net::Shutdown::Both);
                }
            }
        }
        v.push(Pending { id, ip, since: None, sock: handle });
        Ok(PreGuard { inner: self, id })
    }

    /// Queue `msg` for every client; a client whose queue is full or gone is dropped.
    fn broadcast(core: &mut Core, msg: &ServerMsg) {
        let s: Arc<str> = msg.to_json().into();
        core.clients.retain(|c| match c.tx.try_send(s.clone()) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                eprintln!("wavelink remote: client {} stalled, dropping it", c.id);
                false
            }
            Err(TrySendError::Disconnected(_)) => false,
        });
    }

    /// Queue `msg` for the `full` clients only.
    fn broadcast_full(core: &mut Core, msg: &ServerMsg) {
        let s: Arc<str> = msg.to_json().into();
        core.clients.retain(|c| {
            if !c.full {
                return true;
            }
            match c.tx.try_send(s.clone()) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) => {
                    eprintln!("wavelink remote: client {} stalled, dropping it", c.id);
                    false
                }
                Err(TrySendError::Disconnected(_)) => false,
            }
        });
    }

    /// Make `cfg` current and tell everyone what changed (deltas, or a full state on structure changes).
    fn publish(&self, core: &mut Core, cfg: Config) {
        let mut new = model::to_state(&cfg, &self.opts.name, core.state.rev + 1, &self.opts.caps);
        new.hosts = core.hosts.clone();
        core.cfg = cfg;
        self.publish_state(core, new);
        if let Ok(config) = serde_json::to_value(&core.cfg) {
            Self::broadcast_full(core, &ServerMsg::Config { config });
        }
    }

    /// Edit the host list (engine memory only, never config.json) and tell everyone what changed.
    fn edit_hosts(&self, core: &mut Core, f: impl FnOnce(&mut Vec<p::Host>)) {
        f(&mut core.hosts);
        let mut new = core.state.clone();
        new.rev += 1;
        new.hosts = core.hosts.clone();
        self.publish_state(core, new);
    }

    /// Diff `new` against the current state and broadcast it (deltas, or a full state on structure changes).
    fn publish_state(&self, core: &mut Core, new: p::State) {
        match core.state.diff(&new) {
            Some(d) if d.is_empty() => {}
            Some(d) => {
                core.state = new;
                Self::broadcast(core, &ServerMsg::Delta { rev: core.state.rev, changes: d });
            }
            None => {
                core.state = new;
                let st = ServerMsg::State { state: core.state.clone() };
                Self::broadcast(core, &st);
            }
        }
    }

    /// Pick up edits another writer saved (the GTK window, the preset CLI).
    fn sync_disk(&self, core: &mut Core) {
        if let Some(d) = Config::peek(&self.opts.cfg_path) {
            if d != core.cfg {
                self.publish(core, d);
            }
        }
    }

    /// `unlocked(bus)` = the sending connection holds a live unlock for that bus.
    fn command(&self, id: u64, cmd: &p::Command, unlocked: &dyn Fn(&str) -> bool) -> ServerMsg {
        let nack = |e: String| ServerMsg::Ack { id, ok: false, error: Some(e), note: None };
        let mut core = self.lock();
        if let p::Command::SetHostVolume { .. } | p::Command::SetHostMute { .. } = cmd {
            // a host's output: engine memory only (never config.json, never the PipeWire backend) (#37)
            let checked = match safety::check(cmd, &core.state, unlocked) {
                Ok(c) => c,
                Err(e) => return nack(e.to_string()),
            };
            let note = checked.note.clone();
            self.edit_hosts(&mut core, |hosts| {
                for c in &checked.cmds {
                    match c {
                        p::Command::SetHostVolume { host, volume } => {
                            if let Some(h) = hosts.iter_mut().find(|h| &h.id == host) {
                                h.volume = (*volume).min(100);
                            }
                        }
                        p::Command::SetHostMute { host, mute } => {
                            if let Some(h) = hosts.iter_mut().find(|h| &h.id == host) {
                                h.mute = *mute;
                            }
                        }
                        _ => {}
                    }
                }
            });
            return ServerMsg::Ack { id, ok: true, error: None, note };
        }
        let flock = config::lock_config(&self.opts.cfg_path);
        self.sync_disk(&mut core);
        // a destination that is AirPlay only by its node props gets its ceiling saved first (#35's rule),
        // so the config, the state and the raise limiter show the capped gain the speaker really plays
        if let p::Command::SetBusGain { bus, .. } | p::Command::SetBusMute { bus, .. } = cmd {
            let mut cfg = core.cfg.clone();
            if let Some(b) = cfg.buses.iter_mut().find(|b| &b.id == bus) {
                let raop = match &b.kind {
                    config::BusKind::Hardware { target } => b.max_db.is_none() && self.backend.node_is_raop(target),
                    _ => false,
                };
                if raop && b.persist_raop_ceiling(true) {
                    b.gain_db = b.clamp_db(b.gain_db);
                    eprintln!("wavelink remote: bus {bus}: AirPlay destination (node props), ceiling {} dB saved", b.ceiling_db());
                    if cfg.save_to(&self.opts.cfg_path) {
                        self.publish(&mut core, cfg);
                    }
                }
            }
        }
        let mut checked = match safety::check(cmd, &core.state, unlocked) {
            Ok(c) => c,
            Err(e) => return nack(e.to_string()),
        };
        // the time-based raise limit (B36-1/2): shared by all clients, decreases never limited
        let now = self.now_s();
        let mut raised = vec![];
        for c in checked.cmds.iter_mut() {
            if let p::Command::SetBusGain { bus, db, .. } = c {
                let Some(b) = core.state.bus(bus) else { continue };
                if !b.physical() {
                    continue;
                }
                let cur = b.gain_db;
                if *db > cur {
                    let key = limit_key(&core.cfg, bus);
                    let allowed = core.limits.entry(key).or_default().allowed(now, cur);
                    if *db > allowed {
                        *db = allowed.max(cur);
                        checked.note = Some(format!("raised at most {} dB per {} s", safety::MAX_RAISE_PER_WINDOW_DB, safety::RAISE_WINDOW_S));
                    }
                }
                raised.push((bus.clone(), cur, *db));
            }
        }
        let mut cfg = core.cfg.clone();
        for c in &checked.cmds {
            if !model::apply(&mut cfg, c) {
                return nack("target vanished".into());
            }
        }
        if cfg != core.cfg && !cfg.save_to(&self.opts.cfg_path) {
            return nack("could not save config.json".into());
        }
        drop(flock);
        for (bus, before, _) in raised {
            // what was really written (model::apply clamps to the ceiling too)
            let after = cfg.buses.iter().find(|b| b.id == bus).map(|b| b.gain_db).unwrap_or(before);
            let key = limit_key(&cfg, &bus);
            core.last_gain.insert(key.clone(), after);
            core.limits.entry(key).or_default().record(now, before, after);
        }
        for c in &checked.cmds {
            self.backend.apply(&cfg, c);
        }
        self.publish(&mut core, cfg);
        ServerMsg::Ack { id, ok: true, error: None, note: checked.note }
    }

    /// A full window's save: 3-way merge `mine` against config.json (`base` = common ancestor), with every
    /// bus-gain raise capped exactly as `SetBusGain` from this connection would be (I1).
    fn save_config(&self, id: u64, base: serde_json::Value, mine: serde_json::Value, unlocked: &dyn Fn(&str) -> bool) -> ServerMsg {
        let nack = |e: String| ServerMsg::Ack { id, ok: false, error: Some(e), note: None };
        let (mut base, mut mine): (Config, Config) = match (serde_json::from_value(base), serde_json::from_value(mine)) {
            (Ok(b), Ok(m)) => (b, m),
            (Err(e), _) | (_, Err(e)) => return nack(format!("bad config: {e}")),
        };
        let mut core = self.lock();
        self.sync_disk(&mut core);
        let now = self.now_s();
        let mut notes: Vec<String> = vec![];
        let mut raised = vec![];
        // remember every hub bus's gain by target before this save can delete it
        let hub_cfg = core.cfg.clone();
        for b in hub_cfg.buses.iter().filter(|b| matches!(b.kind, config::BusKind::Hardware { .. })) {
            core.last_gain.insert(limit_key(&hub_cfg, &b.id), b.gain_db);
        }
        // a remote save never moves a bus's ceiling or kind: the hub's values win (their lift would defeat I1)
        for b in mine.buses.iter_mut() {
            if let Some(h) = core.cfg.buses.iter().find(|h| h.id == b.id) {
                b.max_db = h.max_db;
                b.kind = h.kind.clone();
            } else if let config::BusKind::Hardware { target } = &b.kind {
                // a new raop bus never brings its own lift: its ceiling is at most what a local bus would get
                if b.is_raop(self.backend.node_is_raop(target)) {
                    b.max_db = Some(b.max_db.filter(|m| m.is_finite()).map_or(config::RAOP_MAX_DB, |m| m.min(config::RAOP_MAX_DB)));
                }
            }
        }
        // a bus the hub does not know yet has no live state to check against: it is only capped by its own ceiling
        let want: Vec<(String, f64)> = mine
            .buses
            .iter()
            .filter(|b| base.buses.iter().find(|o| o.id == b.id).map_or(true, |o| b.gain_db > o.gain_db))
            .map(|b| (b.id.clone(), b.gain_db))
            .collect();
        for (bus, db) in want {
            if core.state.bus(&bus).is_none() {
                // not live (new id, offline target, or deleted and re-added): the rule is per target,
                // the same one SetBusGain applies (raise window + 6 dB step), never skipped
                let key = match mine.buses.iter().find(|b| b.id == bus).map(|b| &b.kind) {
                    Some(config::BusKind::Hardware { target }) => format!("target:{target}"),
                    _ => bus.clone(),
                };
                let known = hub_cfg.buses.iter().find(|b| b.id == bus).map(|b| b.gain_db).or_else(|| core.last_gain.get(&key).copied());
                let Some(cur) = known else { continue }; // a target the hub has never seen: ceiling-clamped only
                let allowed = core.limits.entry(key).or_default().allowed(now, cur).min(cur + safety::MAX_STEP_DB);
                if db > allowed {
                    let capped = allowed.max(cur);
                    if let Some(b) = mine.buses.iter_mut().find(|b| b.id == bus) {
                        b.gain_db = capped;
                    }
                    notes.push(format!("raised at most {} dB per {} s", safety::MAX_RAISE_PER_WINDOW_DB, safety::RAISE_WINDOW_S));
                }
                continue;
            }
            let cmd = p::Command::SetBusGain { bus: bus.clone(), db, unlock: false };
            let mut checked = match safety::check(&cmd, &core.state, unlocked) {
                Ok(c) => c,
                Err(e) => return nack(e.to_string()),
            };
            for c in checked.cmds.iter_mut() {
                if let p::Command::SetBusGain { bus, db, .. } = c {
                    if let Some(b) = core.state.bus(bus).filter(|b| b.physical()) {
                        let cur = b.gain_db;
                        if *db > cur {
                            let key = limit_key(&hub_cfg, bus);
                            let allowed = core.limits.entry(key).or_default().allowed(now, cur);
                            if *db > allowed {
                                *db = allowed.max(cur);
                                checked.note = Some(format!("raised at most {} dB per {} s", safety::MAX_RAISE_PER_WINDOW_DB, safety::RAISE_WINDOW_S));
                            }
                        }
                        raised.push((bus.clone(), cur));
                    }
                }
            }
            for c in &checked.cmds {
                model::apply(&mut mine, c); // writes the capped (ceiling-clamped) gain back into `mine`
            }
            notes.extend(checked.note);
        }
        // every bus in the candidate, new ones included, ends at or under its own ceiling (raop ceiling by props)
        for b in mine.buses.iter_mut() {
            if let config::BusKind::Hardware { target } = &b.kind {
                let raop = self.backend.node_is_raop(target);
                b.persist_raop_ceiling(raop);
            }
            b.gain_db = b.clamp_db(b.gain_db);
        }
        match mine.save_merged_with(&mut base, &self.opts.cfg_path, &|_, _, _| {}) {
            Err(e) => nack(e),
            Ok(_) => {
                let cfg = Config::load_from(&self.opts.cfg_path);
                for (bus, before) in raised {
                    let after = cfg.buses.iter().find(|b| b.id == bus).map_or(before, |b| b.gain_db);
                    let key = limit_key(&hub_cfg, &bus);
                    core.last_gain.insert(key.clone(), after);
                    core.limits.entry(key).or_default().record(now, before, after);
                }
                self.publish(&mut core, cfg);
                ServerMsg::Ack { id, ok: true, error: None, note: if notes.is_empty() { None } else { Some(notes.join("; ")) } }
            }
        }
    }

    /// A full window's live PipeWire write. A bus node's volume/mute goes through `command` (the caps); the rest is raw.
    fn pw(&self, id: u64, cmd: p::PwCmd, unlocked: &dyn Fn(&str) -> bool) -> ServerMsg {
        let node = match &cmd {
            p::PwCmd::SetVolume { id, .. } | p::PwCmd::SetVolumeMute { id, .. } | p::PwCmd::SetMute { id, .. } => Some(*id),
            _ => None,
        };
        let bus = node.and_then(|n| {
            let cfg = self.lock().cfg.clone();
            self.backend.bus_for_node(&cfg, n)
        });
        let Some(bus) = bus else {
            // fail closed: a volume/mute write is raw only to a node the hub can see and that is not a bus or raop node
            if let Some(n) = node {
                let find = |w: Option<crate::pw::GraphWire>| w.and_then(|w| w.nodes.into_iter().find(|x| x.id == n)).map(|x| x.name);
                let mut name = find(self.backend.graph_wire());
                if name.is_none() {
                    self.backend.refresh_graph(); // a node under 2 s old is not in the cache yet
                    name = find(self.backend.graph_wire());
                }
                let refuse = |e: &str| ServerMsg::Ack { id, ok: false, error: Some(e.into()), note: None };
                match name {
                    None => return refuse("hub can't see that PipeWire node"),
                    Some(nm) if nm.starts_with("bus_") || config::is_raop_name(&nm) || self.backend.node_is_raop(&nm) => {
                        return refuse("that node is a bus or speaker the hub does not map: refused");
                    }
                    Some(_) => {}
                }
            }
            self.backend.pw(&cmd);
            return ServerMsg::Ack { id, ok: true, error: None, note: None };
        };
        let gain = |v: f64| p::Command::SetBusGain { bus: bus.clone(), db: config::vol_to_db(v), unlock: false };
        let mute = |m: bool| p::Command::SetBusMute { bus: bus.clone(), mute: m };
        match cmd {
            p::PwCmd::SetVolume { v, .. } => self.command(id, &gain(v), unlocked),
            p::PwCmd::SetMute { mute: m, .. } => self.command(id, &mute(m), unlocked),
            p::PwCmd::SetVolumeMute { v, mute: m, .. } => {
                let g = self.command(id, &gain(v), unlocked);
                match self.command(id, &mute(m), unlocked) {
                    ServerMsg::Ack { ok: false, error, .. } => match g {
                        ServerMsg::Ack { error: e0, note, .. } => {
                            let err = [e0, error].into_iter().flatten().collect::<Vec<_>>().join("; ");
                            ServerMsg::Ack { id, ok: false, error: Some(err), note }
                        }
                        other => other,
                    },
                    _ => g,
                }
            }
            _ => unreachable!(),
        }
    }

    fn conn(self: &Arc<Self>, s: TcpStream, peer: SocketAddr) -> Result<(), String> {
        let deadline = Instant::now() + HELLO_DEADLINE;
        let pre = self.admit(&s, peer)?;
        let left = || deadline.saturating_duration_since(Instant::now());
        s.set_nonblocking(false).map_err(|e| e.to_string())?;
        s.set_nodelay(true).ok();
        s.set_write_timeout(Some(Duration::from_secs(5))).map_err(|e| e.to_string())?;
        // short read timeouts so the wall-clock deadline is checked while the peer trickles bytes
        s.set_read_timeout(Some(Duration::from_millis(200))).map_err(|e| e.to_string())?;
        let mut hs = tungstenite::accept_with_config(s, Some(ws_config()));
        let mut ws = loop {
            match hs {
                Ok(ws) => break ws,
                Err(tungstenite::HandshakeError::Interrupted(mid)) => {
                    if left().is_zero() {
                        return Err("no handshake before the deadline".into());
                    }
                    hs = mid.handshake();
                }
                Err(tungstenite::HandshakeError::Failure(e)) => return Err(format!("handshake: {e}")),
            }
        };
        pre.handshaken();
        let send = |ws: &mut tungstenite::WebSocket<TcpStream>, m: &ServerMsg| ws.send(Message::text(m.to_json())).map_err(|e| e.to_string());
        let refuse = |ws: &mut tungstenite::WebSocket<TcpStream>, code: ErrorCode, msg: &str| {
            let _ = send(ws, &ServerMsg::Error { code, message: msg.into() });
            let _ = ws.close(None);
            let _ = ws.flush();
            Err(msg.to_string())
        };
        // first frame must be Hello with the right protocol and token, within the deadline
        let hello = loop {
            if left().is_zero() {
                return refuse(&mut ws, ErrorCode::BadRequest, "no hello within 5 s");
            }
            match ws.read() {
                Ok(Message::Text(t)) => break ClientMsg::from_json(t.as_str()),
                Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {
                    let _ = ws.flush(); // the auto pong
                    continue;
                }
                Ok(_) => return refuse(&mut ws, ErrorCode::BadRequest, "expected hello"),
                Err(e) if io_timeout(&e) => continue,
                Err(e) => return Err(format!("no hello: {e}")),
            }
        };
        let hello = match hello {
            Ok(ClientMsg::Hello(h)) => h,
            _ => return refuse(&mut ws, ErrorCode::BadRequest, "expected hello"),
        };
        if hello.protocol != p::PROTOCOL_VERSION {
            let m = format!("protocol {} not supported (server speaks {})", hello.protocol, p::PROTOCOL_VERSION);
            return refuse(&mut ws, ErrorCode::Protocol, &m);
        }
        let Some(token_gen) = self.token_ok(&hello.token) else {
            std::thread::sleep(Duration::from_millis(400)); // slows guessing
            return refuse(&mut ws, ErrorCode::Auth, "bad token");
        };
        if let Some(h) = &hello.host {
            if !valid_host_id(&h.id) {
                return refuse(&mut ws, ErrorCode::BadRequest, "bad host id");
            }
        }
        drop(pre); // authenticated: no longer counts against the pre-auth cap
        send(&mut ws, &ServerMsg::Welcome { protocol: p::PROTOCOL_VERSION, server: self.opts.name.clone(), version: env!("CARGO_PKG_VERSION").into() })?;
        let (tx, rx) = mpsc::sync_channel::<Arc<str>>(QUEUE);
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let meters = Arc::new(AtomicBool::new(hello.meters));
        {
            // snapshot first, then join the broadcast list, under one lock: no delta can slip between
            let mut core = self.lock();
            self.sync_disk(&mut core);
            if let Some(h) = &hello.host {
                self.register_host(&mut core, id, h);
            }
            let _ = tx.try_send(ServerMsg::State { state: core.state.clone() }.to_json().into());
            let mut last_graph = None;
            if hello.full {
                if let Ok(config) = serde_json::to_value(&core.cfg) {
                    let _ = tx.try_send(ServerMsg::Config { config }.to_json().into());
                }
                if let Some(Ok(graph)) = self.backend.graph_wire().map(|g| serde_json::to_value(&g)) {
                    let j: Arc<str> = ServerMsg::Graph { graph }.to_json().into();
                    let _ = tx.try_send(j.clone());
                    last_graph = Some(j);
                }
            }
            core.clients.push(Client { id, tx, meters: meters.clone(), full: hello.full, last_graph });
        }
        let who = format!("client {id} ({}, {peer})", sanitize_name(&hello.client));
        eprintln!("wavelink remote: {who} connected");
        let host = hello.host.as_ref().map(|h| (id, h.id.clone()));
        let r = self.client_loop(&mut ws, &rx, &meters, &who, token_gen, host.as_ref());
        self.lock().clients.retain(|c| c.id != id);
        if let Some((_, hid)) = &host {
            self.host_left(id, hid);
        }
        let _ = ws.close(None);
        let _ = ws.flush();
        eprintln!("wavelink remote: {who} left");
        r
    }

    fn client_loop(&self, ws: &mut tungstenite::WebSocket<TcpStream>, rx: &mpsc::Receiver<Arc<str>>, meters: &AtomicBool, who: &str, token_gen: u64, host: Option<&(u64, String)>) -> Result<(), String> {
        ws.get_ref().set_read_timeout(Some(Duration::from_millis(15))).map_err(|e| e.to_string())?;
        let mut heard = Instant::now();
        // this connection's unlocks: bus -> expiry. Gone with the connection.
        let mut unlocks: HashMap<String, Instant> = HashMap::new();
        let result = loop {
            if self.stop.load(Ordering::SeqCst) {
                break Ok(());
            }
            if self.token_gen.load(Ordering::SeqCst) != token_gen {
                eprintln!("wavelink remote: {who}: token rotated, closing the session");
                let _ = ws.write(Message::text(ServerMsg::Error { code: ErrorCode::Auth, message: "token rotated".into() }.to_json()));
                break Err("token rotated".into());
            }
            let now = Instant::now();
            let expired: Vec<String> = unlocks.iter().filter(|(_, t)| **t <= now).map(|(b, _)| b.clone()).collect();
            for b in expired {
                unlocks.remove(&b);
                eprintln!("wavelink remote: {who}: unlock of bus {b} expired");
                if let Err(e) = ws.write(Message::text(ServerMsg::Unlock { bus: b, seconds: 0 }.to_json())) {
                    return Err(e.to_string());
                }
            }
            match ws.read() {
                Ok(Message::Text(t)) => {
                    heard = Instant::now();
                    let reply = match ClientMsg::from_json(t.as_str()) {
                        Ok(ClientMsg::Command { .. }) if self.token_gen.load(Ordering::SeqCst) != token_gen => {
                            break Err("token rotated".into());
                        }
                        Ok(ClientMsg::Command { id, cmd }) => {
                            let live = |b: &str| unlocks.get(b).is_some_and(|t| *t > Instant::now());
                            Some(self.command(id, &cmd, &live))
                        }
                        Ok(ClientMsg::SaveConfig { .. } | ClientMsg::Pw { .. }) if self.token_gen.load(Ordering::SeqCst) != token_gen => {
                            break Err("token rotated".into());
                        }
                        Ok(ClientMsg::SaveConfig { id, base, mine }) => {
                            let live = |b: &str| unlocks.get(b).is_some_and(|t| *t > Instant::now());
                            Some(self.save_config(id, base, mine, &live))
                        }
                        Ok(ClientMsg::Pw { id, cmd }) => {
                            let live = |b: &str| unlocks.get(b).is_some_and(|t| *t > Instant::now());
                            Some(self.pw(id, cmd, &live))
                        }
                        Ok(ClientMsg::Subscribe { meters: on }) => {
                            meters.store(on, Ordering::Relaxed);
                            None
                        }
                        Ok(ClientMsg::Unlock { bus, on }) => Some(self.unlock(&mut unlocks, &bus, on, who)),
                        Ok(ClientMsg::Ping { t }) => Some(ServerMsg::Pong { t }),
                        Ok(ClientMsg::Hello(_)) => Some(ServerMsg::Error { code: ErrorCode::BadRequest, message: "already greeted".into() }),
                        Ok(ClientMsg::HostState { volume, mute }) => self.host_state(host, volume, mute),
                        Err(e) => Some(ServerMsg::Error { code: ErrorCode::BadRequest, message: format!("bad message: {e}") }),
                    };
                    if let Some(r) = reply {
                        ws.write(Message::text(r.to_json())).map_err(|e| e.to_string())?;
                    }
                }
                Ok(Message::Close(_)) => break Ok(()),
                Ok(_) => heard = Instant::now(), // ping/pong/binary: alive
                Err(e) if io_timeout(&e) => {}
                Err(tungstenite::Error::ConnectionClosed) | Err(tungstenite::Error::AlreadyClosed) => break Ok(()),
                Err(e) => break Err(e.to_string()),
            }
            loop {
                match rx.try_recv() {
                    Ok(m) => ws.write(Message::text(m.to_string())).map_err(|e| e.to_string())?,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return Err("dropped (stalled)".into()),
                }
            }
            match ws.flush() {
                Ok(()) => {}
                Err(e) if io_timeout(&e) => {}
                Err(e) => break Err(e.to_string()),
            }
            if heard.elapsed() > IDLE_TIMEOUT {
                break Err("idle timeout".into());
            }
        };
        for b in unlocks.keys() {
            eprintln!("wavelink remote: {who}: unlock of bus {b} cleared (disconnect)");
        }
        result
    }

    /// A host connection registers (or re-registers) its output; the newest registration owns the host.
    fn register_host(&self, core: &mut Core, conn: u64, h: &p::HostHello) {
        let (id, label, volume, mute) = (h.id.clone(), h.label.clone(), h.volume.min(100), h.mute);
        self.edit_hosts(core, |hosts| match hosts.iter_mut().find(|x| x.id == id) {
            Some(x) => {
                x.label = label;
                x.volume = volume;
                x.mute = mute;
                x.online = true;
            }
            None => hosts.push(p::Host { id: id.clone(), label, volume, mute, online: true }),
        });
        core.host_owner.insert(h.id.clone(), conn);
        eprintln!("wavelink remote: host {} ({}) online", h.id, sanitize_name(&h.label));
    }

    /// A host connection closed: its host goes OFFLINE, unless a newer connection has taken it over.
    fn host_left(&self, conn: u64, hid: &str) {
        let mut core = self.lock();
        if core.host_owner.get(hid) != Some(&conn) {
            return;
        }
        core.host_owner.remove(hid);
        self.edit_hosts(&mut core, |hosts| {
            if let Some(h) = hosts.iter_mut().find(|h| h.id == hid) {
                h.online = false;
            }
        });
        eprintln!("wavelink remote: host {hid} offline");
    }

    /// A host reports the volume/mute it saw itself (volume keys). Overrides the engine's value and tells everyone.
    fn host_state(&self, host: Option<&(u64, String)>, volume: u8, mute: bool) -> Option<ServerMsg> {
        let Some((conn, hid)) = host else {
            return Some(ServerMsg::Error { code: ErrorCode::BadRequest, message: "not a host connection".into() });
        };
        let mut core = self.lock();
        if core.host_owner.get(hid) != Some(conn) {
            return Some(ServerMsg::Error { code: ErrorCode::BadRequest, message: "host superseded by a newer connection".into() });
        }
        let volume = volume.min(100);
        self.edit_hosts(&mut core, |hosts| {
            if let Some(h) = hosts.iter_mut().find(|h| &h.id == hid) {
                h.volume = volume;
                h.mute = mute;
            }
        });
        None
    }

    /// Unlock (or relock) a bus's soft cap for one connection, for safety::UNLOCK_S seconds.
    fn unlock(&self, unlocks: &mut HashMap<String, Instant>, bus: &str, on: bool, who: &str) -> ServerMsg {
        let cap = {
            let core = self.lock();
            match core.state.bus(bus) {
                Some(b) => b.max_db,
                None => return ServerMsg::Error { code: ErrorCode::BadRequest, message: format!("unknown bus '{}'", sanitize_name(bus)) },
            }
        };
        if on {
            let Some(cap) = cap else {
                return ServerMsg::Error { code: ErrorCode::BadRequest, message: format!("bus {bus} has no cap to unlock") };
            };
            unlocks.insert(bus.to_string(), Instant::now() + Duration::from_secs(safety::UNLOCK_S));
            eprintln!("wavelink remote: {who}: UNLOCKED bus {bus} above its {cap} dB cap for {} s (the ceiling still holds)", safety::UNLOCK_S);
            ServerMsg::Unlock { bus: bus.into(), seconds: safety::UNLOCK_S as u32 }
        } else {
            if unlocks.remove(bus).is_some() {
                eprintln!("wavelink remote: {who}: relocked bus {bus}");
            }
            ServerMsg::Unlock { bus: bus.into(), seconds: 0 }
        }
    }
}
