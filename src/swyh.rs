//! Network output (#15, SWYH-style): a Network bus plays on a DLNA renderer and/or is served over HTTP.
//! Only `route()` decides what is served, and `tap_args()` pins pw-record to the routed bus.
use crate::config::{names, BusKind, Config, Renderer};
use crate::pw::Graph;
use std::collections::{HashMap, HashSet};
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream, UdpSocket};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const PORT: u16 = 5959;
const RATE: u32 = 44100;
const MAX_CLIENTS: usize = 4;
const DLNA_FEATURES: &str = "DLNA.ORG_OP=00;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000";
const AVTRANSPORT: &str = "urn:schemas-upnp-org:service:AVTransport:1";
static CLIENTS: AtomicUsize = AtomicUsize::new(0);
static TAPS: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub udn: String,
    pub name: String,
    pub location: String,
    pub control: String,
}

#[derive(Default)]
pub struct Out {
    allowed: Arc<Mutex<Vec<(String, String)>>>,
    server: Option<Arc<AtomicBool>>,
    bind_retry: Option<Instant>,
    pushed: HashMap<String, Found>,
    inflight: HashSet<String>,
    last_try: HashMap<String, Instant>,
    last_err: HashMap<String, String>,
    results: Arc<Mutex<Vec<(String, Result<Found, String>)>>>,
}

impl Out {
    /// Called from the engine's 1 s loop. Never blocks: discovery and SOAP run on spawned threads.
    pub fn tick(&mut self, cfg: &Config, g: &Graph) {
        let present = |n: &str| g.has(n);
        let al = allowed(cfg, &present);
        let nonempty = !al.is_empty();
        if let Ok(mut v) = self.allowed.lock() {
            *v = al;
        }
        // 2. Serve only while at least one Network bus exists; closing the stop flag closes the port.
        if nonempty && self.server.is_none() {
            let retry_ok = self.bind_retry.map_or(true, |t| t.elapsed() >= Duration::from_secs(30));
            if retry_ok {
                match TcpListener::bind(("0.0.0.0", PORT)) {
                    Ok(l) => {
                        let stop = Arc::new(AtomicBool::new(false));
                        let (a, s) = (self.allowed.clone(), stop.clone());
                        std::thread::spawn(move || serve(l, a, s));
                        self.server = Some(stop);
                        self.bind_retry = None;
                        eprintln!("net out: serving on port {PORT}");
                    }
                    Err(e) => {
                        self.bind_retry = Some(Instant::now());
                        eprintln!("net out: cannot bind port {PORT}: {e} (retry in 30 s)");
                    }
                }
            }
        } else if !nonempty {
            if let Some(stop) = self.server.take() {
                stop.store(true, Ordering::SeqCst);
            }
        }
        // 3. Collect finished pushes.
        let drained: Vec<(String, Result<Found, String>)> = match self.results.lock() {
            Ok(mut r) => r.drain(..).collect(),
            Err(_) => vec![],
        };
        for (id, res) in drained {
            self.inflight.remove(&id);
            match res {
                Ok(f) => {
                    eprintln!("net out {id} -> {}", f.name);
                    self.last_err.remove(&id);
                    self.pushed.insert(id, f);
                }
                Err(e) => {
                    if self.last_err.get(&id) != Some(&e) {
                        eprintln!("net out {id}: {e}");
                        self.last_err.insert(id, e);
                    }
                }
            }
        }
        // 4. Push to renderers that are wanted but not yet playing the bus.
        let wanted = wanted_pushes(cfg, &present);
        for (id, r) in &wanted {
            let due = self.pushed.get(id).map_or(true, |f| f.udn != r.udn);
            let cool = self.last_try.get(id).map_or(true, |t| t.elapsed() >= Duration::from_secs(30));
            if !due || !cool || self.inflight.contains(id) {
                continue;
            }
            self.inflight.insert(id.clone());
            self.last_try.insert(id.clone(), Instant::now());
            let title = cfg.buses.iter().find(|b| &b.id == id).map_or("Network".to_string(), |b| b.name.clone());
            let (id2, r2, results) = (id.clone(), r.clone(), self.results.clone());
            std::thread::spawn(move || {
                let res = push_to(&id2, &r2, &title);
                if let Ok(mut v) = results.lock() {
                    v.push((id2, res));
                }
            });
        }
        // 5. Stop renderers whose bus is gone or whose renderer was switched.
        let stale: Vec<String> = self
            .pushed
            .iter()
            .filter(|(id, f)| !wanted.iter().any(|(w, r)| w == *id && r.udn == f.udn))
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            if let Some(f) = self.pushed.remove(&id) {
                std::thread::spawn(move || {
                    let _ = post(&f.control, "Stop", &envelope("Stop", "<InstanceID>0</InstanceID>"));
                });
            }
        }
    }
}

