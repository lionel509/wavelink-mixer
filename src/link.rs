//! Client link to an engine (#37): a copy of `client/src/net.rs` in the mixer crate (the egui client stays untouched),
//! plus two additions: every Hello carries the host's live volume (`HostSource`), and `Conn::send` can carry
//! `ClientMsg::HostState` like any other message.
//! SCAFFOLD: signatures only (U3 fills them in).
#![allow(dead_code)]
use std::sync::mpsc::Receiver;
use wavelink_proto::{ClientMsg, HostHello, ServerMsg};

/// What the link thread reports to the window.
#[derive(Debug)]
pub enum Event {
    Connecting(String),
    Connected { server: String },
    Msg(ServerMsg),
    Lost(String),
    /// Auth or protocol refused: the link stops and does not retry.
    Fatal(String),
}

/// Called on every Hello (first connect and each reconnect) to fill `Hello.host` with the live volume.
pub type HostSource = Box<dyn Fn() -> HostHello + Send>;

/// A running link. Dropping it stops the thread.
pub struct Conn {
    pub rx: Receiver<Event>,
}

impl Conn {
    /// Queue a message for the engine (`ClientMsg::HostState` included).
    pub fn send(&self, _m: ClientMsg) {
        todo!("U3: link send")
    }
}

/// Connect to `addr` (host:port) with `token`; `wake` is called after each event so a GTK window can poll.
pub fn connect(_addr: String, _token: String, _meters: bool, _host: Option<HostSource>, _wake: impl Fn() + Send + 'static) -> Conn {
    todo!("U3: link connect")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::sync::{mpsc, Arc};
    use std::time::Duration;
    use tungstenite::Message;
    use wavelink_proto::PROTOCOL_VERSION;

    /// Read one text frame from a client and parse it.
    fn read_msg(ws: &mut tungstenite::WebSocket<std::net::TcpStream>) -> ClientMsg {
        loop {
            match ws.read().expect("frame") {
                Message::Text(t) => return ClientMsg::from_json(t.as_str()).expect("client json"),
                _ => continue,
            }
        }
    }

    fn welcome(ws: &mut tungstenite::WebSocket<std::net::TcpStream>) {
        let w = ServerMsg::Welcome { protocol: PROTOCOL_VERSION, server: "fake".into(), version: "t".into() };
        ws.send(Message::text(w.to_json())).unwrap();
    }

    #[test]
    fn hello_carries_host_from_source_on_every_connect() {
        let lis = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = lis.local_addr().unwrap().to_string();
        let vol = Arc::new(AtomicU8::new(10));
        let (tx, rx) = mpsc::channel::<u8>();
        let server_vol = vol.clone();
        std::thread::spawn(move || {
            for round in 0..2 {
                let (s, _) = lis.accept().unwrap();
                let mut ws = tungstenite::accept(s).unwrap();
                let hello = match read_msg(&mut ws) {
                    ClientMsg::Hello(h) => h,
                    m => panic!("expected Hello, got {m:?}"),
                };
                tx.send(hello.host.expect("Hello must carry the host").volume).unwrap();
                welcome(&mut ws);
                if round == 0 {
                    // the Mac's volume moved while the link was up; the drop forces a reconnect
                    server_vol.store(40, Ordering::SeqCst);
                }
                drop(ws);
            }
        });
        let src = vol.clone();
        let _conn = connect(
            addr,
            "tok".into(),
            false,
            Some(Box::new(move || HostHello { id: "macbook".into(), label: "MacBook".into(), volume: src.load(Ordering::SeqCst), mute: false })),
            || {},
        );
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), 10, "first Hello");
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), 40, "reconnect Hello must re-read the source");
    }

    #[test]
    fn host_state_is_sent_as_is() {
        let lis = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = lis.local_addr().unwrap().to_string();
        let (tx, rx) = mpsc::channel::<ClientMsg>();
        std::thread::spawn(move || {
            let (s, _) = lis.accept().unwrap();
            let mut ws = tungstenite::accept(s).unwrap();
            let _hello = read_msg(&mut ws);
            welcome(&mut ws);
            tx.send(read_msg(&mut ws)).unwrap();
        });
        let conn = connect(addr, "tok".into(), false, None, || {});
        loop {
            match conn.rx.recv_timeout(Duration::from_secs(5)) {
                Ok(Event::Connected { .. }) => break,
                Ok(_) => continue,
                Err(e) => panic!("never connected: {e}"),
            }
        }
        conn.send(ClientMsg::HostState { volume: 12, mute: true });
        let got = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(got, ClientMsg::HostState { volume: 12, mute: true });
    }
}
