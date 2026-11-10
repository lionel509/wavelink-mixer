//! Server tests: a real WebSocket server on 127.0.0.1:0 with a fake graph backend and a config in a temp dir.
use super::backend::FakeBackend;
use super::server::{serve, Opts, Running};
use crate::config::{Bus, BusKind, Config};
use crate::pw;
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
    send(ws, &ClientMsg::Hello(Hello { protocol: PROTOCOL_VERSION, token: token.into(), client: "test".into(), meters, host: None }));
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
    send(&mut ws, &ClientMsg::Hello(Hello { protocol: PROTOCOL_VERSION + 1, token: TOKEN.into(), client: String::new(), meters: false, host: None }));
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
    std::thread::sleep(super::server::PREAUTH_IDLE + Duration::from_millis(300)); // idle: 1 s past their handshake
    let mut newer = connect(&r);
    std::thread::sleep(Duration::from_millis(200));
    assert!(!alive(&mut idle[0]), "the oldest idle pending socket was closed to make room");
    assert!(alive(&mut idle[1]) && alive(&mut newer), "the others are still pending");
    // and a real client always gets in, however many silent sockets came first
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
    let ip = |d: u8| IpAddr::V4(Ipv4Addr::new(192, 168, 1, d));
    let (a, b, c, real, lo) = (ip(50), ip(60), ip(61), ip(20), IpAddr::V4(Ipv4Addr::LOCALHOST));
    let idle = PREAUTH_IDLE * 3;
    let fresh = Duration::from_millis(5);
    // per-IP cap: the IP makes room by losing its own oldest socket (idle first), never someone else's
    assert_eq!(admit_policy(&[(a, idle * 2), (a, idle)], a), Admit::Evict(0));
    assert_eq!(admit_policy(&[(a, fresh), (a, idle)], a), Admit::Evict(1));
    assert_eq!(admit_policy(&[(a, fresh), (a, fresh)], b), Admit::Ok);
    // loopback is exempt from the per-IP cap
    assert_eq!(admit_policy(&[(lo, fresh), (lo, fresh)], lo), Admit::Ok);
    // overall cap: silent sockets from 2 hosts never lock out a third
    let full = vec![(a, idle * 4), (a, idle * 3), (b, idle * 2), (b, idle)];
    assert_eq!(full.len(), MAX_PREAUTH);
    assert_eq!(admit_policy(&full, lo), Admit::Evict(0));
    // R3-1: a real client mid-handshake (its only socket) is never the victim while an IP holds 2
    let p = [(real, Duration::ZERO), (a, fresh), (a, fresh), (b, fresh)];
    assert!(matches!(admit_policy(&p, b), Admit::Evict(1) | Admit::Evict(2)));
    // ... not even when it is idle-looking (handshake done long ago) and the flooder's are fresh
    let p = [(real, idle), (a, fresh), (a, fresh), (b, fresh)];
    assert!(matches!(admit_policy(&p, c), Admit::Evict(1) | Admit::Evict(2)));
    // R3-2: a newcomer from an empty IP is never refused while some IP holds 2: a full IP loses one
    let p: Vec<_> = (0..4u8).flat_map(|k| [(ip(70 + k), fresh), (ip(70 + k), fresh)]).collect();
    assert_eq!(p.len(), 2 * MAX_PREAUTH);
    assert!(matches!(admit_policy(&p, real), Admit::Evict(_)));
    // everyone holds a single fresh socket: nobody is evicted, the newcomer waits in the overflow
    let p = [(a, fresh), (b, fresh), (c, fresh), (real, fresh)];
    assert_eq!(admit_policy(&p, ip(99)), Admit::Ok);
    // ... but an idle single socket does make room
    let p = [(a, fresh), (b, idle), (c, fresh), (real, fresh)];
    assert_eq!(admit_policy(&p, ip(99)), Admit::Evict(1));
    // a local loopback flood of fresh sockets: overflow, then refused (accepted: same-user processes have the token)
    let busy: Vec<_> = (0..MAX_PREAUTH).map(|_| (lo, fresh)).collect();
    assert_eq!(admit_policy(&busy, lo), Admit::Ok);
    let flood: Vec<_> = (0..2 * MAX_PREAUTH).map(|_| (lo, fresh)).collect();
    assert_eq!(admit_policy(&flood, lo), Admit::Refuse);
    // ... while a LAN client is still let in past it
    assert!(matches!(admit_policy(&flood, real), Admit::Evict(_)));
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