/// SSDP M-SEARCH for renderers and AVTransport services, then fetch each description.
/// Blocking: call it from a spawned thread or the CLI, never from `tick`.
pub fn discover(wait: Duration) -> Vec<Found> {
    let sock = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    let _ = sock.set_read_timeout(Some(Duration::from_millis(200)));
    let _ = sock.set_multicast_ttl_v4(2);
    for st in ["urn:schemas-upnp-org:device:MediaRenderer:1", AVTRANSPORT] {
        let msg = format!("M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: {st}\r\n\r\n");
        let _ = sock.send_to(msg.as_bytes(), ("239.255.255.250", 1900));
    }
    let deadline = Instant::now() + wait;
    let mut locs: Vec<String> = vec![];
    let mut buf = [0u8; 2048];
    while Instant::now() < deadline {
        if let Ok((n, _)) = sock.recv_from(&mut buf) {
            if let Some(loc) = parse_ssdp(&String::from_utf8_lossy(&buf[..n])) {
                if !locs.contains(&loc) {
                    locs.push(loc);
                }
            }
        }
    }
    let mut out: Vec<Found> = vec![];
    for loc in locs {
        let xml = match ureq::get(&loc).timeout(Duration::from_secs(2)).call() {
            Ok(r) => match r.into_string() {
                Ok(s) => s,
                Err(_) => continue,
            },
            Err(_) => continue,
        };
        if let Some(f) = parse_description(&xml, &loc) {
            if !out.iter().any(|o| o.udn == f.udn) {
                out.push(f);
            }
        }
    }
    out
}

pub fn stream_url(bus_id: &str, toward: Option<&str>) -> String {
    let ip = local_ip_for(toward.unwrap_or("239.255.255.250")).unwrap_or(IpAddr::from([127, 0, 0, 1]));
    url_at(bus_id, ip)
}

/// `dlna-scan`, `net-add <name> [renderer]`, `net-rm <id>`. Anything else returns None.
pub fn cli(args: &[String]) -> Option<i32> {
    let a: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    match a.as_slice() {
        ["dlna-scan"] => {
            let found = discover(Duration::from_secs(3));
            if found.is_empty() {
                eprintln!("no renderers found");
            }
            for f in &found {
                println!("{}\t{}\t{}", f.name, f.udn, f.location);
            }
            Some(0)
        }
        ["net-add", name, rest @ ..] => {
            let renderer = match rest.first() {
                None => None,
                Some(want) => {
                    let w = want.to_lowercase();
                    match discover(Duration::from_secs(3))
                        .into_iter()
                        .find(|f| f.name.to_lowercase().contains(&w) || f.udn == *want)
                    {
                        Some(f) => Some(Renderer { udn: f.udn, name: f.name }),
                        None => {
                            eprintln!("renderer {want:?} not found (run dlna-scan)");
                            return Some(1);
                        }
                    }
                }
            };
            let mut cfg = Config::load();
            let id = cfg.add_network_bus(name);
            if let Some(r) = renderer {
                cfg.net_out.insert(id.clone(), r);
            }
            cfg.save();
            println!("{id}\t{}", stream_url(&id, None));
            Some(0)
        }
        ["net-rm", id] => {
            let mut cfg = Config::load();
            if cfg.remove_id(id) {
                cfg.save();
                Some(0)
            } else {
                eprintln!("unknown id {id}");
                Some(1)
            }
        }
        _ => None,
    }
}

