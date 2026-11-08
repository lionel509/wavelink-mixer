//! Minimal Spotify Web API client: PKCE login (no client secret), token refresh,
//! and JSON helpers. Audio itself comes from librespot ("Mixer" Connect device).
use base64::Engine;
use serde_json::{json, Value};
use sha2::Digest;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const REDIRECT: &str = "http://127.0.0.1:8898/callback";
const SCOPES: &str = "user-read-playback-state user-modify-playback-state user-read-currently-playing \
                      playlist-read-private playlist-read-collaborative user-library-read user-read-private";
/// Name of the librespot Connect device the mixer plays on.
pub const DEVICE: &str = "Mixer";

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn state_path() -> PathBuf {
    crate::config::glib_home().join(".local/state/wavelink-mixer/spotify.json")
}

/// Saved login: client id + refresh token (file is 0600).
pub fn saved() -> (String, String) {
    let v: Value = std::fs::read(state_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
    (v["client_id"].as_str().unwrap_or("").to_string(), v["refresh_token"].as_str().unwrap_or("").to_string())
}

fn save(client_id: &str, refresh: &str) {
    let p = state_path();
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    let body = serde_json::to_vec_pretty(&json!({ "client_id": client_id, "refresh_token": refresh })).unwrap();
    use std::os::unix::fs::OpenOptionsExt;
    if let Ok(mut f) = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&p) {
        let _ = f.write_all(&body);
    }
}

fn b64url(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

fn random_bytes(n: usize) -> Vec<u8> {
    let mut b = vec![0u8; n];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut b);
    }
    b
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|c| match c {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (c as char).to_string(),
            _ => format!("%{c:02X}"),
        })
        .collect()
}

pub struct Api {
    client_id: String,
    tok: Mutex<(String, String, u64)>, // access, refresh, expiry (unix s)
}

impl Api {
    /// Browser login with PKCE. Blocks (call from a worker thread) until the redirect arrives.
    pub fn login(client_id: &str) -> Result<Api, String> {
        let verifier = b64url(&random_bytes(64));
        let challenge = b64url(&sha2::Sha256::digest(verifier.as_bytes()));
        let state = b64url(&random_bytes(16));
        let url = format!(
            "https://accounts.spotify.com/authorize?response_type=code&client_id={}&scope={}&redirect_uri={}&code_challenge_method=S256&code_challenge={}&state={}",
            enc(client_id), enc(SCOPES), enc(REDIRECT), challenge, state
        );
        let listener = std::net::TcpListener::bind("127.0.0.1:8898").map_err(|e| format!("port 8898 busy: {e}"))?;
        crate::pw::spawn("xdg-open", &[url.as_str()]);
        listener.set_nonblocking(false).ok();
        let deadline = SystemTime::now() + Duration::from_secs(300);
        let code = loop {
            if SystemTime::now() > deadline {
                return Err("login timed out".into());
            }
            let (mut s, _) = listener.accept().map_err(|e| e.to_string())?;
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let line = req.lines().next().unwrap_or("").to_string();
            let q = line.split_whitespace().nth(1).unwrap_or("").split_once('?').map(|x| x.1).unwrap_or("").to_string();
            let param = |k: &str| q.split('&').find_map(|kv| kv.strip_prefix(&format!("{k}="))).map(|v| v.to_string());
            if !line.contains("/callback") {
                let _ = s.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
                continue;
            }
            let ok = param("state").as_deref() == Some(state.as_str()) && param("code").is_some();
            let msg = if ok { "Spotify connected to the Mixer. You can close this tab." } else { "Spotify login failed. Close this tab and try again in the Mixer." };
            let html = format!("<html><body style='font-family:sans-serif;background:#111;color:#eee;padding:40px'><h2>{msg}</h2></body></html>");
            let _ = s.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{html}", html.len()).as_bytes());
            if !ok {
                return Err(param("error").unwrap_or_else(|| "login was cancelled".into()));
            }
            break param("code").unwrap();
        };
        let v: Value = ureq::post("https://accounts.spotify.com/api/token")
            .send_form(&[("grant_type", "authorization_code"), ("code", &code), ("redirect_uri", REDIRECT), ("client_id", client_id), ("code_verifier", &verifier)])
            .map_err(|e| format!("token exchange failed: {e}"))?
            .into_json()
            .map_err(|e| e.to_string())?;
        let api = Api::from_token(client_id, &v)?;
        Ok(api)
    }

