//! Finds engines on the LAN by browsing `_wavelink._tcp.local.` (pure-Rust mDNS, works on macOS too).
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    /// Instance name, e.g. "Wavelink Mixer on bazzite".
    pub name: String,
    /// "host:port" to connect to (the mDNS host name, e.g. "bazzite.local:8790").
    pub addr: String,
    /// Resolved IPs, for display and as a fallback.
    pub ips: Vec<String>,
}

pub struct Discovery {
    pub found: Arc<Mutex<Vec<Found>>>,
    pub error: Option<String>,
    pub(crate) _daemon: Option<mdns_sd::ServiceDaemon>,
}

pub fn start(wake: impl Fn() + Send + 'static) -> Discovery {
    let found = Arc::new(Mutex::new(Vec::new()));
    let daemon = match mdns_sd::ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => return Discovery { found, error: Some(format!("mDNS unavailable: {e}")), _daemon: None },
    };
    let ty = format!("{}.local.", wavelink_proto::MDNS_SERVICE);
    let rx = match daemon.browse(&ty) {
        Ok(rx) => rx,
        Err(e) => return Discovery { found, error: Some(format!("mDNS browse failed: {e}")), _daemon: Some(daemon) },
    };
    let f = found.clone();
    std::thread::spawn(move || {
        while let Ok(ev) = rx.recv() {
            match ev {
                mdns_sd::ServiceEvent::ServiceResolved(info) => {
                    let name = info.fullname.trim_end_matches(&format!(".{ty}")).replace("\\032", " ").to_string();
                    let host = info.host.trim_end_matches('.').to_string();
                    let mut ips: Vec<String> = info.addresses.iter().map(|a| a.to_ip_addr()).filter(|a| a.is_ipv4()).map(|a| a.to_string()).collect();
                    ips.sort();
                    let entry = Found { name, addr: format!("{host}:{}", info.port), ips };
                    let mut v = f.lock().unwrap();
                    v.retain(|x| x.name != entry.name);
                    v.push(entry);
                    v.sort_by(|a, b| a.name.cmp(&b.name));
                    drop(v);
                    wake();
                }
                mdns_sd::ServiceEvent::ServiceRemoved(_, fullname) => {
                    let name = fullname.trim_end_matches(&format!(".{ty}")).to_string();
                    f.lock().unwrap().retain(|x| x.name != name);
                    wake();
                }
                _ => {}
            }
        }
    });
    Discovery { found, error: None, _daemon: Some(daemon) }
}