fn url_at(id: &str, ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(_) => format!("http://{ip}:{PORT}/stream/{id}.wav"),
        IpAddr::V6(_) => format!("http://[{ip}]:{PORT}/stream/{id}.wav"),
    }
}

fn local_ip_for(host: &str) -> Option<IpAddr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect((host, 1900)).ok()?;
    Some(s.local_addr().ok()?.ip())
}

fn parse_ssdp(resp: &str) -> Option<String> {
    resp.lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            if k.trim().eq_ignore_ascii_case("location") {
                Some(v.trim().to_string())
            } else {
                None
            }
        })
        .filter(|s| !s.is_empty())
}

fn tag_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let s = xml.find(&open)? + open.len();
    let e = s + xml[s..].find(&close)?;
    Some(xml[s..e].trim().to_string())
}

fn unesc(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

fn parse_description(xml: &str, location: &str) -> Option<Found> {
    let name = unesc(&tag_text(xml, "friendlyName")?);
    let udn = tag_text(xml, "UDN")?;
    let base = tag_text(xml, "URLBase").unwrap_or_else(|| location.to_string());
    let svc = xml.split("<service>").skip(1).find(|b| b.contains(AVTRANSPORT))?;
    let ctl = tag_text(svc, "controlURL")?;
    Some(Found { udn, name, location: location.to_string(), control: join_url(&base, &ctl) })
}

fn scheme_end(base: &str) -> usize {
    base.find("://").map_or(0, |i| i + 3)
}

fn join_url(base: &str, rel: &str) -> String {
    if rel.starts_with("http") {
        return rel.to_string();
    }
    let se = scheme_end(base);
    if rel.starts_with('/') {
        let end = base[se..].find('/').map_or(base.len(), |i| se + i);
        return format!("{}{rel}", &base[..end]);
    }
    match base[se..].rfind('/') {
        Some(i) => format!("{}{rel}", &base[..se + i + 1]),
        None => format!("{base}/{rel}"),
    }
}

fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split('/').next().unwrap_or("");
    authority.rsplit_once(':').map_or(authority, |(h, _)| h)
}

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            c => o.push(c),
        }
    }
    o
}

fn envelope(action: &str, args: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:{action} xmlns:u="{AVTRANSPORT}">{args}</u:{action}></s:Body></s:Envelope>"#
    )
}

fn soap_set_uri(url: &str, title: &str) -> String {
    let didl = format!(
        r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/"><item id="1" parentID="0" restricted="1"><dc:title>{}</dc:title><upnp:class>object.item.audioItem.musicTrack</upnp:class><res protocolInfo="http-get:*:audio/wav:{DLNA_FEATURES}">{}</res></item></DIDL-Lite>"#,
        esc(title),
        esc(url)
    );
    envelope(
        "SetAVTransportURI",
        &format!(
            "<InstanceID>0</InstanceID><CurrentURI>{}</CurrentURI><CurrentURIMetaData>{}</CurrentURIMetaData>",
            esc(url),
            esc(&didl)
        ),
    )
}

fn post(control: &str, action: &str, body: &str) -> Result<(), String> {
    ureq::post(control)
        .timeout(Duration::from_secs(5))
        .set("Content-Type", "text/xml; charset=\"utf-8\"")
        .set("SOAPACTION", &format!("\"{AVTRANSPORT}#{action}\""))
        .send_string(body)
        .map(|_| ())
        .map_err(|e| format!("{action}: {e}"))
}

