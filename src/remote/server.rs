//! WebSocket control server: one accept thread, one thread per client, a config watcher and a meter pump.
//!
//! The server is the single writer of config.json for remote edits: every command is applied to the
//! freshest file (read under the config lock), saved atomically, pushed into the graph, and broadcast
//! to every client as a delta. Edits other writers make (the GTK window) are picked up by the watcher
//! and broadcast the same way.
use super::backend::Backend;
use super::model;
use crate::config::{self, Config};
use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tungstenite::Message;
use wavelink_proto::{self as p, safety, ClientMsg, ErrorCode, ServerMsg};

pub struct Opts {
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
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// A client that sends nothing (not even its ping) for this long is dropped.
const IDLE_TIMEOUT: Duration = Duration::from_secs(45);
const WATCH_EVERY: Duration = Duration::from_millis(250);

struct Client {
    id: u64,
    tx: SyncSender<Arc<str>>,
    meters: Arc<AtomicBool>,
}

struct Core {
    cfg: Config,
    state: p::State,
    clients: Vec<Client>,
}

struct Inner {
    opts: Opts,
    backend: Arc<dyn Backend>,
    core: Mutex<Core>,
    next_id: AtomicU64,
    stop: AtomicBool,
}

/// A running server; dropping it stops the accept loop and the pumps (client threads end with their sockets).
pub struct Running {
    pub addr: SocketAddr,
    inner: Arc<Inner>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
impl Running {
    pub fn clients(&self) -> usize {
        self.inner.lock().clients.len()
    }
}

pub fn serve(opts: Opts, backend: Arc<dyn Backend>) -> std::io::Result<Running> {
    let listener = TcpListener::bind((opts.bind.as_str(), opts.port))?;
    listener.set_nonblocking(true)?;
    let addr = listener.local_addr()?;
    let cfg = Config::load_from(&opts.cfg_path);
    let state = model::to_state(&cfg, &opts.name, 1, &opts.caps);
    let inner = Arc::new(Inner {
        opts,
        backend,
        core: Mutex::new(Core { cfg, state, clients: vec![] }),
        next_id: AtomicU64::new(1),
        stop: AtomicBool::new(false),
    });
    {
        let inner = inner.clone();
        std::thread::Builder::new().name("remote-accept".into()).spawn(move || {
            while !inner.stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((s, peer)) => {
                        let inner = inner.clone();
                        let _ = std::thread::Builder::new().name("remote-client".into()).spawn(move || {
                            match inner.conn(s) {
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
    }
    {
        let inner = inner.clone();
        std::thread::Builder::new().name("remote-watch".into()).spawn(move || {
            while !inner.stop.load(Ordering::SeqCst) {
                {
                    let mut core = inner.lock();
                    inner.sync_disk(&mut core);
                }
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
    Ok(Running { addr, inner })
}

fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn io_timeout(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(io) if matches!(io.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut))
}

impl Inner {
    fn lock(&self) -> std::sync::MutexGuard<'_, Core> {
        self.core.lock().unwrap_or_else(|e| e.into_inner())
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

    fn command(&self, id: u64, cmd: &p::Command) -> ServerMsg {
        let nack = |e: String| ServerMsg::Ack { id, ok: false, error: Some(e), note: None };
        let mut core = self.lock();
        let flock = config::lock_config(&self.opts.cfg_path);
        self.sync_disk(&mut core);
        let checked = match safety::check(cmd, &core.state) {
            Ok(c) => c,
            Err(e) => return nack(e.to_string()),
        };
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
        for c in &checked.cmds {
            self.backend.apply(&cfg, c);
        }
        self.publish(&mut core, cfg);
        ServerMsg::Ack { id, ok: true, error: None, note: checked.note }
    }

    fn conn(self: &Arc<Self>, s: TcpStream) -> Result<(), String> {
        s.set_nonblocking(false).map_err(|e| e.to_string())?;
        s.set_nodelay(true).ok();
        s.set_read_timeout(Some(HELLO_TIMEOUT)).map_err(|e| e.to_string())?;
        s.set_write_timeout(Some(Duration::from_secs(5))).map_err(|e| e.to_string())?;
        let mut ws = tungstenite::accept(s).map_err(|e| format!("handshake: {e}"))?;
        let send = |ws: &mut tungstenite::WebSocket<TcpStream>, m: &ServerMsg| ws.send(Message::text(m.to_json())).map_err(|e| e.to_string());
        let refuse = |ws: &mut tungstenite::WebSocket<TcpStream>, code: ErrorCode, msg: &str| {
            let _ = send(ws, &ServerMsg::Error { code, message: msg.into() });
            let _ = ws.close(None);
            let _ = ws.flush();
            Err(msg.to_string())
        };
        // first frame must be Hello with the right protocol and token
        let hello = loop {
            match ws.read() {
                Ok(Message::Text(t)) => break ClientMsg::from_json(t.as_str()),
                Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => continue,
                Ok(_) => return refuse(&mut ws, ErrorCode::BadRequest, "expected hello"),
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
        if !ct_eq(hello.token.trim(), &self.opts.token) {
            std::thread::sleep(Duration::from_millis(400)); // slows guessing
            return refuse(&mut ws, ErrorCode::Auth, "bad token");
        }
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
        eprintln!("wavelink remote: client {id} connected ({})", if hello.client.is_empty() { "unnamed" } else { &hello.client });
        let r = self.client_loop(&mut ws, &rx, &meters);
        self.lock().clients.retain(|c| c.id != id);
        let _ = ws.close(None);
        let _ = ws.flush();
        eprintln!("wavelink remote: client {id} left");
        r
    }

    fn client_loop(&self, ws: &mut tungstenite::WebSocket<TcpStream>, rx: &mpsc::Receiver<Arc<str>>, meters: &AtomicBool) -> Result<(), String> {
        ws.get_ref().set_read_timeout(Some(Duration::from_millis(15))).map_err(|e| e.to_string())?;
        let mut heard = Instant::now();
        loop {
            if self.stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            match ws.read() {
                Ok(Message::Text(t)) => {
                    heard = Instant::now();
                    let reply = match ClientMsg::from_json(t.as_str()) {
                        Ok(ClientMsg::Command { id, cmd }) => Some(self.command(id, &cmd)),
                        Ok(ClientMsg::Subscribe { meters: on }) => {
                            meters.store(on, Ordering::Relaxed);
                            None
                        }
                        Ok(ClientMsg::Ping { t }) => Some(ServerMsg::Pong { t }),
                        Ok(ClientMsg::Hello(_)) => Some(ServerMsg::Error { code: ErrorCode::BadRequest, message: "already greeted".into() }),
                        Err(e) => Some(ServerMsg::Error { code: ErrorCode::BadRequest, message: format!("bad message: {e}") }),
                    };
                    if let Some(r) = reply {
                        ws.write(Message::text(r.to_json())).map_err(|e| e.to_string())?;
                    }
                }
                Ok(Message::Close(_)) => return Ok(()),
                Ok(_) => heard = Instant::now(), // ping/pong/binary: alive
                Err(e) if io_timeout(&e) => {}
                Err(tungstenite::Error::ConnectionClosed) | Err(tungstenite::Error::AlreadyClosed) => return Ok(()),
                Err(e) => return Err(e.to_string()),
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
                Err(e) => return Err(e.to_string()),
            }
            if heard.elapsed() > IDLE_TIMEOUT {
                return Err("idle timeout".into());
            }
        }
    }
}
