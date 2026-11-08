use super::*;
use crate::config::{names, Bus, BusKind, Config, Renderer};
use std::io::{Read, Write};
use std::net::{IpAddr, TcpStream};
use std::sync::atomic::Ordering;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

const SEED: u64 = 0x9E3779B97F4A7C15;

fn rng(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

const DESC: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0"><device>
<friendlyName>Living Room &amp; TV</friendlyName>
<UDN>uuid:abc-123</UDN>
<serviceList>
<service><serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType><controlURL>upnp/control/RenderingControl1</controlURL></service>
<service><serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType><controlURL>upnp/control/AVTransport1</controlURL></service>
</serviceList></device></root>"#;

#[test]
fn ssdp_response_location_parsed() {
    let resp = "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nlocation: http://192.168.1.50:49152/desc.xml\r\nST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n";
    assert_eq!(parse_ssdp(resp).as_deref(), Some("http://192.168.1.50:49152/desc.xml"));
    let upper = "HTTP/1.1 200 OK\r\nLOCATION:   http://10.0.0.9:1/d.xml  \r\n\r\n";
    assert_eq!(parse_ssdp(upper).as_deref(), Some("http://10.0.0.9:1/d.xml"));
    assert_eq!(parse_ssdp("HTTP/1.1 200 OK\r\nST: x\r\n\r\n"), None);
}

#[test]
fn description_finds_avtransport_control() {
    let loc = "http://192.168.1.50:49152/desc.xml";
    let f = parse_description(DESC, loc).expect("renderer with AVTransport");
    assert_eq!(f.udn, "uuid:abc-123");
    assert_eq!(f.name, "Living Room & TV");
    assert_eq!(f.location, loc);
    assert_eq!(f.control, "http://192.168.1.50:49152/upnp/control/AVTransport1");
    let no_av = DESC.replace("AVTransport:1", "Other:1");
    assert_eq!(parse_description(&no_av, loc), None);
}

#[test]
fn join_url_cases() {
    assert_eq!(join_url("http://h:1/desc.xml", "http://other/x"), "http://other/x");
    assert_eq!(join_url("http://h:1/desc.xml", "/a/b"), "http://h:1/a/b");
    assert_eq!(join_url("http://h:1/dir/desc.xml", "ctl/x"), "http://h:1/dir/ctl/x");
    assert_eq!(join_url("http://h:1", "/x"), "http://h:1/x");
}

#[test]
fn soap_set_uri_escapes_everything() {
    let body = soap_set_uri("http://10.0.0.2:5959/stream/n1.wav", "Mom's <b>&\"Mix\"");
    assert!(body.contains("<CurrentURI>http://10.0.0.2:5959/stream/n1.wav</CurrentURI>"));
    assert!(body.contains("&lt;DIDL-Lite"));
    assert!(body.contains("<InstanceID>0</InstanceID>"));
    assert!(!body.contains("<b>"));
    let alphabet: Vec<char> = "<>&\"' ab".chars().collect();
    let mut s = SEED;
    for _ in 0..300 {
        let len = (rng(&mut s) % 12) as usize;
        let title: String = (0..len).map(|_| alphabet[(rng(&mut s) % alphabet.len() as u64) as usize]).collect();
        let body = soap_set_uri("http://10.0.0.2:5959/stream/n1.wav", &title);
        let start = body.find("<CurrentURIMetaData>").expect("metadata open") + "<CurrentURIMetaData>".len();
        let end = body.find("</CurrentURIMetaData>").expect("metadata close");
        let meta = &body[start..end];
        assert!(!meta.contains('<') && !meta.contains('>'), "title {title:?} leaked markup into metadata");
    }
}

#[test]
fn wav_header_bytes() {
    let h = wav_header(44100, 2);
    assert_eq!(&h[0..4], b"RIFF");
    assert_eq!(&h[4..8], &u32::MAX.to_le_bytes());
    assert_eq!(&h[8..12], b"WAVE");
    assert_eq!(&h[12..16], b"fmt ");
    assert_eq!(&h[16..20], &16u32.to_le_bytes());
    assert_eq!(&h[20..22], &1u16.to_le_bytes());
    assert_eq!(&h[22..24], &2u16.to_le_bytes());
    assert_eq!(&h[24..28], &44100u32.to_le_bytes());
    assert_eq!(&h[28..32], &(44100u32 * 4).to_le_bytes());
    assert_eq!(&h[32..34], &4u16.to_le_bytes());
    assert_eq!(&h[34..36], &16u16.to_le_bytes());
    assert_eq!(&h[36..40], b"data");
    assert_eq!(&h[40..44], &(u32::MAX - 36).to_le_bytes());
}

#[test]
fn parse_request_line() {
    let get = parse_request("GET /stream/n1.wav HTTP/1.1\r\nHost: x\r\n\r\n");
    assert_eq!(get, Some(("GET".to_string(), "/stream/n1.wav".to_string())));
    assert_eq!(parse_request("HEAD /x HTTP/1.0"), Some(("HEAD".to_string(), "/x".to_string())));
    assert_eq!(parse_request("GET /x"), None);
    assert_eq!(parse_request("GET /x FTP/1.0\r\n\r\n"), None);
    assert_eq!(parse_request("GET /a b HTTP/1.1\r\n\r\n"), None);
    assert_eq!(parse_request(""), None);
}

#[test]
fn route_only_serves_network_buses() {
    let allowed = vec!["n1".to_string(), "n2".to_string()];
    assert_eq!(route("/stream/n1.wav", &allowed).as_deref(), Some("n1"));
    assert_eq!(route("/stream/n1.wav?bd=16", &allowed).as_deref(), Some("n1"));
    for p in [
        "/stream/a1.wav",
        "/stream/n1",
        "/stream/../n1.wav",
        "/stream/n1.wav/../a1.wav",
        "/stream/N1.wav",
        "//stream/n1.wav",
        "/stream/n1.wav.wav",
        "/stream/%6E1.wav",
        "",
        "/",
    ] {
        assert_eq!(route(p, &allowed), None, "{p:?} must not be served");
    }
    let alphabet: Vec<char> = "/stream.wavn12a?%".chars().collect();
    let mut s = SEED;
    for _ in 0..1000 {
        let len = (rng(&mut s) % 30) as usize;
        let p: String = (0..len).map(|_| alphabet[(rng(&mut s) % alphabet.len() as u64) as usize]).collect();
        if let Some(id) = route(&p, &allowed) {
            assert!(allowed.contains(&id), "{p:?} routed to {id}");
            let want = format!("/stream/{id}.wav");
            assert!(p == want || p.starts_with(&format!("{want}?")), "{p:?} routed to {id}");
        }
    }
}

#[test]
fn tap_args_pin_the_bus() {
    for id in ["n1", "n2", "x_y"] {
        let a = tap_args(id, 3);
        let t = a.iter().position(|x| x == "--target").expect("--target");
        assert_eq!(a[t + 1], format!("bus_{id}"));
        let p = a.iter().position(|x| x == "-P").expect("-P");
        assert!(a[p + 1].contains("stream.capture.sink=true"), "{}", a[p + 1]);
        assert!(a[p + 1].contains("node.dont-fallback=true"), "{}", a[p + 1]);
        assert!(a[p + 1].contains(&format!("node.name=net_{id}_3")), "{}", a[p + 1]);
        assert_eq!(a[a.len() - 2..], ["--raw", "-"]);
    }
}

#[test]
fn allowed_and_pushes_only_network_buses() {
    let mut s = SEED;
    let mut hits = 0;
    for _ in 0..300 {
        let mut cfg = Config::default_config();
        if rng(&mut s) % 2 == 0 {
            cfg.add_network_bus("Net");
        }
        if rng(&mut s) % 2 == 0 {
            cfg.add_network_bus("");
        }
        let mut ids: Vec<String> = cfg.buses.iter().map(|b| b.id.clone()).collect();
        ids.push("zz".to_string());
        for _ in 0..4 {
            let k = ids[(rng(&mut s) % ids.len() as u64) as usize].clone();
            let udn = format!("uuid:{}", rng(&mut s) % 5);
            cfg.net_out.insert(k, Renderer { udn, name: "R".into() });
        }
        let present_names: Vec<String> = cfg
            .buses
            .iter()
            .map(|b| names::bus(&b.id))
            .filter(|_| rng(&mut s) % 3 != 0)
            .collect();
        let present = |n: &str| present_names.iter().any(|p| p == n);
        let al = allowed(&cfg, &present);
        for (id, _) in &al {
            let b = cfg.buses.iter().find(|b| &b.id == id).expect("allowed id is a bus");
            assert!(matches!(b.kind, BusKind::Network), "{id} is not a Network bus");
            assert!(present(&names::bus(id)), "{id} is not present");
        }
        // positive case: every present Network bus is allowed, and one with a renderer is pushed
        let pushes = wanted_pushes(&cfg, &present);
        for b in cfg.buses.iter().filter(|b| matches!(b.kind, BusKind::Network) && present(&names::bus(&b.id))) {
            assert!(al.iter().any(|(a, _)| a == &b.id), "present Network bus {} is not allowed", b.id);
            hits += 1;
            if cfg.net_out.contains_key(&b.id) {
                assert!(pushes.iter().any(|(p, _)| p == &b.id), "allowed bus {} with a renderer is not pushed", b.id);
            }
        }
        for (id, r) in pushes {
            assert!(al.iter().any(|(a, _)| a == &id), "{id} pushed but not allowed");
            assert_eq!(Some(&r), cfg.net_out.get(&id), "{id} pushed to the wrong renderer");
        }
    }
    assert!(hits > 0, "no round had a present Network bus, so the test checked nothing");
}

#[test]
fn server_heads_and_404s_over_tcp() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let allowed_list = Arc::new(Mutex::new(vec![("n1".to_string(), "Cast".to_string())]));
    let stop = Arc::new(AtomicBool::new(false));
    let (a2, s2) = (allowed_list.clone(), stop.clone());
    let th = std::thread::spawn(move || serve(l, a2, s2));
    let req = |line: &str| -> String {
        let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        write!(c, "{line}\r\nHost: x\r\n\r\n").unwrap();
        let mut out = String::new();
        let _ = c.read_to_string(&mut out);
        out
    };
    let head = req("HEAD /stream/n1.wav HTTP/1.1");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(head.contains("audio/wav"), "{head}");
    assert!(req("GET /stream/a1.wav HTTP/1.1").starts_with("HTTP/1.1 404"));
    assert!(req("GET /etc/passwd HTTP/1.1").starts_with("HTTP/1.1 404"));
    stop.store(true, Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_secs(1);
    while !th.is_finished() {
        assert!(Instant::now() < deadline, "serve did not stop within 1 s");
        std::thread::sleep(Duration::from_millis(20));
    }
    th.join().unwrap();
}

#[test]
fn stream_url_format() {
    assert_eq!(PORT, 5959);
    let ip: IpAddr = "10.0.0.2".parse().unwrap();
    assert_eq!(url_at("n1", ip), "http://10.0.0.2:5959/stream/n1.wav");
}

#[test]
fn local_ip_for_loopback() {
    let lo: IpAddr = "127.0.0.1".parse().unwrap();
    assert_eq!(local_ip_for("127.0.0.1"), Some(lo));
}

#[test]
fn network_bus_fader_controls_bus_sink() {
    let b = Bus {
        id: "n1".into(),
        name: "Cast".into(),
        color: "#30d158".into(),
        kind: BusKind::Network,
        gain_db: 0.0,
        mute: false,
    };
    assert_eq!(crate::engine::bus_fader_node(&b), "bus_n1");
}

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