/// Stop (error ignored), SetAVTransportURI, Play. Returns the first error from the last two.
fn push(f: &Found, url: &str, title: &str) -> Result<(), String> {
    let _ = post(&f.control, "Stop", &envelope("Stop", "<InstanceID>0</InstanceID>"));
    post(&f.control, "SetAVTransportURI", &soap_set_uri(url, title))?;
    post(&f.control, "Play", &envelope("Play", "<InstanceID>0</InstanceID><Speed>1</Speed>"))
}

fn push_to(id: &str, r: &Renderer, title: &str) -> Result<Found, String> {
    let f = discover(Duration::from_secs(3))
        .into_iter()
        .find(|f| f.udn == r.udn)
        .ok_or_else(|| format!("renderer {} not found", r.name))?;
    let ip = local_ip_for(host_of(&f.control)).ok_or_else(|| "no route to renderer".to_string())?;
    push(&f, &url_at(id, ip), title)?;
    Ok(f)
}

fn wav_header(rate: u32, channels: u16) -> [u8; 44] {
    let block = channels * 2;
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes());
    h[22..24].copy_from_slice(&channels.to_le_bytes());
    h[24..28].copy_from_slice(&rate.to_le_bytes());
    h[28..32].copy_from_slice(&(rate * block as u32).to_le_bytes());
    h[32..34].copy_from_slice(&block.to_le_bytes());
    h[34..36].copy_from_slice(&16u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&(u32::MAX - 36).to_le_bytes());
    h
}

fn parse_request(head: &str) -> Option<(String, String)> {
    let line = head.lines().next()?;
    let parts: Vec<&str> = line.split(' ').collect();
    if parts.len() != 3 || !parts[2].starts_with("HTTP/") {
        return None;
    }
    Some((parts[0].to_string(), parts[1].to_string()))
}

/// Serves only `/stream/<id>.wav` (query string ignored) for an allowed id.
fn route(path: &str, allowed: &[String]) -> Option<String> {
    let p = path.split('?').next().unwrap_or("");
    let id = p.strip_prefix("/stream/")?.strip_suffix(".wav")?;
    allowed.iter().find(|a| a.as_str() == id).cloned()
}

fn tap_args(bus_id: &str, n: usize) -> Vec<String> {
    vec![
        "--target".into(),
        names::bus(bus_id),
        "-P".into(),
        format!(
            "{{ node.name=net_{bus_id}_{n} media.class=Stream/Input/Audio stream.capture.sink=true node.dont-fallback=true node.dont-reconnect=true }}"
        ),
        "--format".into(),
        "s16".into(),
        "--rate".into(),
        RATE.to_string(),
        "--channels".into(),
        "2".into(),
        "--raw".into(),
        "-".into(),
    ]
}

fn allowed(cfg: &Config, present: &dyn Fn(&str) -> bool) -> Vec<(String, String)> {
    cfg.buses
        .iter()
        .filter(|b| matches!(b.kind, BusKind::Network) && present(&names::bus(&b.id)))
        .map(|b| (b.id.clone(), b.name.clone()))
        .collect()
}

fn wanted_pushes(cfg: &Config, present: &dyn Fn(&str) -> bool) -> Vec<(String, Renderer)> {
    allowed(cfg, present)
        .into_iter()
        .filter_map(|(id, _)| cfg.net_out.get(&id).map(|r| (id.clone(), r.clone())))
        .collect()
}

struct Slot;

impl Slot {
    fn take() -> Option<Slot> {
        if CLIENTS.fetch_add(1, Ordering::SeqCst) >= MAX_CLIENTS {
            CLIENTS.fetch_sub(1, Ordering::SeqCst);
            None
        } else {
            Some(Slot)
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        CLIENTS.fetch_sub(1, Ordering::SeqCst);
    }
}

fn still_allowed(allowed: &Mutex<Vec<(String, String)>>, id: &str) -> bool {
    allowed.lock().map(|v| v.iter().any(|(a, _)| a == id)).unwrap_or(false)
}

/// True once the client has closed its end. A peek sees the FIN without consuming any bytes.
fn peer_closed(s: &TcpStream) -> bool {
    if s.set_nonblocking(true).is_err() {
        return false;
    }
    let closed = match s.peek(&mut [0u8; 1]) {
        Ok(0) => true,
        Ok(_) => false,
        Err(e) => !matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted),
    };
    let _ = s.set_nonblocking(false);
    closed
}