// ---------------------------------------------------------------- hosts (#37): a MacBook's own system volume

/// Greet as a host (a computer that owns its system volume).
fn hello_host(ws: &mut Ws, id: &str, label: &str, volume: u8, mute: bool) {
    let host = HostHello { id: id.into(), label: label.into(), volume, mute };
    send(ws, &ClientMsg::Hello(Hello { protocol: PROTOCOL_VERSION, token: TOKEN.into(), client: "test-host".into(), meters: false, host: Some(host) }));
}

/// Wait for the welcome and the first snapshot of a host connection.
fn join_host(r: &Rig, id: &str, label: &str, volume: u8, mute: bool) -> Ws {
    let mut ws = connect(r);
    hello_host(&mut ws, id, label, volume, mute);
    assert!(matches!(recv_np(&mut ws), Some(ServerMsg::Welcome { .. })));
    assert!(matches!(recv_np(&mut ws), Some(ServerMsg::State { .. })));
    ws
}

/// Read until a delta matches `want` (other traffic is skipped), up to 3 s.
fn wait_delta(ws: &mut Ws, want: impl Fn(&Delta) -> bool) -> Option<Delta> {
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(3) {
        match recv_np(ws) {
            Some(ServerMsg::Delta { changes, .. }) => {
                if let Some(d) = changes.into_iter().find(|d| want(d)) {
                    return Some(d);
                }
            }
            Some(_) => {}
            None => break,
        }
    }
    None
}

/// The first error a socket reports (skipping anything else), if any.
fn expect_error(ws: &mut Ws) -> Option<(ErrorCode, String)> {
    for _ in 0..8 {
        match recv_np(ws)? {
            ServerMsg::Error { code, message } => return Some((code, message)),
            _ => continue,
        }
    }
    None
}

#[test]
fn host_hello_registers_online_and_leaving_marks_offline() {
    let r = rig("host-online");
    let (mut watcher, _) = join(&r);
    let mut host = join_host(&r, "macbook", "MacBook", 20, false);
    let (_, st) = join(&r);
    assert_eq!(st.hosts.len(), 1, "{:?}", st.hosts);
    assert_eq!(st.hosts[0].id, "macbook");
    assert!(st.hosts[0].online);
    assert_eq!(st.hosts[0].volume, 20);

    let _ = host.close(None);
    let _ = host.flush();
    let went = wait_delta(&mut watcher, |d| matches!(d, Delta::HostOnline { online: false, .. }));
    assert_eq!(went, Some(Delta::HostOnline { host: "macbook".into(), online: false }));
}

#[test]
fn host_volume_command_changes_state_only() {
    let r = rig("host-volume");
    let mut host = join_host(&r, "macbook", "MacBook", 50, false);
    let (mut c, _) = join(&r);
    let cfg_before = std::fs::read(&r.cfg).unwrap();

    let (ok, _err, note, deltas) = cmd(&mut c, 1, Command::SetHostVolume { host: "macbook".into(), volume: 90 });
    assert!(ok);
    assert_eq!(note.as_deref(), Some("raised at most 25 per step"));
    assert!(deltas.contains(&Delta::HostVolume { host: "macbook".into(), volume: 75 }), "{deltas:?}");
    // the Mac itself hears about it (it applies the volume to CoreAudio from this delta)
    assert_eq!(wait_delta(&mut host, |d| matches!(d, Delta::HostVolume { .. })), Some(Delta::HostVolume { host: "macbook".into(), volume: 75 }));

    // host changes never touch config.json or the PipeWire backend
    assert_eq!(std::fs::read(&r.cfg).unwrap(), cfg_before);
    assert!(r.fake.applied.lock().unwrap().is_empty());

    // a later config edit republishes the state: the host must survive it
    let input = Config::default_config().inputs[0].id.clone();
    let (ok, _, _, _) = cmd(&mut c, 2, Command::SetInputGain { input, db: -3.0 });
    assert!(ok);
    let (_, st) = join(&r);
    assert_eq!(st.hosts.len(), 1, "a config edit dropped the host: {:?}", st.hosts);
    assert_eq!(st.hosts[0].volume, 75);
}

