//! The connection to one engine, on its own thread: connect, Hello, then pump frames both ways.
//! Reconnects with backoff until dropped; a refused token stops it (no point retrying).
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tungstenite::Message;
use wavelink_proto::{self as p, ClientMsg, ErrorCode, ServerMsg};

#[derive(Debug)]
pub enum Event {
    Connecting(String),
    Connected { server: String },
    Msg(ServerMsg),
    /// Lost the connection; retrying.
    Lost(String),
    /// Refused for good (bad token, protocol mismatch); not retrying.
    Fatal(String),
}

pub enum Out {
    Send(ClientMsg),
}

pub struct Conn {
    tx: Sender<Out>,
    pub rx: Receiver<Event>,
    stop: Arc<AtomicBool>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl Conn {
    pub fn send(&self, m: ClientMsg) {
        let _ = self.tx.send(Out::Send(m));
    }
}

/// `addr` is "host:port" (port defaults to 8790). `wake` is called after every event (egui repaint).
pub fn connect(addr: String, token: String, meters: bool, wake: impl Fn() + Send + 'static) -> Conn {
    let (tx, orx) = mpsc::channel::<Out>();
    let (etx, rx) = mpsc::channel::<Event>();
    let stop = Arc::new(AtomicBool::new(false));
    let st = stop.clone();
    std::thread::spawn(move || {
        let emit = |e: Event| {
            let _ = etx.send(e);
            wake();
        };
        let mut backoff = Duration::from_secs(1);
        let addr = if addr.contains(':') && !addr.ends_with(']') { addr } else { format!("{addr}:{}", p::DEFAULT_PORT) };
        while !st.load(Ordering::SeqCst) {
            emit(Event::Connecting(addr.clone()));
            let started = Instant::now();
            match session(&addr, &token, meters, &orx, &st, &emit) {
                Ok(()) => return, // stopped
                Err(Fail::Fatal(m)) => {
                    emit(Event::Fatal(m));
                    return;
                }
                Err(Fail::Retry(m)) => emit(Event::Lost(m)),
            }
            if started.elapsed() > Duration::from_secs(20) {
                backoff = Duration::from_secs(1);
            }
            let t0 = Instant::now();
            while t0.elapsed() < backoff && !st.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(50));
            }
            backoff = (backoff * 2).min(Duration::from_secs(10));
            // commands queued while offline are stale (old fader positions): drop them
            while orx.try_recv().is_ok() {}
        }
    });
    Conn { tx, rx, stop }
}

enum Fail {
    Retry(String),
    Fatal(String),
}

fn timeout(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(io) if matches!(io.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut))
}

fn session(addr: &str, token: &str, meters: bool, orx: &Receiver<Out>, stop: &AtomicBool, emit: &dyn Fn(Event)) -> Result<(), Fail> {
    let r = |e: String| Fail::Retry(e);
    let sa = addr.to_socket_addrs().map_err(|e| r(format!("{addr}: {e}")))?.collect::<Vec<_>>();
    // prefer IPv4 (mDNS .local names often resolve to a link-local v6 first)
    let mut sa = sa;
    sa.sort_by_key(|a| !a.is_ipv4());
    let mut last = String::from("no address");
    let mut stream = None;
    for a in sa {
        match TcpStream::connect_timeout(&a, Duration::from_secs(3)) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => last = format!("{a}: {e}"),
        }
    }
    let s = stream.ok_or_else(|| r(last))?;
    s.set_nodelay(true).ok();
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    s.set_write_timeout(Some(Duration::from_secs(5))).ok();
    let (mut ws, _) = tungstenite::client(format!("ws://{addr}/"), s).map_err(|e| r(format!("handshake: {e}")))?;
    let hello = ClientMsg::Hello(p::Hello {
        protocol: p::PROTOCOL_VERSION,
        token: token.trim().to_string(),
        client: format!("Wavelink Remote {} ({})", env!("CARGO_PKG_VERSION"), std::env::consts::OS),
        meters,
        host: None,
        full: false,
    });
    ws.send(Message::text(hello.to_json())).map_err(|e| r(e.to_string()))?;
    match ws.read() {
        Ok(Message::Text(t)) => match ServerMsg::from_json(t.as_str()) {
            Ok(ServerMsg::Welcome { server, .. }) => emit(Event::Connected { server }),
            Ok(ServerMsg::Error { code: ErrorCode::Auth, .. }) => return Err(Fail::Fatal("The engine refused the token. Copy it again with `wavelink-mixer remote-token` on the PC.".into())),
            Ok(ServerMsg::Error { code: ErrorCode::Protocol, message }) => return Err(Fail::Fatal(format!("Version mismatch: {message}. Update this app or the engine."))),
            Ok(ServerMsg::Error { message, .. }) => return Err(r(message)),
            Ok(m) => return Err(r(format!("unexpected first message {m:?}"))),
            Err(e) => return Err(r(format!("bad message: {e}"))),
        },
        Ok(m) => return Err(r(format!("unexpected frame {m:?}"))),
        Err(e) => return Err(r(format!("no welcome: {e}"))),
    }
    ws.get_ref().set_read_timeout(Some(Duration::from_millis(10))).ok();
    let mut last_ping = Instant::now();
    let mut heard = Instant::now();
    loop {
        if stop.load(Ordering::SeqCst) {
            let _ = ws.close(None);
            let _ = ws.flush();
            return Ok(());
        }
        match ws.read() {
            Ok(Message::Text(t)) => {
                heard = Instant::now();
                match ServerMsg::from_json(t.as_str()) {
                    Ok(m) => emit(Event::Msg(m)),
                    Err(e) => eprintln!("wavelink-remote: ignoring message: {e}"),
                }
            }
            Ok(Message::Close(_)) => return Err(r("closed by the engine".into())),
            Ok(_) => heard = Instant::now(),
            Err(e) if timeout(&e) => {}
            Err(e) => return Err(r(e.to_string())),
        }
        loop {
            match orx.try_recv() {
                Ok(Out::Send(m)) => ws.write(Message::text(m.to_json())).map_err(|e| r(e.to_string()))?,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }
        if last_ping.elapsed() > Duration::from_secs(10) {
            last_ping = Instant::now();
            ws.write(Message::text(ClientMsg::Ping { t: 0 }.to_json())).map_err(|e| r(e.to_string()))?;
        }
        match ws.flush() {
            Ok(()) => {}
            Err(e) if timeout(&e) => {}
            Err(e) => return Err(r(e.to_string())),
        }
        if heard.elapsed() > Duration::from_secs(30) {
            return Err(r("engine stopped answering".into()));
        }
    }
}
