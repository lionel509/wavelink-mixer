//! ~/.config/wavelink-mixer/remote.json: the server's token, address and safety caps (mode 600).
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct RemoteCfg {
    /// Shared secret every client sends in Hello. Generated on first run.
    pub token: String,
    pub enabled: bool,
    /// "auto" (default): this machine's LAN IPv4 addresses (Wi-Fi, Ethernet) plus 127.0.0.1, re-checked
    /// every 30 s; container bridges (podman, docker) and VPN interfaces are left out. Or a comma-separated
    /// list of addresses to listen on, e.g. "192.168.1.157,127.0.0.1" or "0.0.0.0" (everything).
    pub bind: String,
    /// With bind = "auto": also listen on the NordVPN Meshnet (nordlynx) / Tailscale address.
    pub allow_meshnet: bool,
    pub port: u16,
    /// Advertise `_wavelink._tcp` with avahi-publish.
    pub mdns: bool,
    /// Per-bus gain caps (dB) on top of the automatic one for network speakers (AirPlay/Sonos, bus a3).
    /// A value of 12 or more removes a cap.
    pub caps: BTreeMap<String, f64>,
}

impl Default for RemoteCfg {
    fn default() -> Self {
        RemoteCfg {
            token: String::new(),
            enabled: true,
            bind: "auto".into(),
            allow_meshnet: false,
            port: wavelink_proto::DEFAULT_PORT,
            mdns: true,
            caps: BTreeMap::new(),
        }
    }
}

pub fn path() -> PathBuf {
    crate::config::glib_home().join(".config/wavelink-mixer/remote.json")
}

/// 32 random bytes from the kernel as hex.
pub fn new_token() -> String {
    use std::io::Read as _;
    let mut b = [0u8; 32];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).is_err() {
        // no /dev/urandom (never on Linux): hash what varies; still unguessable enough for a LAN token
        use sha2::Digest as _;
        let seed = format!("{:?} {} {:?}", std::time::SystemTime::now(), std::process::id(), std::thread::current().id());
        b.copy_from_slice(&sha2::Sha256::digest(seed.as_bytes()));
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A temp name no other process or thread uses.
fn unique_tmp(p: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    p.with_extension(format!("json.tmp.{}.{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)))
}

/// Write `bytes` (mode 600) to a fresh unique temp file next to `p`.
fn write_tmp_600(p: &Path, bytes: &[u8]) -> std::io::Result<PathBuf> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = unique_tmp(p);
    let r = (|| {
        let mut f = std::fs::OpenOptions::new().create_new(true).write(true).mode(0o600).open(&tmp)?;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        f.write_all(bytes)?;
        f.sync_all()
    })();
    match r {
        Ok(()) => Ok(tmp),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Replace `p` atomically (rotate, repair).
fn write_600(p: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = write_tmp_600(p, bytes)?;
    std::fs::rename(&tmp, p).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Create `p` only if it doesn't exist: a complete temp file hard-linked into place (link() fails with
/// EEXIST, atomically, if someone else got there first). Ok(false) = it already existed.
fn create_exclusive_600(p: &Path, bytes: &[u8]) -> std::io::Result<bool> {
    let tmp = write_tmp_600(p, bytes)?;
    let r = std::fs::hard_link(&tmp, p);
    let _ = std::fs::remove_file(&tmp);
    match r {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e),
    }
}

fn parse(p: &Path, b: &[u8]) -> std::io::Result<RemoteCfg> {
    serde_json::from_slice::<RemoteCfg>(b).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{}: {e}", p.display())))
}

/// Load remote.json, creating it (with a fresh token, mode 600) on first run. Concurrent first runs
/// (the engine starting while `remote-token` runs) agree on one token: the file is created exclusively
/// and everyone reads back what won. A file readable by others is tightened to 600. Environment
/// overrides for tests/side-by-side runs: WAVELINK_REMOTE_PORT, WAVELINK_REMOTE_BIND, WAVELINK_REMOTE=0
/// (disable), WAVELINK_REMOTE_MDNS=0.
pub fn load_or_create(p: &Path) -> std::io::Result<RemoteCfg> {
    let mut cfg = None;
    for _ in 0..5 {
        match std::fs::read(p) {
            Ok(b) => {
                let mut c = parse(p, &b)?;
                if c.token.trim().len() < 16 {
                    // a hand-edited file without a usable token: give it one (replace, keep the other keys)
                    c.token = new_token();
                    write_600(p, &serde_json::to_vec_pretty(&c).unwrap())?;
                    continue; // re-read whatever is there now
                }
                if let Ok(m) = std::fs::metadata(p) {
                    if m.permissions().mode() & 0o077 != 0 {
                        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600))?;
                    }
                }
                cfg = Some(c);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let c = RemoteCfg { token: new_token(), ..RemoteCfg::default() };
                if create_exclusive_600(p, &serde_json::to_vec_pretty(&c).unwrap())? {
                    eprintln!("wavelink remote: created {} with a new token", p.display());
                }
            }
            Err(e) => return Err(e),
        }
    }
    let mut cfg = cfg.ok_or_else(|| std::io::Error::other(format!("{}: could not create a token", p.display())))?;
    if let Some(port) = std::env::var("WAVELINK_REMOTE_PORT").ok().and_then(|v| v.parse().ok()) {
        cfg.port = port;
    }
    if let Ok(b) = std::env::var("WAVELINK_REMOTE_BIND") {
        cfg.bind = b;
    }
    if std::env::var("WAVELINK_REMOTE").as_deref() == Ok("0") {
        cfg.enabled = false;
    }
    if std::env::var("WAVELINK_REMOTE_MDNS").as_deref() == Ok("0") {
        cfg.mdns = false;
    }
    Ok(cfg)
}

/// `remote-token --rotate`: a new token in remote.json (other keys kept). The engine picks it up on the
/// next Hello (it re-reads the file when it changes); clients must be given the new token.
pub fn rotate(p: &Path) -> std::io::Result<String> {
    let _lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).mode(0o600).open(p.with_extension("lock")).and_then(|f| f.lock().map(|_| f));
    let mut c = match std::fs::read(p) {
        Ok(b) => parse(p, &b)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => RemoteCfg::default(),
        Err(e) => return Err(e),
    };
    c.token = new_token();
    write_600(p, &serde_json::to_vec_pretty(&c).unwrap())?;
    Ok(c.token)
}

/// The current token in the file (for the server's live re-read after a rotate).
pub fn read_token(p: &Path) -> Option<String> {
    let c: RemoteCfg = serde_json::from_slice(&std::fs::read(p).ok()?).ok()?;
    (c.token.trim().len() >= 16).then(|| c.token.trim().to_string())
}

/// Interface kinds for the "auto" bind.
fn is_container_if(dev: &str) -> bool {
    ["podman", "docker", "veth", "virbr", "br-", "cni", "flannel", "lxc", "vnet"].iter().any(|p| dev.starts_with(p))
}
fn is_meshnet_if(dev: &str) -> bool {
    ["nordlynx", "tailscale"].iter().any(|p| dev.starts_with(p))
}
fn is_vpn_if(dev: &str) -> bool {
    is_meshnet_if(dev) || ["tun", "tap", "wg", "zt", "utun", "ppp", "nordtun"].iter().any(|p| dev.starts_with(p))
}

/// (interface, IPv4) from `ip -4 -o addr show scope global`.
pub fn parse_ip_addrs(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let dev = f.get(1)?.trim_end_matches(':').to_string();
            let ip = f.iter().position(|x| *x == "inet").and_then(|i| f.get(i + 1))?.split('/').next()?.to_string();
            Some((dev, ip))
        })
        .collect()
}

