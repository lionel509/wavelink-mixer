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
}

struct Core {
    cfg: Config,
    state: p::State,
    clients: Vec<Client>,
    /// time-based raise limit per physical bus, shared by every client
    limits: HashMap<String, safety::RaiseLimiter>,
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
        core: Mutex::new(Core { cfg, state, clients: vec![], limits: HashMap::new() }),
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

    /// Make `cfg` current and tell everyone what changed (deltas, or a full state on structure changes).
    fn publish(&self, core: &mut Core, cfg: Config) {
        let new = model::to_state(&cfg, &self.opts.name, core.state.rev + 1, &self.opts.caps);
        core.cfg = cfg;
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
                    let allowed = core.limits.entry(bus.clone()).or_default().allowed(now, cur);
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
            core.limits.entry(bus).or_default().record(now, before, after);
        }
        for c in &checked.cmds {
            self.backend.apply(&cfg, c);
        }
        self.publish(&mut core, cfg);
        ServerMsg::Ack { id, ok: true, error: None, note: checked.note }
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
        drop(pre); // authenticated: no longer counts against the pre-auth cap
        send(&mut ws, &ServerMsg::Welcome { protocol: p::PROTOCOL_VERSION, server: self.opts.name.clone(), version: env!("CARGO_PKG_VERSION").into() })?;
        let (tx, rx) = mpsc::sync_channel::<Arc<str>>(QUEUE);
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let meters = Arc::new(AtomicBool::new(hello.meters));
        {
            // snapshot first, then join the broadcast list, under one lock: no delta can slip between
            let mut core = self.lock();
            self.sync_disk(&mut core);
            let _ = tx.try_send(ServerMsg::State { state: core.state.clone() }.to_json().into());
            core.clients.push(Client { id, tx, meters: meters.clone() });
        }
        let who = format!("client {id} ({}, {peer})", sanitize_name(&hello.client));
        eprintln!("wavelink remote: {who} connected");
        let r = self.client_loop(&mut ws, &rx, &meters, &who, token_gen);
        self.lock().clients.retain(|c| c.id != id);
        let _ = ws.close(None);
        let _ = ws.flush();
        eprintln!("wavelink remote: {who} left");
        r
    }

    fn client_loop(&self, ws: &mut tungstenite::WebSocket<TcpStream>, rx: &mpsc::Receiver<Arc<str>>, meters: &AtomicBool, who: &str, token_gen: u64) -> Result<(), String> {
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
                        Ok(ClientMsg::Subscribe { meters: on }) => {
                            meters.store(on, Ordering::Relaxed);
                            None
                        }
                        Ok(ClientMsg::Unlock { bus, on }) => Some(self.unlock(&mut unlocks, &bus, on, who)),
                        Ok(ClientMsg::Ping { t }) => Some(ServerMsg::Pong { t }),
                        Ok(ClientMsg::Hello(_)) => Some(ServerMsg::Error { code: ErrorCode::BadRequest, message: "already greeted".into() }),
                        // scaffold (U2 todo): HostState is not handled yet; answers with a non-BadRequest error
                        Ok(ClientMsg::HostState { .. }) => Some(ServerMsg::Error { code: ErrorCode::Other, message: "todo: host state".into() }),
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