#[test]
fn host_state_report_overrides_and_broadcasts() {
    let r = rig("host-report");
    let mut host = join_host(&r, "macbook", "MacBook", 50, false);
    let (mut c, _) = join(&r);
    send(&mut host, &ClientMsg::HostState { volume: 12, mute: true });
    // both changes arrive in one Delta frame: collect across frames (wait_delta would drop the second)
    let mut seen: Vec<Delta> = vec![];
    let t0 = Instant::now();
    while seen.len() < 2 && t0.elapsed() < Duration::from_secs(3) {
        if let Some(ServerMsg::Delta { changes, .. }) = recv_np(&mut c) {
            seen.extend(changes);
        }
    }
    assert!(seen.contains(&Delta::HostVolume { host: "macbook".into(), volume: 12 }), "{seen:?}");
    assert!(seen.contains(&Delta::HostMute { host: "macbook".into(), mute: true }), "{seen:?}");
    let (_, st) = join(&r);
    assert_eq!((st.hosts[0].volume, st.hosts[0].mute), (12, true));
}

#[test]
fn newest_registration_owns_the_host() {
    let r = rig("host-newest");
    let mut old = join_host(&r, "macbook", "MacBook", 30, false);
    let _new = join_host(&r, "macbook", "MacBook", 31, false);
    let _ = old.close(None);
    let _ = old.flush();
    std::thread::sleep(Duration::from_millis(300));
    let (_, st) = join(&r);
    assert_eq!(st.hosts.len(), 1);
    assert!(st.hosts[0].online, "the older socket leaving must not mark the newer one offline");
}

#[test]
fn non_host_connection_cannot_report_host_state() {
    let r = rig("host-plain");
    let (mut c, _) = join(&r);
    send(&mut c, &ClientMsg::HostState { volume: 10, mute: false });
    assert!(matches!(expect_error(&mut c), Some((ErrorCode::BadRequest, _))));
}

#[test]
fn bad_host_id_is_refused() {
    let r = rig("host-badid");
    let mut ws = connect(&r);
    hello_host(&mut ws, "Mac Book!", "MacBook", 20, false);
    match expect_error(&mut ws) {
        Some((ErrorCode::BadRequest, msg)) => assert!(msg.contains("bad host id"), "{msg}"),
        m => panic!("expected BadRequest, got {m:?}"),
    }
}

// ---- #44 U2: hub side of the full window (Hello.full, SaveConfig, Pw, Config/Graph snapshots)

fn hello_full(ws: &mut Ws) {
    send(ws, &ClientMsg::Hello(Hello { protocol: PROTOCOL_VERSION, token: TOKEN.into(), client: "test".into(), meters: false, host: None, full: true }));
}

/// Everything that arrives within `wait`.
fn collect(ws: &mut Ws, wait: Duration) -> Vec<ServerMsg> {
    ws.get_ref().set_read_timeout(Some(wait)).unwrap();
    let mut out = vec![];
    while let Ok(Message::Text(t)) = ws.read() {
        out.push(ServerMsg::from_json(t.as_str()).unwrap());
    }
    ws.get_ref().set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    out
}

/// The Ack for command `id`, skipping other traffic.
fn ack_for(ws: &mut Ws, id: u64) -> (bool, Option<String>, Option<String>) {
    loop {
        match recv_np(ws) {
            Some(ServerMsg::Ack { id: i, ok, error, note }) if i == id => return (ok, error, note),
            Some(_) => continue,
            None => panic!("no ack for {id}"),
        }
    }
}

fn graph_fixture(cookie: u64) -> pw::GraphWire {
    let node = |id: u64, name: &str| pw::Node { id, serial: id + 100, name: name.into(), ..Default::default() };
    pw::GraphWire { nodes: vec![node(1, "hp"), node(2, "spotify")], links: vec![(2, 1)], cookie }
}

