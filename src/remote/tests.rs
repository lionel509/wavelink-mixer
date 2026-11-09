//! Server tests: a real WebSocket server on 127.0.0.1:0 with a fake graph backend and a config in a temp dir.
use super::backend::FakeBackend;
use super::server::{serve, Opts, Running};
use crate::config::{Bus, BusKind, Config};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tungstenite::{Message, WebSocket};
use wavelink_proto::*;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

struct Rig {
    dir: PathBuf,
    cfg: PathBuf,
    fake: Arc<FakeBackend>,
    run: Running,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn rig(tag: &str) -> Rig {
    rig_with(tag, |_| {})
}

fn rig_with(tag: &str, edit: impl FnOnce(&mut Config)) -> Rig {
    let dir = std::env::temp_dir().join(format!("wavelink-remote-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let cfg_path = dir.join(".config/wavelink-mixer/config.json");
    let mut c = Config::default_config();
    c.buses.insert(
        2,
        Bus { id: "a3".into(), name: "Sonos".into(), color: "#ff9f0a".into(), kind: BusKind::Hardware { target: "raop_sonos_test".into() }, gain_db: -30.0, mute: false, max_db: None },
    );
    edit(&mut c);
    assert!(c.save_to(&cfg_path));
    let fake = Arc::new(FakeBackend::default());
    let run = serve(
        Opts { bind: "127.0.0.1".into(), port: 0, token: TOKEN.into(), cfg_path: cfg_path.clone(), name: "testhost".into(), caps: Default::default() },
        fake.clone(),
    )
    .unwrap();
    Rig { dir, cfg: cfg_path, fake, run }
}

type Ws = WebSocket<TcpStream>;

fn connect(r: &Rig) -> Ws {
    let s = TcpStream::connect(r.run.addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let (ws, _) = tungstenite::client(format!("ws://{}/", r.run.addr), s).unwrap();
    ws
}

fn send(ws: &mut Ws, m: &ClientMsg) {
    ws.send(Message::text(m.to_json())).unwrap();
}

fn recv(ws: &mut Ws) -> Option<ServerMsg> {
    loop {
        match ws.read() {
            Ok(Message::Text(t)) => return Some(ServerMsg::from_json(t.as_str()).unwrap()),
            Ok(Message::Close(_)) => return None,
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
}

/// Next message that isn't a meter frame.
fn recv_np(ws: &mut Ws) -> Option<ServerMsg> {
    loop {
        match recv(ws)? {
            ServerMsg::Meters { .. } => continue,
            m => return Some(m),
        }
    }
}

fn hello(ws: &mut Ws, token: &str, meters: bool) {
    send(ws, &ClientMsg::Hello(Hello { protocol: PROTOCOL_VERSION, token: token.into(), client: "test".into(), meters }));
}

/// Connect, greet, return the socket and the first snapshot.
fn join(r: &Rig) -> (Ws, State) {
    let mut ws = connect(r);
    hello(&mut ws, TOKEN, false);
    assert!(matches!(recv_np(&mut ws), Some(ServerMsg::Welcome { protocol: PROTOCOL_VERSION, .. })));
    match recv_np(&mut ws) {
        Some(ServerMsg::State { state }) => (ws, state),
        m => panic!("expected state, got {m:?}"),
    }
}

/// Drop whatever is already queued (other clients' deltas), so the next deltas are this command's.
fn drain(ws: &mut Ws) {
    ws.get_ref().set_read_timeout(Some(Duration::from_millis(60))).unwrap();
    while ws.read().is_ok() {}
    ws.get_ref().set_read_timeout(Some(Duration::from_secs(3))).unwrap();
}

fn cmd(ws: &mut Ws, id: u64, c: Command) -> (bool, Option<String>, Option<String>, Vec<Delta>) {
    drain(ws);
    send(ws, &ClientMsg::Command { id, cmd: c });
    let mut deltas = vec![];
    // the ack and the delta may arrive in either order
    let mut ack = None;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(3) {
        match recv_np(ws) {
            Some(ServerMsg::Ack { id: i, ok, error, note }) if i == id => {
                ack = Some((ok, error, note));
                if !ok || !deltas.is_empty() {
                    break;
                }
            }
            Some(ServerMsg::Delta { changes, .. }) => {
                deltas.extend(changes);
                if ack.is_some() {
                    break;
                }
            }
            Some(_) => {}
            None => break,
        }
    }
    let (ok, error, note) = ack.expect("ack");
    (ok, error, note, deltas)
}

#[test]
fn rejects_bad_token_and_protocol() {
    let r = rig("auth");
    let mut ws = connect(&r);
    hello(&mut ws, "wrong", false);
    assert!(matches!(recv(&mut ws), Some(ServerMsg::Error { code: ErrorCode::Auth, .. })));
    let mut ws = connect(&r);
    send(&mut ws, &ClientMsg::Hello(Hello { protocol: PROTOCOL_VERSION + 1, token: TOKEN.into(), client: String::new(), meters: false }));
    assert!(matches!(recv(&mut ws), Some(ServerMsg::Error { code: ErrorCode::Protocol, .. })));
    let mut ws = connect(&r);
    send(&mut ws, &ClientMsg::Ping { t: 1 });
    assert!(matches!(recv(&mut ws), Some(ServerMsg::Error { code: ErrorCode::BadRequest, .. })));
    assert!(r.fake.applied.lock().unwrap().is_empty());
}

#[test]
fn snapshot_matches_config() {
    let r = rig("snap");
    let (_ws, st) = join(&r);
    let c = Config::peek(&r.cfg).unwrap();
    assert_eq!(st.server, "testhost");
    assert_eq!(st.inputs.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), c.inputs.iter().map(|i| i.id.as_str()).collect::<Vec<_>>());
    assert_eq!(st.buses.len(), c.buses.len());
    let sonos = st.bus("a3").unwrap();
    assert_eq!(sonos.max_db, Some(-20.0));
    assert!(sonos.physical());
    assert_eq!(st.bus("a1").unwrap().max_db, None);
    assert_eq!(st.bus("b1").unwrap().kind, wavelink_proto::BusKind::Virtual);
    assert_eq!(st.input("spotify").unwrap().route("a1"), Route { on: true, level_db: -4.2 });
    assert!(matches!(st.input("mic").unwrap().source, InputSource::Device { .. }));
}

#[test]
fn command_saves_applies_and_broadcasts() {
    let r = rig("cmd");
    let (mut a, _) = join(&r);
    let (mut b, _) = join(&r);
    let (ok, _, _, d) = cmd(&mut a, 1, Command::SetInputGain { input: "spotify".into(), db: -9.5 });
    assert!(ok);
    assert_eq!(d, vec![Delta::InputGain { input: "spotify".into(), db: -9.5 }]);
    // the other client hears it too
    match recv_np(&mut b) {
        Some(ServerMsg::Delta { changes, .. }) => assert_eq!(changes, d),
        m => panic!("{m:?}"),
    }
    // saved to disk and pushed to the graph
    assert_eq!(Config::peek(&r.cfg).unwrap().inputs.iter().find(|i| i.id == "spotify").unwrap().gain_db, -9.5);
    assert_eq!(r.fake.applied.lock().unwrap().as_slice(), &[Command::SetInputGain { input: "spotify".into(), db: -9.5 }]);

    let (ok, _, _, d) = cmd(&mut a, 2, Command::SetRoute { input: "spotify".into(), bus: "b2".into(), on: Some(true), level_db: Some(-12.0) });
    assert!(ok);
    assert_eq!(d, vec![Delta::Route { input: "spotify".into(), bus: "b2".into(), route: Route { on: true, level_db: -12.0 } }]);
    let (ok, _, _, d) = cmd(&mut a, 3, Command::SetNormalize { input: "game".into(), on: Some(true), target_db: Some(-24.0) });
    assert!(ok);
    assert_eq!(d, vec![Delta::Normalize { input: "game".into(), on: true, target_db: Some(-24.0) }]);
    let c = Config::peek(&r.cfg).unwrap();
    let game = c.inputs.iter().find(|i| i.id == "game").unwrap();
    assert!(game.normalize);
    assert_eq!(game.norm_out, Some(-24.0));
    let (ok, _, _, d) = cmd(&mut a, 4, Command::SetBusMute { bus: "b1".into(), mute: true });
    assert!(ok);
    assert_eq!(d, vec![Delta::BusMute { bus: "b1".into(), mute: true }]);
    let (ok, _, _, d) = cmd(&mut a, 5, Command::SetInputMute { input: "mic".into(), mute: true });
    assert!(ok);
    assert_eq!(d, vec![Delta::InputMute { input: "mic".into(), mute: true }]);

    // invalid: refused, nothing applied, nothing written
    let before = std::fs::read(&r.cfg).unwrap();
    let n = r.fake.applied.lock().unwrap().len();
    let (ok, err, _, d) = cmd(&mut a, 6, Command::SetInputGain { input: "nope".into(), db: 0.0 });
    assert!(!ok && err.unwrap().contains("unknown input"));
    assert!(d.is_empty());
    assert_eq!(std::fs::read(&r.cfg).unwrap(), before);
    assert_eq!(r.fake.applied.lock().unwrap().len(), n);
}

#[test]
fn speaker_cap_and_step_are_enforced_server_side() {
    let r = rig("caps");
    let (mut a, _) = join(&r);
    // a3 at -30, cap -20: a jump to 0 dB goes up one 6 dB step only
    let (ok, _, note, d) = cmd(&mut a, 1, Command::SetBusGain { bus: "a3".into(), db: 0.0, unlock: false });
    assert!(ok);
    assert!(note.unwrap().contains("per step"));
    assert_eq!(d, vec![Delta::BusGain { bus: "a3".into(), db: -24.0 }]);
    let (_, _, note, d) = cmd(&mut a, 2, Command::SetBusGain { bus: "a3".into(), db: 0.0, unlock: false });
    assert!(note.unwrap().contains("capped at -20"));
    assert_eq!(d, vec![Delta::BusGain { bus: "a3".into(), db: -20.0 }]);
    // at the cap: no change, no delta; the command's own unlock flag changes nothing
    let (ok, _, _, d) = cmd(&mut a, 3, Command::SetBusGain { bus: "a3".into(), db: -5.0, unlock: false });
    assert!(ok && d.is_empty());
    let (ok, _, _, d) = cmd(&mut a, 4, Command::SetBusGain { bus: "a3".into(), db: 0.0, unlock: true });
    assert!(ok && d.is_empty(), "{d:?}");
    // an unlock can't go past the hard ceiling: this AirPlay bus's ceiling is -20 too (#35)
    assert_eq!(unlock(&mut a, "a3", true), 60);
    let (_, _, note, d) = cmd(&mut a, 5, Command::SetBusGain { bus: "a3".into(), db: 0.0, unlock: false });
    assert!(d.is_empty() && note.unwrap().contains("ceiling"));
    // down is never limited
    let (_, _, _, d) = cmd(&mut a, 6, Command::SetBusGain { bus: "a3".into(), db: -60.0, unlock: false });
    assert_eq!(d, vec![Delta::BusGain { bus: "a3".into(), db: -60.0 }]);
    // headphones: step limit, no cap
    let (_, _, _, d) = cmd(&mut a, 7, Command::SetBusGain { bus: "a1".into(), db: 12.0, unlock: false });
    assert_eq!(d, vec![Delta::BusGain { bus: "a1".into(), db: -4.2 + 6.0 }]);
    let applied = r.fake.applied.lock().unwrap();
    assert!(applied.iter().all(|c| !matches!(c, Command::SetBusGain { bus, db, .. } if bus == "a3" && *db > -20.0)));
}

/// Send Unlock and return the seconds the server granted (0 = locked).
fn unlock(ws: &mut Ws, bus: &str, on: bool) -> u32 {
    send(ws, &ClientMsg::Unlock { bus: bus.into(), on });
    loop {
        match recv_np(ws) {
            Some(ServerMsg::Unlock { bus: b, seconds }) if b == bus => return seconds,
            Some(ServerMsg::Error { message, .. }) => panic!("unlock refused: {message}"),
            Some(_) => {}
            None => panic!("closed"),
        }
    }
}

#[test]
fn unlock_is_per_connection_bounded_by_the_ceiling_and_rate_limited() {
    // a raised ceiling (-10) above the speaker's soft cap (-20)
    let r = rig_with("unlock", |c| c.buses.iter_mut().find(|b| b.id == "a3").unwrap().max_db = Some(-10.0));
    let (mut a, st) = join(&r);
    assert_eq!((st.bus("a3").unwrap().max_db, st.bus("a3").unwrap().ceiling_db), (Some(-20.0), Some(-10.0)));
    let (mut b, _) = join(&r);
    assert!(cmd(&mut a, 1, Command::SetBusGain { bus: "a3".into(), db: -20.0, unlock: false }).0); // -30 -> -24
    assert!(cmd(&mut a, 2, Command::SetBusGain { bus: "a3".into(), db: -20.0, unlock: false }).0); // -> -20
    assert_eq!(unlock(&mut a, "a3", true), 60);
    // within the same second the raise budget (12 dB above the window's floor of -30) is nearly spent
    let (_, _, note, d) = cmd(&mut a, 3, Command::SetBusGain { bus: "a3".into(), db: 0.0, unlock: false });
    assert_eq!(d, vec![Delta::BusGain { bus: "a3".into(), db: -18.0 }]);
    assert!(note.unwrap().contains("per 1 s"));
    // the other connection holds no unlock: it can't go above the soft cap (only down)
    let (ok, _, note, _) = cmd(&mut b, 4, Command::SetBusGain { bus: "a3".into(), db: -15.0, unlock: true });
    assert!(ok && note.unwrap().contains("capped at -20"));
    assert_eq!(Config::peek(&r.cfg).unwrap().buses.iter().find(|b| b.id == "a3").unwrap().gain_db, -20.0);
    std::thread::sleep(Duration::from_millis(1100));
    let (_, _, _, d) = cmd(&mut a, 5, Command::SetBusGain { bus: "a3".into(), db: 0.0, unlock: false });
    assert_eq!(d, vec![Delta::BusGain { bus: "a3".into(), db: -14.0 }]);
    // the hard ceiling holds even unlocked
    let (_, _, note, d) = cmd(&mut a, 6, Command::SetBusGain { bus: "a3".into(), db: 0.0, unlock: false });
    assert_eq!(d, vec![Delta::BusGain { bus: "a3".into(), db: -10.0 }]);
    assert!(note.unwrap().contains("ceiling"));
    assert_eq!(Config::peek(&r.cfg).unwrap().buses.iter().find(|b| b.id == "a3").unwrap().gain_db, -10.0);
    // relock: back under the soft cap rules
    assert_eq!(unlock(&mut a, "a3", false), 0);
    let (_, _, _, d) = cmd(&mut a, 7, Command::SetBusGain { bus: "a3".into(), db: -12.0, unlock: false });
    assert_eq!(d, vec![Delta::BusGain { bus: "a3".into(), db: -20.0 }]);
    // an unlock dies with its connection
    assert_eq!(unlock(&mut a, "a3", true), 60);
    drop(a);
    let (mut c, _) = join(&r);
    let (_, _, note, _) = cmd(&mut c, 8, Command::SetBusGain { bus: "a3".into(), db: -14.0, unlock: true });
    assert!(note.unwrap().contains("capped at -20"));
    // a bus without a cap has nothing to unlock
    send(&mut c, &ClientMsg::Unlock { bus: "b1".into(), on: true });
    assert!(matches!(recv_np(&mut c), Some(ServerMsg::Error { code: ErrorCode::BadRequest, .. })));
}

#[test]
fn remote_writes_respect_the_config_ceiling() {
    // a user ceiling on the headphones (#35's max_db): the remote can never go above it
    let r = rig_with("ceiling", |c| c.buses[0].max_db = Some(-10.0));
    let (mut a, st) = join(&r);
    assert_eq!(st.bus("a1").unwrap().ceiling_db, Some(-10.0));
    assert_eq!(st.bus("a1").unwrap().max_db, Some(-10.0), "the soft cap is never above the ceiling");
    let (_, _, _, d) = cmd(&mut a, 1, Command::SetBusGain { bus: "a1".into(), db: 12.0, unlock: false });
    assert_eq!(d, vec![Delta::BusGain { bus: "a1".into(), db: -10.0 }]);
    assert!(r.fake.applied.lock().unwrap().iter().all(|c| !matches!(c, Command::SetBusGain { db, .. } if *db > -10.0)));
}

/// Intended behaviour changed in fix round 3 (breaker #36 R2-1): a full set of silent pre-auth sockets no
/// longer locks anyone out. A new connection is admitted and the OLDEST IDLE pending one is closed (per
/// IP at MAX_PREAUTH_PER_IP, loopback exempt; overall at MAX_PREAUTH). The total cap still refuses beyond
/// MAX_CONNS.
#[test]
fn connection_caps_hold() {
    let r = rig("conncap");
    let alive = |ws: &mut Ws| -> bool {
        ws.get_ref().set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        match ws.read() {
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => true,
            _ => false,
        }
    };
    let mut idle: Vec<Ws> = (0..super::server::MAX_PREAUTH).map(|_| connect(&r)).collect();
    std::thread::sleep(Duration::from_millis(300));
    let mut newer = connect(&r);
    std::thread::sleep(Duration::from_millis(200));
    assert!(!alive(&mut idle[0]), "the oldest idle pending socket was closed to make room");
    assert!(alive(&mut idle[1]) && alive(&mut newer), "the others are still pending");
    // and a real client always gets in, however many silent sockets came first
    std::thread::sleep(Duration::from_millis(150));
    let (mut ok, _) = join(&r);
    assert!(cmd(&mut ok, 1, Command::SetInputMute { input: "mic".into(), mute: true }).0);
    drop((idle, newer, ok));
    std::thread::sleep(Duration::from_millis(500));
    // MAX_CONNS authenticated sessions: the next connection is refused (the total cap stays)
    let mut held = vec![];
    for _ in 0..super::server::MAX_CONNS {
        held.push(join(&r).0);
    }
    let s = TcpStream::connect(r.run.addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    assert!(tungstenite::client(format!("ws://{}/", r.run.addr), s).is_err());
    drop(held);
    // and once they are gone, a new client gets in
    std::thread::sleep(Duration::from_millis(500));
    let _ = join(&r);
}

#[test]
fn preauth_admission_policy() {
    use super::server::{admit_policy, Admit, MAX_PREAUTH, PREAUTH_IDLE};
    use std::net::{IpAddr, Ipv4Addr};
    let (a, b, lo) = (IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)), IpAddr::V4(Ipv4Addr::new(192, 168, 1, 60)), IpAddr::V4(Ipv4Addr::LOCALHOST));
    let idle = PREAUTH_IDLE * 3;
    let fresh = Duration::from_millis(5);
    // per-IP cap: a's own oldest idle socket goes; a peer with only fresh sockets is refused (hurts itself)
    assert_eq!(admit_policy(&[(a, idle * 2), (a, idle)], a), Admit::Evict(0));
    assert_eq!(admit_policy(&[(a, fresh), (a, fresh)], a), Admit::Refuse);
    assert_eq!(admit_policy(&[(a, fresh), (a, fresh)], b), Admit::Ok);
    // loopback is exempt from the per-IP cap
    assert_eq!(admit_policy(&[(lo, fresh), (lo, fresh)], lo), Admit::Ok);
    // overall cap: silent sockets from 2 hosts never lock out a third
    let full = vec![(a, idle * 4), (a, idle * 3), (b, idle * 2), (b, idle)];
    assert_eq!(full.len(), MAX_PREAUTH);
    assert_eq!(admit_policy(&full, lo), Admit::Evict(0));
    // fresh sockets are never closed; brief overflow allowed, then refused
    let busy: Vec<_> = (0..MAX_PREAUTH).map(|_| (lo, fresh)).collect();
    assert_eq!(admit_policy(&busy, lo), Admit::Ok);
    let flood: Vec<_> = (0..2 * MAX_PREAUTH).map(|_| (lo, fresh)).collect();
    assert_eq!(admit_policy(&flood, lo), Admit::Refuse);
}

#[test]
fn airplay_by_props_only_shows_the_capped_gain() {
    // a renamed AirPlay sink: nothing in its name, only the node props say so
    let r = rig_with("props", |c| {
        c.buses[1].kind = BusKind::Hardware { target: "living_room".into() };
        c.buses[1].gain_db = -5.0;
    });
    r.fake.raop_nodes.lock().unwrap().push("living_room".into());
    let (mut a, st) = join(&r);
    assert_eq!(st.bus("a2").unwrap().ceiling_db, None, "unknown until the graph is asked");
    let (ok, ..) = cmd(&mut a, 1, Command::SetBusGain { bus: "a2".into(), db: -3.0, unlock: false });
    assert!(ok);
    let c = Config::peek(&r.cfg).unwrap();
    let a2 = c.buses.iter().find(|b| b.id == "a2").unwrap();
    assert_eq!((a2.max_db, a2.gain_db), (Some(-20.0), -20.0), "ceiling saved and the gain shown as what the speaker plays");
}

#[test]
fn client_names_are_sanitised_for_logs() {
    assert_eq!(super::server::sanitize_name("Mac\nwavelink remote: client 9 connected (fake)"), "Macwavelink remote: client 9 connected (fake)");
    assert_eq!(super::server::sanitize_name("\u{1b}[31mred"), "[31mred");
    assert_eq!(super::server::sanitize_name(""), "unnamed");
    assert_eq!(super::server::sanitize_name(&"x".repeat(500)).len(), 64);
}

#[test]
fn external_edits_are_broadcast() {
    let r = rig("watch");
    let (mut a, _) = join(&r);
    // the GTK window saves a change
    let mut c = Config::peek(&r.cfg).unwrap();
    c.inputs[0].gain_db = -17.0;
    c.buses[0].mute = true;
    c.save_to(&r.cfg);
    let mut got = vec![];
    while got.len() < 2 {
        match recv_np(&mut a) {
            Some(ServerMsg::Delta { changes, .. }) => got.extend(changes),
            m => panic!("{m:?}"),
        }
    }
    assert!(got.contains(&Delta::InputGain { input: c.inputs[0].id.clone(), db: -17.0 }));
    assert!(got.contains(&Delta::BusMute { bus: "a1".into(), mute: true }));
    // a structural change (renamed input) arrives as a full state
    c.inputs[1].name = "Music".into();
    c.save_to(&r.cfg);
    match recv_np(&mut a) {
        Some(ServerMsg::State { state }) => assert_eq!(state.inputs[1].name, "Music"),
        m => panic!("{m:?}"),
    }
    // the backend is not asked to re-apply other writers' edits (they apply their own)
    assert!(r.fake.applied.lock().unwrap().is_empty());
}

#[test]
fn command_merges_with_unseen_external_edit() {
    let r = rig("race");
    let (mut a, _) = join(&r);
    // written right before the command, possibly before the watcher saw it
    let mut c = Config::peek(&r.cfg).unwrap();
    c.inputs[2].mute = true;
    c.save_to(&r.cfg);
    let (ok, ..) = cmd(&mut a, 1, Command::SetInputGain { input: "system".into(), db: -3.0 });
    assert!(ok);
    let disk = Config::peek(&r.cfg).unwrap();
    assert!(disk.inputs[2].mute, "the other writer's edit survived");
    assert_eq!(disk.inputs[0].gain_db, -3.0);
}

#[test]
fn meters_stream_while_subscribed() {
    let r = rig("meters");
    let (mut a, st) = join(&r);
    send(&mut a, &ClientMsg::Subscribe { meters: true });
    let t0 = Instant::now();
    let mut frames = 0;
    while frames < 5 && t0.elapsed() < Duration::from_secs(3) {
        if let Some(ServerMsg::Meters { meters }) = recv(&mut a) {
            assert_eq!(meters.inputs.len(), st.inputs.len());
            assert_eq!(meters.buses.len(), st.buses.len());
            frames += 1;
        }
    }
    assert_eq!(frames, 5);
    // about 15 fps: 5 frames take at least ~250 ms
    assert!(t0.elapsed() >= Duration::from_millis(200), "{:?}", t0.elapsed());
    send(&mut a, &ClientMsg::Subscribe { meters: false });
    let t1 = Instant::now();
    while r.fake.stopped.load(std::sync::atomic::Ordering::SeqCst) == 0 && t1.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(r.fake.stopped.load(std::sync::atomic::Ordering::SeqCst), 1, "taps stopped once nobody watches");
    send(&mut a, &ClientMsg::Ping { t: 77 });
    let t2 = Instant::now();
    loop {
        match recv(&mut a) {
            Some(ServerMsg::Pong { t: 77 }) => break,
            Some(_) if t2.elapsed() < Duration::from_secs(2) => continue,
            m => panic!("{m:?}"),
        }
    }
}

#[test]
fn client_leaving_is_unregistered() {
    let r = rig("leave");
    {
        let (mut a, _) = join(&r);
        assert_eq!(r.run.clients(), 1);
        let _ = a.close(None);
        let _ = a.flush();
    }
    let t0 = Instant::now();
    while r.run.clients() != 0 && t0.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(r.run.clients(), 0);
}