    /// Resume from a saved refresh token.
    pub fn resume() -> Option<Api> {
        let (cid, refresh) = saved();
        if cid.is_empty() || refresh.is_empty() {
            return None;
        }
        let api = Api { client_id: cid, tok: Mutex::new((String::new(), refresh, 0)) };
        api.access().ok()?;
        Some(api)
    }

    fn from_token(client_id: &str, v: &Value) -> Result<Api, String> {
        let access = v["access_token"].as_str().ok_or("no access token")?.to_string();
        let refresh = v["refresh_token"].as_str().unwrap_or("").to_string();
        let exp = now() + v["expires_in"].as_u64().unwrap_or(3600);
        save(client_id, &refresh);
        Ok(Api { client_id: client_id.to_string(), tok: Mutex::new((access, refresh, exp)) })
    }

    fn access(&self) -> Result<String, String> {
        let mut t = self.tok.lock().unwrap();
        if !t.0.is_empty() && now() + 60 < t.2 {
            return Ok(t.0.clone());
        }
        let v: Value = ureq::post("https://accounts.spotify.com/api/token")
            .send_form(&[("grant_type", "refresh_token"), ("refresh_token", &t.1), ("client_id", &self.client_id)])
            .map_err(|e| format!("refresh failed: {e}"))?
            .into_json()
            .map_err(|e| e.to_string())?;
        t.0 = v["access_token"].as_str().ok_or("no access token")?.to_string();
        if let Some(r) = v["refresh_token"].as_str() {
            t.1 = r.to_string(); // Spotify rotates refresh tokens
        }
        t.2 = now() + v["expires_in"].as_u64().unwrap_or(3600);
        save(&self.client_id, &t.1);
        Ok(t.0.clone())
    }

    fn url(path: &str) -> String {
        if path.starts_with("https://") { path.to_string() } else { format!("https://api.spotify.com/v1{path}") }
    }

    fn handle(r: Result<ureq::Response, ureq::Error>) -> Result<Value, String> {
        match r {
            Ok(resp) => {
                let s = resp.into_string().unwrap_or_default();
                Ok(if s.trim().is_empty() { Value::Null } else { serde_json::from_str(&s).unwrap_or(Value::Null) })
            }
            Err(ureq::Error::Status(code, resp)) => {
                let body: Value = resp.into_json().unwrap_or(Value::Null);
                let msg = body["error"]["message"].as_str().unwrap_or("").to_string();
                Err(format!("Spotify {code}: {msg}"))
            }
            Err(e) => Err(e.to_string()),
        }
    }

    pub fn get(&self, path: &str) -> Result<Value, String> {
        let a = self.access()?;
        Self::handle(ureq::get(&Self::url(path)).set("Authorization", &format!("Bearer {a}")).timeout(Duration::from_secs(10)).call())
    }

    pub fn put(&self, path: &str, body: Option<Value>) -> Result<Value, String> {
        let a = self.access()?;
        let req = ureq::put(&Self::url(path)).set("Authorization", &format!("Bearer {a}")).timeout(Duration::from_secs(10));
        Self::handle(match body {
            Some(b) => req.send_json(b),
            None => req.set("Content-Length", "0").call(),
        })
    }

    pub fn post(&self, path: &str) -> Result<Value, String> {
        let a = self.access()?;
        Self::handle(ureq::post(&Self::url(path)).set("Authorization", &format!("Bearer {a}")).set("Content-Length", "0").timeout(Duration::from_secs(10)).call())
    }

    /// The librespot device's id, if it's online.
    pub fn mixer_device(&self) -> Option<String> {
        let v = self.get("/me/player/devices").ok()?;
        v["devices"].as_array()?.iter().find(|d| d["name"].as_str() == Some(DEVICE)).and_then(|d| d["id"].as_str().map(String::from))
    }

    pub fn image(url: &str) -> Option<Vec<u8>> {
        let mut b = vec![];
        ureq::get(url).timeout(Duration::from_secs(10)).call().ok()?.into_reader().take(4_000_000).read_to_end(&mut b).ok()?;
        Some(b)
    }
}

/// URL-encode a query value.
pub fn q(s: &str) -> String {
    enc(s)
}