/// The LAN addresses (Wi-Fi, Ethernet): no container bridges, no VPNs (Meshnet only with `meshnet`).
pub fn lan_addrs(ifs: &[(String, String)], meshnet: bool) -> Vec<(String, String)> {
    ifs.iter().filter(|(d, _)| !is_container_if(d) && (!is_vpn_if(d) || (meshnet && is_meshnet_if(d)))).cloned().collect()
}

/// Addresses to listen on for a `bind` setting; `ifs` = the machine's interfaces (parse_ip_addrs).
pub fn bind_addrs(bind: &str, meshnet: bool, ifs: &[(String, String)]) -> Vec<String> {
    if bind.trim().is_empty() || bind.trim() == "auto" {
        let mut v: Vec<String> = lan_addrs(ifs, meshnet).into_iter().map(|(_, ip)| ip).collect();
        v.push("127.0.0.1".into());
        v.dedup();
        v
    } else {
        bind.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()
    }
}

/// This machine's interfaces now (empty if `ip` is missing).
pub fn interfaces() -> Vec<(String, String)> {
    crate::pw::output_timeout("ip", &["-4", "-o", "addr", "show", "scope", "global"], std::time::Duration::from_secs(2))
        .and_then(|o| String::from_utf8(o).ok())
        .map(|s| parse_ip_addrs(&s))
        .unwrap_or_default()
}

/// The hard ceiling of a bus: the mixer's own per-bus `max_db` (#35: -20 dB for an AirPlay speaker by
/// name, case-insensitive, or as persisted after the node props said so). None = the fader's top.
pub fn ceiling_for(bus: &crate::config::Bus) -> Option<f64> {
    let c = bus.ceiling_db();
    (c < wavelink_proto::MAX_DB).then_some(c)
}

