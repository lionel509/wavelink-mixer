//! Breaker round 1 for #36 (LAN control server). Red tests are real defects; green probes pin
//! behaviour that already holds. Everything runs on 127.0.0.1:0 with the fake backend and a temp config:
//! no PipeWire, no wpctl, no port 8790.
use super::backend::FakeBackend;
use super::server::{serve, Opts, Running};
use crate::config::{Bus, BusKind, Config};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
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

fn rig_with(tag: &str, edit: impl FnOnce(&mut Config)) -> Rig {
    let dir = std::env::temp_dir().join(format!("wavelink-b36-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let cfg_path = dir.join(".config/wavelink-mixer/config.json");
    let mut c = Config::default_config();
    c.buses.insert(
        2,
        Bus { id: "a3".into(), name: "Sonos".into(), color: "#ff9f0a".into(), kind: BusKind::Hardware { target: "raop_sonos_test".into() }, gain_db: -30.0, mute: false },
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

fn rig(tag: &str) -> Rig {
    rig_with(tag, |_| {})
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

fn recv_np(ws: &mut Ws) -> Option<ServerMsg> {
    loop {
        match recv(ws)? {
            ServerMsg::Meters { .. } => continue,
            m => return Some(m),
        }
    }
}

fn hello(ws: &mut Ws, token: &str, meters: bool) {
    send(ws, &ClientMsg::Hello(Hello { protocol: PROTOCOL_VERSION, token: token.into(), client: "breaker".into(), meters }));
}

fn join(r: &Rig) -> (Ws, State) {
    let mut ws = connect(r);
    hello(&mut ws, TOKEN, false);
    assert!(matches!(recv_np(&mut ws), Some(ServerMsg::Welcome { .. })));
    match recv_np(&mut ws) {
        Some(ServerMsg::State { state }) => (ws, state),
        m => panic!("expected state, got {m:?}"),
    }
}

/// Send one command and wait for its ack (deltas in between are skipped).
fn cmd(ws: &mut Ws, id: u64, c: Command) -> (bool, Option<String>) {
    send(ws, &ClientMsg::Command { id, cmd: c });
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(3) {
        match recv_np(ws) {
            Some(ServerMsg::Ack { id: i, ok, error, .. }) if i == id => return (ok, error),
            Some(_) => {}
            None => break,
        }
    }
    panic!("no ack for {id}");
}

fn bus_gain(r: &Rig, id: &str) -> f64 {
    Config::peek(&r.cfg).unwrap().buses.iter().find(|b| b.id == id).unwrap().gain_db
}

// ======================================================================= RED: safety

/// B36-1 (HIGH). The +6 dB/step limit is per *command*, with no time component, and `unlock` is a bare
/// per-command flag the server trusts from any client (there is no server-side unlock session, despite
/// the PR text). So the Sonos goes from its -20 dB cap to +12 dB (the speaker at well over 100 % of
/// the volume that was "painfully loud" at 0 dB) in a handful of back-to-back commands. The client's own
/// fader sends every 40 ms while dragging, so a single drag with "unlock" ticked does this in ~250 ms.
/// Expected: a raise above the cap is rate-limited in time (e.g. at most MAX_STEP_DB per second), so a
/// burst inside one second cannot add more than one step.
#[test]
fn b36_unlocked_burst_ramps_sonos_far_past_cap_within_a_second() {
    let r = rig("ramp-a3");
    let (mut ws, _) = join(&r);
    // get to the cap the normal way: -30 -> -24 -> -20
    for id in 1..=2 {
        assert!(cmd(&mut ws, id, Command::SetBusGain { bus: "a3".into(), db: -20.0, unlock: false }).0);
    }
    assert_eq!(bus_gain(&r, "a3"), -20.0);
    let t0 = Instant::now();
    for id in 10..18 {
        let _ = cmd(&mut ws, id, Command::SetBusGain { bus: "a3".into(), db: 12.0, unlock: true });
    }
    let took = t0.elapsed();
    let got = bus_gain(&r, "a3");
    assert!(
        got <= -20.0 + safety::MAX_STEP_DB,
        "a3 went from its -20 dB cap to {got:+.1} dB in {took:?} (8 unlocked commands); \
         the 6 dB step must be limited per unit of time, not per command"
    );
}

/// B36-2 (HIGH). Same hole on an uncapped physical bus (a1 = the Scarlett headphones): 13 commands in a
/// burst take it from -60 dB to +12 dB, i.e. the full range in well under a second. The "at most 6 dB
/// per command" rule is only a speed bump when commands are free and unthrottled.
#[test]
fn b36_burst_ramps_headphones_from_silence_to_max() {
    let r = rig_with("ramp-a1", |c| c.buses[0].gain_db = -60.0);
    let (mut ws, _) = join(&r);
    let t0 = Instant::now();
    for id in 1..=13 {
        let _ = cmd(&mut ws, id, Command::SetBusGain { bus: "a1".into(), db: 12.0, unlock: false });
    }
    let took = t0.elapsed();
    let got = bus_gain(&r, "a1");
    assert!(got <= -60.0 + 2.0 * safety::MAX_STEP_DB, "a1 went -60 -> {got:+.1} dB in {took:?}");
}

/// B36-3 (MEDIUM). AirPlay detection is case-sensitive (`contains("raop") || contains("airplay")`), so a
/// RAOP node named with capitals, e.g. by a node.name rule or a renamed sink, on any bus but a3 gets no
/// cap at all (and no cap means: unlock not even needed). PR #35 already does this case-insensitively
/// (`is_raop_name`), so the two PRs disagree on what is a speaker.
#[test]
fn b36_airplay_cap_detection_is_case_insensitive() {
    let none = Default::default();
    let mut b = Config::default_config().buses[0].clone();
    b.id = "a4".into();
    for t in ["AirPlay_Kitchen", "RAOP_sonos_kitchen", "raop_sink.Sonos-ABC.local"] {
        b.kind = BusKind::Hardware { target: t.into() };
        assert_eq!(super::settings::cap_for(&none, &b), Some(safety::SPEAKER_CAP_DB), "target {t} must be capped");
    }
}

// ======================================================================= RED: DoS (pre-auth)

/// B36-4 (MEDIUM). Unauthenticated clients get tungstenite's default limits (64 MiB message, 16 MiB
/// frame): one TCP connection can make the engine buffer 64 MiB before the token is even looked at, and
/// there is no connection cap, so a few dozen sockets from anything on the LAN/Meshnet OOM the engine
/// (which is also the audio routing engine). Expected: a small limit (a Hello is < 1 KiB; a command too)
/// via `accept_with_config`, so a frame header announcing 8 MiB is refused at once.
#[test]
fn b36_pre_auth_huge_frame_is_refused_promptly() {
    let r = rig("bigframe");
    let mut ws = connect(&r);
    let s = ws.get_mut();
    // FIN|text, MASK|127, 64-bit length = 8 MiB, mask key; then nothing (server must not wait for 8 MiB)
    let mut hdr = vec![0x81u8, 0x80 | 127];
    hdr.extend_from_slice(&(8u64 << 20).to_be_bytes());
    hdr.extend_from_slice(&[1, 2, 3, 4]);
    s.write_all(&hdr).unwrap();
    s.write_all(&vec![b'x' ^ 1; 64 * 1024]).unwrap();
    s.set_read_timeout(Some(Duration::from_millis(2000))).unwrap();
    let mut buf = [0u8; 256];
    let reacted = match s.read(&mut buf) {
        Ok(_) => true, // close frame or EOF
        Err(e) => !matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut),
    };
    assert!(reacted, "server is still waiting to buffer an 8 MiB pre-auth frame");
}

/// B36-5 (MEDIUM). HELLO_TIMEOUT is a per-read socket timeout, not a deadline: a client that never sends
/// Hello but sends a WebSocket ping every second (or trickles the HTTP upgrade / a frame one byte every
/// 4 s) holds its thread forever. With no connection cap that is a slowloris on the engine.
/// Expected: Hello within HELLO_TIMEOUT (5 s) of the TCP accept, whatever else arrives.
#[test]
fn b36_pre_auth_has_an_overall_hello_deadline() {
    let r = rig("slowloris");
    let mut ws = connect(&r);
    ws.get_ref().set_read_timeout(Some(Duration::from_millis(200))).unwrap();
    let t0 = Instant::now();
    let mut closed_at = None;
    while t0.elapsed() < Duration::from_secs(8) {
        if ws.send(Message::Ping(vec![1u8].into())).is_err() {
            closed_at = Some(t0.elapsed());
            break;
        }
        let tick = Instant::now();
        let mut gone = false;
        while tick.elapsed() < Duration::from_millis(1000) {
            match ws.read() {
                Ok(Message::Close(_)) | Err(tungstenite::Error::ConnectionClosed) | Err(tungstenite::Error::AlreadyClosed) => {
                    gone = true;
                    break;
                }
                Ok(_) => {}
                Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                Err(_) => {
                    gone = true;
                    break;
                }
            }
        }
        if gone {
            closed_at = Some(t0.elapsed());
            break;
        }
    }
    assert!(closed_at.is_some_and(|t| t < Duration::from_secs(7)), "pre-auth connection still open after 8 s of pings (closed_at {closed_at:?})");
}

// ======================================================================= RED: token file

/// B36-6 (LOW). First-run token creation is not exclusive: two concurrent `load_or_create` calls (the
/// engine starting while `wavelink-mixer remote-token` runs, or a restart overlap) each generate their
/// own token and the last rename wins, so one process keeps a token that is not in the file (the engine
/// then refuses the token `remote-token` printed until it restarts). Same-process threads also share the
/// `remote.json.tmp.<pid>` name. Expected: create with O_EXCL (or under a lock) and re-read.
#[test]
fn b36_concurrent_first_run_agrees_on_one_token() {
    for round in 0..20 {
        let dir = std::env::temp_dir().join(format!("wavelink-b36-token-{}-{round}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = dir.join("remote.json");
        let bar = Arc::new(std::sync::Barrier::new(6));
        let hs: Vec<_> = (0..6)
            .map(|_| {
                let (p, bar) = (p.clone(), bar.clone());
                std::thread::spawn(move || {
                    bar.wait();
                    super::settings::load_or_create(&p).map(|c| c.token).map_err(|e| e.to_string())
                })
            })
            .collect();
        let got: Vec<_> = hs.into_iter().map(|h| h.join().unwrap()).collect();
        let file = super::settings::load_or_create(&p).map(|c| c.token).unwrap_or_default();
        let _ = std::fs::remove_dir_all(&dir);
        for g in &got {
            assert_eq!(g.as_deref().ok(), Some(file.as_str()), "round {round}: a first-run caller got {g:?}, the file holds {file}");
        }
    }
}

// ======================================================================= GREEN probes

/// Commands, subscribes and pings before Hello never reach the backend or the file.
#[test]
fn b36_probe_nothing_before_hello() {
    let r = rig("prehello");
    let before = std::fs::read(&r.cfg).unwrap();
    for first in [
        ClientMsg::Command { id: 1, cmd: Command::SetBusGain { bus: "a3".into(), db: 12.0, unlock: true } },
        ClientMsg::Subscribe { meters: true },
    ] {
        let mut ws = connect(&r);
        send(&mut ws, &first);
        assert!(matches!(recv(&mut ws), Some(ServerMsg::Error { code: ErrorCode::BadRequest, .. })));
        assert!(recv(&mut ws).is_none(), "connection closed after the refusal");
    }
    // a binary frame first: refused too
    let mut ws = connect(&r);
    ws.send(Message::Binary(br#"{"type":"hello"}"#.to_vec().into())).unwrap();
    assert!(matches!(recv(&mut ws), Some(ServerMsg::Error { code: ErrorCode::BadRequest, .. })));
    std::thread::sleep(Duration::from_millis(300));
    assert!(r.fake.applied.lock().unwrap().is_empty());
    assert_eq!(r.fake.meter_calls.load(Ordering::SeqCst), 0);
    assert_eq!(std::fs::read(&r.cfg).unwrap(), before);
}

/// Empty, whitespace, prefix, 100 KB and NUL-padded tokens are refused; a wrong token costs >= 400 ms.
#[test]
fn b36_probe_token_edge_cases() {
    let r = rig("tokens");
    let long = "a".repeat(100_000);
    let prefix = &TOKEN[..16];
    let nul = format!("{TOKEN}\0");
    for t in ["", "   ", prefix, long.as_str(), nul.as_str()] {
        let mut ws = connect(&r);
        let t0 = Instant::now();
        hello(&mut ws, t, false);
        assert!(matches!(recv(&mut ws), Some(ServerMsg::Error { code: ErrorCode::Auth, .. })), "token {:?}", &t[..t.len().min(20)]);
        assert!(t0.elapsed() >= Duration::from_millis(390));
    }
    // a second Hello after a good one is refused and does not re-auth anything
    let (mut ws, _) = join(&r);
    hello(&mut ws, "wrong", false);
    assert!(matches!(recv_np(&mut ws), Some(ServerMsg::Error { code: ErrorCode::BadRequest, .. })));
    assert!(cmd(&mut ws, 1, Command::SetInputMute { input: "mic".into(), mute: true }).0, "still the authed session");
}

/// Ids are only ever looked up, never used as paths or shell words: traversal / shell metachar ids are
/// refused with nothing written or applied (the remote can't create ids; wpctl gets numeric node ids).
#[test]
fn b36_probe_hostile_ids_are_refused() {
    let r = rig("ids");
    let (mut ws, _) = join(&r);
    let before = std::fs::read(&r.cfg).unwrap();
    let bad = ["../../../../etc/passwd", "a3; rm -rf ~", "$(reboot)", "a3\0", "A3", "a3 ", ""];
    let mut id = 0;
    for b in bad {
        id += 1;
        let (ok, err) = cmd(&mut ws, id, Command::SetBusGain { bus: b.into(), db: 0.0, unlock: true });
        assert!(!ok && err.unwrap().contains("unknown bus"), "{b:?}");
        id += 1;
        let (ok, _) = cmd(&mut ws, id, Command::SetRoute { input: "spotify".into(), bus: b.into(), on: Some(true), level_db: None });
        assert!(!ok, "{b:?}");
        id += 1;
        let (ok, _) = cmd(&mut ws, id, Command::SetInputGain { input: b.into(), db: 0.0 });
        assert!(!ok, "{b:?}");
    }
    assert!(r.fake.applied.lock().unwrap().is_empty());
    assert_eq!(std::fs::read(&r.cfg).unwrap(), before);
}

/// Without unlock, no burst gets a3 above its cap, and NaN/inf are refused (JSON can't carry them, so
/// they arrive as 1e999 -> parse error, or as huge finite values that clamp).
#[test]
fn b36_probe_locked_burst_never_passes_cap() {
    let r = rig("lockedburst");
    let (mut ws, _) = join(&r);
    for id in 1..40 {
        let _ = cmd(&mut ws, id, Command::SetBusGain { bus: "a3".into(), db: 1e300, unlock: false });
    }
    assert_eq!(bus_gain(&r, "a3"), -20.0);
    // a raw frame with 1e999 (inf in JSON) is a parse error, not a command
    ws.send(Message::text(r#"{"type":"command","id":99,"cmd":{"op":"set_bus_gain","bus":"a3","db":1e999,"unlock":true}}"#)).unwrap();
    assert!(matches!(recv_np(&mut ws), Some(ServerMsg::Error { code: ErrorCode::BadRequest, .. })));
    assert_eq!(bus_gain(&r, "a3"), -20.0);
    assert!(r.fake.applied.lock().unwrap().iter().all(|c| !matches!(c, Command::SetBusGain { bus, db, .. } if bus == "a3" && *db > -20.0)));
}

/// Unmuting a3 that the GUI left muted at -5 dB lowers the gain *before* the unmute reaches the graph.
#[test]
fn b36_probe_unmute_lowers_first() {
    let r = rig_with("unmute", |c| {
        let a3 = c.buses.iter_mut().find(|b| b.id == "a3").unwrap();
        a3.gain_db = -5.0;
        a3.mute = true;
    });
    let (mut ws, _) = join(&r);
    assert!(cmd(&mut ws, 1, Command::SetBusMute { bus: "a3".into(), mute: false }).0);
    let ap = r.fake.applied.lock().unwrap().clone();
    assert_eq!(
        ap,
        vec![Command::SetBusGain { bus: "a3".into(), db: -20.0, unlock: false }, Command::SetBusMute { bus: "a3".into(), mute: false }]
    );
    assert_eq!(bus_gain(&r, "a3"), -20.0);
}

/// Meter taps are shared: 12 subscribed clients cost one backend poll per frame (~15/s), not 12x, and the
/// taps stop once the last subscriber leaves.
#[test]
fn b36_probe_meters_are_shared_and_stop() {
    let r = rig("meters");
    let mut socks = vec![];
    for _ in 0..12 {
        let (mut ws, _) = join(&r);
        send(&mut ws, &ClientMsg::Subscribe { meters: true });
        socks.push(ws);
    }
    let n0 = r.fake.meter_calls.load(Ordering::SeqCst);
    std::thread::sleep(Duration::from_secs(2));
    let per_s = (r.fake.meter_calls.load(Ordering::SeqCst) - n0) as f64 / 2.0;
    assert!(per_s < 20.0, "{per_s} backend meter polls per second with 12 subscribers");
    for ws in socks.iter_mut() {
        send(ws, &ClientMsg::Subscribe { meters: false });
    }
    std::thread::sleep(Duration::from_millis(300));
    assert!(r.fake.stopped.load(Ordering::SeqCst) >= 1, "taps stopped after the last unsubscribe");
}

/// A taken port makes serve() fail (the engine logs it and keeps routing audio).
#[test]
fn b36_probe_port_taken_fails_cleanly() {
    let hold = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = hold.local_addr().unwrap().port();
    let dir = std::env::temp_dir().join(format!("wavelink-b36-port-{}", std::process::id()));
    let p = dir.join("config.json");
    assert!(Config::default_config().save_to(&p));
    let r = serve(
        Opts { bind: "127.0.0.1".into(), port, token: TOKEN.into(), cfg_path: p, name: "x".into(), caps: Default::default() },
        Arc::new(FakeBackend::default()),
    );
    assert!(r.is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

// ======================================================================= GUI merge probes (document behaviour)

/// Probe (documents B36-8): `save_merged` lets the window's in-memory gain beat a remote gain on the same
/// input. That is the stated policy, but auto-normalize moves the in-memory gain without the user, so on
/// the GUI's save path (any dirty tick, or the once-a-minute normalize save) a remote fader move on an
/// N-enabled input is overwritten on disk. `pull_cfg` has a special case for exactly this; `save_cfg`
/// does not (it calls `apply_external(&before, None)`), and save wins over pull on ticks % 10.
#[test]
fn b36_probe_save_merged_auto_gain_beats_remote_gain() {
    let dir = std::env::temp_dir().join(format!("wavelink-b36-merge-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let p = dir.join("config.json");
    let mut base = Config::default_config();
    base.inputs[1].normalize = true;
    assert!(base.save_to(&p));
    let mut gui = base.clone();
    gui.inputs[1].gain_db = -8.0; // auto-normalize nudged the slider (not saved yet: norm_moved)
    gui.inputs[0].eq[2] = 3.0; // he touches an EQ knob -> dirty -> save on the next %5 tick
    let mut remote = Config::peek(&p).unwrap();
    remote.inputs[1].gain_db = -30.0; // the remote pulls Spotify down
    assert!(remote.save_to(&p));
    gui.save_merged(&mut base, &p);
    let disk = Config::peek(&p).unwrap();
    assert_eq!(disk.inputs[1].gain_db, -8.0, "current behaviour: the remote's -30 dB is lost");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Probe: deleting a channel in the GUI while the remote edits it resurrects the channel on merge
/// (edit-beats-delete). Defensible, but it means a deleted strip can come back with a stale config.
#[test]
fn b36_probe_merge_resurrects_deleted_channel_edited_remotely() {
    let base = Config::default_config();
    let mut mine = base.clone();
    let gone = mine.inputs.remove(1).id;
    let mut theirs = base.clone();
    theirs.inputs[1].gain_db = -12.0;
    let m = crate::config::merge3(&base, &mine, &theirs).unwrap();
    assert!(m.inputs.iter().any(|i| i.id == gone), "current behaviour: '{gone}' comes back");
}
