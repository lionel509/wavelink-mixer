//! Breaker round 1, output side (#15). Each test pins one suspected break in swyh.rs.
//! Tests bind 127.0.0.1 port 0 only and send no multicast. Tests that touch PATH or the
//! global client slot counter run under one lock.
use super::*;
use crate::config::Config;
use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Mutex, MutexGuard};

static LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn wait_for(secs: u64, mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + Duration::from_secs(secs);
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

struct Srv {
    port: u16,
    allowed: Arc<Mutex<Vec<(String, String)>>>,
    stop: Arc<AtomicBool>,
    th: Option<std::thread::JoinHandle<()>>,
}

fn srv(ids: &[&str]) -> Srv {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let allowed = Arc::new(Mutex::new(ids.iter().map(|i| (i.to_string(), "Cast".to_string())).collect::<Vec<_>>()));
    let stop = Arc::new(AtomicBool::new(false));
    let (a, s) = (allowed.clone(), stop.clone());
    let th = std::thread::spawn(move || serve(l, a, s));
    Srv { port, allowed, stop, th: Some(th) }
}

impl Drop for Srv {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.th.take() {
            let _ = t.join();
        }
    }
}

struct Fake {
    dir: PathBuf,
    old: Option<std::ffi::OsString>,
}

/// Puts a fake `pw-record` first on PATH. PATH is only this directory, so with `body == None` the binary is missing.
fn fake_pw(tag: &str, body: Option<&str>) -> Fake {
    let dir = std::env::temp_dir().join(format!("breaker15-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    if let Some(b) = body {
        let p = dir.join("pw-record");
        std::fs::write(&p, format!("#!/bin/sh\n{b}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let old = std::env::var_os("PATH");
    std::env::set_var("PATH", &dir);
    Fake { dir, old }
}

impl Drop for Fake {
    fn drop(&mut self) {
        match self.old.take() {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A tap that writes its pid and then never writes audio.
const SILENT: &str = r#"echo $$ > "${0%/*}/pid"
exec /bin/sleep 600"#;

fn get(port: u16, path: &str) -> TcpStream {
    let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(8))).unwrap();
    write!(c, "GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    c
}

fn read_head(c: &mut TcpStream) -> String {
    let mut out = Vec::new();
    let mut b = [0u8; 1];
    while !out.ends_with(b"\r\n\r\n") {
        match c.read(&mut b) {
            Ok(1) => out.push(b[0]),
            _ => break,
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn pid_of(dir: &Path) -> Option<String> {
    wait_for(5, || dir.join("pid").exists());
    std::fs::read_to_string(dir.join("pid")).ok().map(|s| s.trim().to_string())
}

fn alive(pid: &str) -> bool {
    Command::new("/bin/kill")
        .args(["-0", pid])
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Kills one specific pid (never by pattern) so the stuck handler thread can exit.
fn kill_pid(pid: &str) {
    let _ = Command::new("/bin/kill").args(["-9", pid]).status();
}

/// Connection drops while the tap is silent: the handler is blocked in `out.read` and
/// never sees the dead socket, so the client slot and the pw-record child stay held.
#[test]
fn breaker_silent_tap_client_disconnect_frees_slot() {
    let _g = lock();
    let fake = fake_pw("silent", Some(SILENT));
    let s = srv(&["n1"]);
    let mut c = get(s.port, "/stream/n1.wav");
    let head = read_head(&mut c);
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let pid = pid_of(&fake.dir).expect("pw-record pid");
    assert_eq!(CLIENTS.load(Ordering::SeqCst), 1);
    drop(c);
    let freed = wait_for(10, || CLIENTS.load(Ordering::SeqCst) == 0);
    let still = alive(&pid);
    kill_pid(&pid);
    wait_for(5, || CLIENTS.load(Ordering::SeqCst) == 0);
    assert!(freed, "client disconnected but its slot is still held; pw-record pid {pid} alive={still}");
    assert!(!still, "pw-record pid {pid} still running after its client left");
}

/// The bus is removed (allowed list emptied, as `Out::tick` does) while the tap is silent.
/// `still_allowed` is only checked after a write, so the stream stays open indefinitely.
#[test]
fn breaker_removed_bus_closes_silent_stream() {
    let _g = lock();
    let fake = fake_pw("removed", Some(SILENT));
    let s = srv(&["n1"]);
    let mut c = get(s.port, "/stream/n1.wav");
    assert!(read_head(&mut c).starts_with("HTTP/1.1 200"));
    let pid = pid_of(&fake.dir).expect("pw-record pid");
    s.allowed.lock().unwrap().clear();
    c.set_read_timeout(Some(Duration::from_secs(6))).unwrap();
    let mut b = [0u8; 4096];
    let closed = loop {
        match c.read(&mut b) {
            Ok(0) => break true,
            Ok(_) => continue,
            Err(_) => break false,
        }
    };
    kill_pid(&pid);
    wait_for(5, || CLIENTS.load(Ordering::SeqCst) == 0);
    assert!(closed, "bus removed but the HTTP stream is still open 6 s later (pw-record pid {pid})");
}

/// pw-record cannot start. The handler has already sent `200 OK` and the WAV header, so the
/// renderer is told a stream exists when there is nothing behind it.
#[test]
fn breaker_missing_pw_record_is_not_reported_as_stream() {
    let _g = lock();
    let _fake = fake_pw("missing", None);
    let s = srv(&["n1"]);
    let mut c = get(s.port, "/stream/n1.wav");
    let head = read_head(&mut c);
    assert!(!head.starts_with("HTTP/1.1 200"), "pw-record is missing but the client got: {head:?}");
}

/// The description picks the control endpoint. Nothing ties it to the renderer's own host, so
/// SetAVTransportURI (which carries the PC's stream URL) can go to any host the device names.
#[test]
fn breaker_control_url_stays_on_renderer_host() {
    const DEV: &str = r#"<?xml version="1.0"?>
<root><device>
<friendlyName>Den TV</friendlyName>
<UDN>uuid:den-1</UDN>
<serviceList><service><serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType><controlURL>{CTL}</controlURL></service></serviceList>
</device></root>"#;
    let loc = "http://192.168.1.50:49152/desc.xml";
    let abs = DEV.replace("{CTL}", "http://203.0.113.9:8080/upnp/control/AVTransport1");
    let f = parse_description(&abs, loc).expect("renderer with AVTransport");
    assert_eq!(host_of(&f.control), "192.168.1.50", "absolute controlURL leaves the renderer's host: {}", f.control);
    let base = DEV
        .replace("{CTL}", "upnp/control/AVTransport1")
        .replace("<serviceList>", "<URLBase>http://203.0.113.9:8080/</URLBase><serviceList>");
    let f = parse_description(&base, loc).expect("renderer with AVTransport");
    assert_eq!(host_of(&f.control), "192.168.1.50", "URLBase moves the control endpoint off the renderer's host: {}", f.control);
}

/// `new_id` keeps input and bus ids unique together, but `add_network_bus` only checks buses.
/// The new bus can take an input's id, and then `remove_id` on the bus deletes the input too.
#[test]
fn breaker_bus_id_never_reuses_input_id() {
    let mut c = Config::default_config();
    let inp = c.add_stream_input("n1", "http://10.0.0.1:5959/s.wav");
    let bus = c.add_network_bus("Cast");
    assert_ne!(bus, inp, "network bus took the id of input {inp}");
    c.remove_id(&bus);
    assert!(c.inputs.iter().any(|i| i.id == inp), "removing bus {bus} also deleted input {inp}");
}

/// A request head that trickles in (one line every 2 s) keeps the handler alive, because the
/// 5 s read timeout restarts on every byte. There is no total deadline, so one client can hold
/// a thread for as long as it keeps sending.
#[test]
fn breaker_trickled_head_is_dropped() {
    let _g = lock();
    let s = srv(&["n1"]);
    let mut c = TcpStream::connect(("127.0.0.1", s.port)).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    c.write_all(b"GET /stream/n1.wav HTTP/1.1\r\n").unwrap();
    for _ in 0..8 {
        std::thread::sleep(Duration::from_secs(2));
        if c.write_all(b"X-Pad: y\r\n").is_err() {
            break;
        }
    }
    let mut b = [0u8; 256];
    let r = c.read(&mut b);
    assert!(
        !matches!(&r, Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)),
        "server is still reading a head trickled for 16 s: {r:?}"
    );
}