/// The remote's soft cap for a bus (lifted only by a per-connection unlock), never above the ceiling:
/// remote.json's `caps` first (12 or more = no soft cap), else SPEAKER_CAP_DB for an AirPlay speaker
/// (#35's case-insensitive detection), else the ceiling itself if there is one.
pub fn cap_for(caps: &BTreeMap<String, f64>, bus: &crate::config::Bus) -> Option<f64> {
    let soft = match caps.get(&bus.id) {
        Some(c) => (c.is_finite() && *c < wavelink_proto::MAX_DB).then_some(*c),
        None => bus.is_raop(false).then_some(wavelink_proto::safety::SPEAKER_CAP_DB),
    };
    match (soft, ceiling_for(bus)) {
        (Some(s), Some(c)) => Some(s.min(c)),
        (s, c) => s.or(c),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_run_creates_private_token() {
        let dir = std::env::temp_dir().join(format!("wavelink-remote-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = dir.join("remote.json");
        let a = load_or_create(&p).unwrap();
        assert_eq!(a.token.len(), 64);
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        // stable across loads, and a too-open file is tightened
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        let b = load_or_create(&p).unwrap();
        assert_eq!(a.token, b.token);
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        assert_ne!(new_token(), new_token());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn auto_bind_is_lan_only() {
        let out = "2: enp14s0    inet 192.168.1.231/24 brd 192.168.1.255 scope global dynamic\n3: podman0    inet 10.88.0.1/16 scope global\n4: wlp15s0    inet 192.168.1.157/24 scope global\n5: nordlynx    inet 100.85.247.147/10 scope global\n6: veth0@if2    inet 10.89.0.1/24 scope global\n7: tun0    inet 10.8.0.2/24 scope global\n";
        let ifs = parse_ip_addrs(out);
        assert_eq!(bind_addrs("auto", false, &ifs), vec!["192.168.1.231", "192.168.1.157", "127.0.0.1"]);
        assert_eq!(bind_addrs("", true, &ifs), vec!["192.168.1.231", "192.168.1.157", "100.85.247.147", "127.0.0.1"]);
        assert_eq!(bind_addrs("192.168.1.157, 127.0.0.1", false, &ifs), vec!["192.168.1.157", "127.0.0.1"]);
        assert_eq!(bind_addrs("0.0.0.0", false, &ifs), vec!["0.0.0.0"]);
    }

    #[test]
    fn rotate_replaces_the_token_and_keeps_settings() {
        let dir = std::env::temp_dir().join(format!("wavelink-remote-rot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = dir.join("remote.json");
        let a = load_or_create(&p).unwrap();
        let mut j: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        j["port"] = 9999.into();
        std::fs::write(&p, serde_json::to_vec(&j).unwrap()).unwrap();
        let t = rotate(&p).unwrap();
        assert_ne!(t, a.token);
        assert_eq!(read_token(&p).as_deref(), Some(t.as_str()));
        let b = load_or_create(&p).unwrap();
        assert_eq!((b.token.as_str(), b.port), (t.as_str(), 9999));
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn speaker_buses_are_capped() {
        let cfg = crate::config::Config::default_config();
        let none = BTreeMap::new();
        let mut sonos = cfg.buses[0].clone();
        sonos.id = "a3".into();
        sonos.kind = crate::config::BusKind::Hardware { target: "raop_sonos_room".into() };
        assert_eq!(cap_for(&none, &sonos), Some(-20.0));
        let mut other_raop = sonos.clone();
        other_raop.id = "a7".into();
        assert_eq!(cap_for(&none, &other_raop), Some(-20.0));
        assert_eq!(cap_for(&none, &cfg.buses[0]), None); // headphones
        assert_eq!(cap_for(&none, &cfg.buses[2]), None); // virtual
        let mut caps = BTreeMap::new();
        caps.insert("a1".to_string(), -6.0);
        caps.insert("a3".to_string(), 12.0);
        assert_eq!(cap_for(&caps, &cfg.buses[0]), Some(-6.0));
        // removing the soft cap leaves the hard ceiling (#35's RAOP default)
        assert_eq!(cap_for(&caps, &sonos), Some(-20.0));
        assert_eq!(ceiling_for(&sonos), Some(-20.0));
        // a user ceiling below the speaker cap wins (one ceiling for every writer)
        let mut low = sonos.clone();
        low.max_db = Some(-30.0);
        assert_eq!(cap_for(&none, &low), Some(-30.0));
        assert_eq!(ceiling_for(&low), Some(-30.0));
        // a raised ceiling: the soft cap stays at -20, unlock may go up to -10
        let mut high = sonos.clone();
        high.max_db = Some(-10.0);
        assert_eq!(cap_for(&none, &high), Some(-20.0));
        assert_eq!(ceiling_for(&high), Some(-10.0));
        // renamed sink, ceiling persisted by #35 after its props said AirPlay
        let mut renamed = sonos.clone();
        renamed.kind = crate::config::BusKind::Hardware { target: "living_room".into() };
        renamed.max_db = Some(-20.0);
        assert_eq!(cap_for(&none, &renamed), Some(-20.0));
    }
}
