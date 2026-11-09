//! Saved engines and tokens: <config dir>/wavelink-remote/settings.json (macOS: ~/Library/Application
//! Support/wavelink-remote), owner-only on unix.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// Last engine connected to ("host:port").
    pub last: Option<String>,
    /// Token per engine address.
    pub tokens: BTreeMap<String, String>,
    pub meters: Option<bool>,
}

pub fn path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("WAVELINK_REMOTE_SETTINGS") {
        return Some(PathBuf::from(p));
    }
    Some(dirs::config_dir()?.join("wavelink-remote/settings.json"))
}

impl Settings {
    pub fn load() -> Settings {
        path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let tmp = p.with_extension("json.tmp");
        let Ok(bytes) = serde_json::to_vec_pretty(self) else { return };
        let mut o = std::fs::OpenOptions::new();
        o.create(true).write(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        if let Ok(mut f) = o.open(&tmp) {
            use std::io::Write;
            if f.write_all(&bytes).is_ok() {
                let _ = std::fs::rename(&tmp, &p);
            }
        }
    }
}