fn graph_msgs(seen: &[ServerMsg]) -> usize {
    seen.iter().filter(|m| matches!(m, ServerMsg::Graph { .. })).count()
}

#[test]
fn full_hello_gets_config_then_graph_snapshots() {
    let r = rig("full");
    *r.fake.graph.lock().unwrap() = Some(graph_fixture(7));
    let mut ws = connect(&r);
    hello_full(&mut ws);
    let msgs = collect(&mut ws, Duration::from_millis(400));
    let kinds: Vec<&str> = msgs
        .iter()
        .map(|m| match m {
            ServerMsg::Welcome { .. } => "welcome",
            ServerMsg::State { .. } => "state",
            ServerMsg::Config { .. } => "config",
            ServerMsg::Graph { .. } => "graph",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, ["welcome", "state", "config", "graph"], "{kinds:?}");
    let want_cfg = serde_json::to_value(Config::peek(&r.cfg).unwrap()).unwrap();
    match &msgs[2] {
        ServerMsg::Config { config } => assert_eq!(config, &want_cfg),
        m => panic!("{m:?}"),
    }
    let want_graph = serde_json::to_value(graph_fixture(7)).unwrap();
    match &msgs[3] {
        ServerMsg::Graph { graph } => assert_eq!(graph, &want_graph),
        m => panic!("{m:?}"),
    }
}

#[test]
fn plain_hello_never_gets_config_or_graph() {
    // I2: old clients (console.rs, client/) never see config or graph, even while both change
    let r = rig("plain");
    *r.fake.graph.lock().unwrap() = Some(graph_fixture(7));
    let (mut a, _) = join(&r);
    let mut p = connect(&r);
    hello(&mut p, TOKEN, false);
    let first = collect(&mut p, Duration::from_millis(300));
    assert!(matches!(first.first(), Some(ServerMsg::Welcome { .. })), "{first:?}");
    *r.fake.graph.lock().unwrap() = Some(graph_fixture(8));
    let (ok, _, _, _) = cmd(&mut a, 1, Command::SetInputGain { input: "spotify".into(), db: -9.5 });
    assert!(ok);
    let later = collect(&mut p, Duration::from_millis(1200));
    let all: Vec<&ServerMsg> = first.iter().chain(later.iter()).collect();
    assert!(all.iter().all(|m| !matches!(m, ServerMsg::Config { .. } | ServerMsg::Graph { .. })), "{all:?}");
    assert!(all.iter().any(|m| matches!(m, ServerMsg::Delta { .. })), "the plain client still gets deltas");
}

#[test]
fn save_config_merges_three_way_and_publishes() {
    let r = rig("save3");
    let (mut a, _) = join(&r);
    let mut f = connect(&r);
    hello_full(&mut f);
    let base = Config::peek(&r.cfg).unwrap();
    // mine renames spotify; meanwhile disk gets a new gain for spotify from elsewhere
    let mut mine = base.clone();
    mine.inputs.iter_mut().find(|i| i.id == "spotify").unwrap().name = "Renamed".into();
    let mut disk = Config::load_from(&r.cfg);
    disk.inputs.iter_mut().find(|i| i.id == "spotify").unwrap().gain_db = -9.5;
    assert!(disk.save_to(&r.cfg));
    send(&mut a, &ClientMsg::SaveConfig { id: 5, base: serde_json::to_value(&base).unwrap(), mine: serde_json::to_value(&mine).unwrap() });
    let (ok, err, _) = ack_for(&mut a, 5);
    assert!(ok, "{err:?}");
    let saved = Config::peek(&r.cfg).unwrap();
    let sp = saved.inputs.iter().find(|i| i.id == "spotify").unwrap();
    assert_eq!(sp.name, "Renamed", "my edit kept");
    assert_eq!(sp.gain_db, -9.5, "their edit kept");
    // the full client hears the merged file
    let got = collect(&mut f, Duration::from_millis(600));
    let pushed = got.iter().rev().find_map(|m| match m {
        ServerMsg::Config { config } => Some(config.clone()),
        _ => None,
    });
    assert_eq!(pushed, Some(serde_json::to_value(&saved).unwrap()));
}

#[test]
fn save_config_caps_bus_raises_like_set_bus_gain() {
    // I1, generated: a SaveConfig that raises a bus leaves the same gain (or refusal) as SetBusGain
    // from the same connection would. The rig's a3 is a speaker capped at -20 dB.
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for i in 0..50u64 {
        let start = -30.0 + (next() % 101) as f64 / 10.0; // -30.0 ..= -20.0
        let want = -60.0 + (next() % 721) as f64 / 10.0; // -60.0 ..= +12.0
        let gain_of = |c: &mut Config, db: f64| c.buses.iter_mut().find(|b| b.id == "a3").unwrap().gain_db = db;
        let r1 = rig_with(&format!("capA{i}"), |c| gain_of(c, start));
        let r2 = rig_with(&format!("capB{i}"), |c| gain_of(c, start));
        let (mut a, _) = join(&r1);
        let (cmd_ok, _, _, _) = cmd(&mut a, 1, Command::SetBusGain { bus: "a3".into(), db: want, unlock: false });
        let want_gain = Config::peek(&r1.cfg).unwrap().buses.iter().find(|b| b.id == "a3").unwrap().gain_db;

        let (mut b, _) = join(&r2);
        let base = Config::peek(&r2.cfg).unwrap();
        let mut mine = base.clone();
        gain_of(&mut mine, want);
        send(&mut b, &ClientMsg::SaveConfig { id: 1, base: serde_json::to_value(&base).unwrap(), mine: serde_json::to_value(&mine).unwrap() });
        let (save_ok, _, _) = ack_for(&mut b, 1);
        assert_eq!(save_ok, cmd_ok, "case {i}: start {start} want {want}");
        let got = Config::peek(&r2.cfg).unwrap().buses.iter().find(|b| b.id == "a3").unwrap().gain_db;
        assert_eq!(got, want_gain, "case {i}: start {start} want {want}");
    }
}

#[test]
fn pw_volume_on_a_bus_node_goes_through_safety() {
    // I1: a bus's live node is written through the same caps as SetBusGain, never raw
    let r = rig("pwbus");
    r.fake.bus_nodes.lock().unwrap().insert(7, "a3".into());
    let (mut a, _) = join(&r);
    send(&mut a, &ClientMsg::Pw { id: 3, cmd: PwCmd::SetVolume { id: 7, v: 2.0 } });
    let (ok, _, _) = ack_for(&mut a, 3);
    assert!(ok);
    assert!(r.fake.pw_log.lock().unwrap().is_empty(), "no raw pw write for a bus node");
    assert!(r.fake.applied.lock().unwrap().iter().any(|c| matches!(c, Command::SetBusGain { bus, .. } if bus == "a3")));
}

#[test]
fn pw_on_a_plain_node_is_applied_raw() {
    let r = rig("pwraw");
    let (mut a, _) = join(&r);
    let eq = vec![0.0, -3.0, 2.5];
    send(&mut a, &ClientMsg::Pw { id: 4, cmd: PwCmd::SetEq { id: 9, eq: eq.clone() } });
    let (ok, _, _) = ack_for(&mut a, 4);
    assert!(ok);
    assert_eq!(r.fake.pw_log.lock().unwrap().as_slice(), &[PwCmd::SetEq { id: 9, eq }]);
}

#[test]
fn graph_thread_sends_only_on_change() {
    let r = rig("graphchg");
    *r.fake.graph.lock().unwrap() = Some(graph_fixture(7));
    let mut f = connect(&r);
    hello_full(&mut f);
    let mut seen = collect(&mut f, Duration::from_millis(1200)); // hello's snapshot
    *r.fake.graph.lock().unwrap() = Some(graph_fixture(7)); // same value: no send
    seen.extend(collect(&mut f, Duration::from_millis(1200)));
    *r.fake.graph.lock().unwrap() = Some(graph_fixture(8)); // changed: one send
    seen.extend(collect(&mut f, Duration::from_millis(1200)));
    assert_eq!(graph_msgs(&seen), 2, "hello snapshot + one change");
}