fn serve(l: TcpListener, allowed: Arc<Mutex<Vec<(String, String)>>>, stop: Arc<AtomicBool>) {
    if l.set_nonblocking(true).is_err() {
        return;
    }
    while !stop.load(Ordering::SeqCst) {
        match l.accept() {
            Ok((s, _)) => {
                let allowed = allowed.clone();
                std::thread::spawn(move || {
                    let _ = handle(s, &allowed);
                });
            }
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

fn handle(mut s: TcpStream, allowed: &Mutex<Vec<(String, String)>>) -> std::io::Result<()> {
    // Accepted sockets inherit O_NONBLOCK on macOS, so reset it.
    s.set_nonblocking(false)?;
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    s.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut head: Vec<u8> = Vec::new();
    let mut buf = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < 8192 {
        let n = s.read(&mut buf)?;
        if n == 0 {
            break;
        }
        head.extend_from_slice(&buf[..n]);
    }
    let text = String::from_utf8_lossy(&head).into_owned();
    let ids: Vec<String> = allowed.lock().map(|v| v.iter().map(|(i, _)| i.clone()).collect()).unwrap_or_default();
    let parsed = parse_request(&text);
    let routed = parsed.as_ref().filter(|(m, _)| m == "GET" || m == "HEAD").and_then(|(_, p)| route(p, &ids));
    let (method, id) = match (parsed, routed) {
        (Some((m, _)), Some(id)) => (m, id),
        _ => return s.write_all(b"HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n"),
    };
    let Some(_slot) = Slot::take() else {
        return s.write_all(b"HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\n\r\n");
    };
    let hdr = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nCache-Control: no-cache\r\nConnection: close\r\ntransferMode.dlna.org: Streaming\r\ncontentFeatures.dlna.org: {DLNA_FEATURES}\r\nServer: wavelink-mixer\r\n\r\n"
    );
    s.write_all(hdr.as_bytes())?;
    if method == "HEAD" {
        return Ok(());
    }
    s.write_all(&wav_header(RATE, 2))?;
    let n = TAPS.fetch_add(1, Ordering::SeqCst);
    let mut cmd = Command::new("pw-record");
    cmd.args(tap_args(&id, n)).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    crate::pw::die_with_parent(&mut cmd);
    let mut child = cmd.spawn()?;
    let res = match child.stdout.take() {
        None => Ok(()),
        Some(mut out) => {
            // A reader thread hands the tap's bytes over, so this loop wakes up every 500 ms even
            // when the tap is silent and can check the bus and the client socket.
            // Bounded (16 chunks, about 64 KiB): a client that stops reading makes `send` block, so
            // the tap's pipe pushes back and engine RAM stays flat. Dropping `rx` unblocks it.
            let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(16);
            std::thread::spawn(move || {
                let mut b = [0u8; 4096];
                loop {
                    match out.read(&mut b) {
                        Ok(0) => break,
                        Ok(n) => {
                            if tx.send(b[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                        Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
            });
            let mut last = Instant::now();
            loop {
                match rx.recv_timeout(Duration::from_millis(500)) {
                    Ok(chunk) => {
                        if let Err(e) = s.write_all(&chunk) {
                            break Err(e);
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break Ok(()),
                }
                if last.elapsed() >= Duration::from_millis(500) {
                    last = Instant::now();
                    if !still_allowed(allowed, &id) || peer_closed(&s) {
                        break Ok(());
                    }
                }
            }
        }
    };
    let _ = child.kill();
    let _ = child.wait();
    res
}

#[cfg(test)]
#[path = "swyh_tests.rs"]
mod tests;
