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
    pub bind: String,
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
            bind: "0.0.0.0".into(),
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

fn write_600(p: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = p.with_extension(format!("json.tmp.{}", std::process::id()));
    let mut f = std::fs::OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(&tmp)?;
    f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, p)
}

/// Load remote.json, creating it (with a fresh token, mode 600) on first run. A file readable by
/// others is tightened to 600. Environment overrides for tests/side-by-side runs:
/// WAVELINK_REMOTE_PORT, WAVELINK_REMOTE_BIND, WAVELINK_REMOTE=0 (disable), WAVELINK_REMOTE_MDNS=0.
pub fn load_or_create(p: &Path) -> std::io::Result<RemoteCfg> {
    let mut cfg = match std::fs::read(p) {
        Ok(b) => serde_json::from_slice::<RemoteCfg>(&b).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{}: {e}", p.display())))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => RemoteCfg::default(),
        Err(e) => return Err(e),
    };
    let fresh = !p.exists();
    if cfg.token.trim().len() < 16 {
        cfg.token = new_token();
        write_600(p, &serde_json::to_vec_pretty(&cfg).unwrap())?;
        if fresh {
            eprintln!("wavelink remote: created {} with a new token", p.display());
        }
    } else if let Ok(m) = std::fs::metadata(p) {
        if m.permissions().mode() & 0o077 != 0 {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600))?;
        }
    }
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

/// The cap for a bus: remote.json's `caps` first, else the speaker cap for network speakers
/// (an AirPlay/RAOP target, or bus a3 which is the Sonos on this setup).
pub fn cap_for(caps: &BTreeMap<String, f64>, bus: &crate::config::Bus) -> Option<f64> {
    if let Some(c) = caps.get(&bus.id) {
        return (*c < wavelink_proto::MAX_DB).then_some(*c);
    }
    let speaker = match &bus.kind {
        crate::config::BusKind::Hardware { target } => target.contains("raop") || target.contains("airplay") || bus.id == "a3",
        _ => false,
    };
    speaker.then_some(wavelink_proto::safety::SPEAKER_CAP_DB)
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
        assert_eq!(cap_for(&caps, &sonos), None);
    }
}
