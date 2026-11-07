//! Discord local RPC client: reads who is in your current voice call (and who is
//! talking) and sets each person's volume inside the Discord app.
//! Needs your own Discord developer app (client id + secret, stored locally, 0600).
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
pub struct Person {
    pub id: String,
    pub name: String,
    pub volume: f64, // 0..200, 100 = normal
    pub muted: bool,
}

pub enum Event {
    Status(String),
    NeedSetup,
    Call(Option<String>, Vec<Person>), // channel name (None = not in a call), people
    Speaking(String, bool),
}

pub enum Cmd {
    Setup(String, String), // client id, client secret
    Volume(String, f64),
    Mute(String, bool),
}

fn state_path() -> PathBuf {
    crate::config::glib_home().join(".local/state/wavelink-mixer/discord.json")
}

fn load() -> Value {
    std::fs::read(state_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(json!({}))
}

fn save(v: &Value) {
    use std::os::unix::fs::OpenOptionsExt;
    let p = state_path();
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    if let Ok(mut f) = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&p) {
        let _ = f.write_all(&serde_json::to_vec_pretty(v).unwrap());
    }
}

fn sockets() -> Vec<PathBuf> {
    let rt = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/run/user/1000"));
    (0..10).map(|i| rt.join(format!("discord-ipc-{i}"))).filter(|p| p.exists()).collect()
}

struct Ipc {
    s: UnixStream,
    nonce: u64,
}

impl Ipc {
    fn connect(path: &PathBuf, client_id: &str) -> Result<Ipc, String> {
        let s = UnixStream::connect(path).map_err(|e| e.to_string())?;
        s.set_read_timeout(Some(Duration::from_millis(250))).ok();
        let mut ipc = Ipc { s, nonce: 0 };
        ipc.send(0, &json!({ "v": 1, "client_id": client_id }))?;
        let (_, v) = ipc.recv_blocking(Duration::from_secs(5))?;
        if v["evt"] != "READY" {
            return Err(format!("Discord refused the connection: {}", v["message"].as_str().or(v["data"]["message"].as_str()).unwrap_or("unknown")));
        }
        Ok(ipc)
    }

    fn send(&mut self, op: u32, v: &Value) -> Result<(), String> {
        let body = serde_json::to_vec(v).unwrap();
        let mut buf = Vec::with_capacity(8 + body.len());
        buf.extend_from_slice(&op.to_le_bytes());
        buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
        buf.extend_from_slice(&body);
        self.s.write_all(&buf).map_err(|e| e.to_string())
    }

    fn cmd(&mut self, cmd: &str, args: Value, evt: Option<&str>) -> Result<String, String> {
        self.nonce += 1;
        let nonce = format!("n{}", self.nonce);
        let mut m = json!({ "cmd": cmd, "args": args, "nonce": nonce });
        if let Some(e) = evt {
            m["evt"] = json!(e);
        }
        self.send(1, &m)?;
        Ok(nonce)
    }

    /// One frame, or None on timeout.
    fn recv(&mut self) -> Result<Option<(u32, Value)>, String> {
        let mut hdr = [0u8; 8];
        match self.s.read_exact(&mut hdr) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => return Ok(None),
            Err(e) => return Err(e.to_string()),
        }
        let op = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
        let len = u32::from_le_bytes(hdr[4..8].try_into().unwrap()) as usize;
        let mut body = vec![0u8; len];
        self.s.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let r = self.s.read_exact(&mut body);
        self.s.set_read_timeout(Some(Duration::from_millis(250))).ok();
        r.map_err(|e| e.to_string())?;
        if op == 2 {
            return Err("Discord closed the connection".into());
        }
        Ok(Some((op, serde_json::from_slice(&body).unwrap_or(Value::Null))))
    }

    fn recv_blocking(&mut self, limit: Duration) -> Result<(u32, Value), String> {
        let t = Instant::now();
        loop {
            if let Some(f) = self.recv()? {
                return Ok(f);
            }
            if t.elapsed() > limit {
                return Err("Discord didn't answer".into());
            }
        }
    }

    /// Wait for the reply to `nonce` (other frames are handed to `other`).
    fn reply(&mut self, nonce: &str, limit: Duration, other: &mut impl FnMut(Value)) -> Result<Value, String> {
        let t = Instant::now();
        loop {
            if let Some((_, v)) = self.recv()? {
                if v["nonce"] == nonce {
                    if v["evt"] == "ERROR" {
                        return Err(v["data"]["message"].as_str().unwrap_or("error").to_string());
                    }
                    return Ok(v["data"].clone());
                }
                other(v);
            }
            if t.elapsed() > limit {
                return Err("Discord didn't answer in time".into());
            }
        }
    }
}

fn token(form: &[(&str, &str)]) -> Result<Value, String> {
    let v: Value = ureq::post("https://discord.com/api/oauth2/token")
        .send_form(form)
        .map_err(|e| match e {
            ureq::Error::Status(c, r) => format!("Discord token error {c}: {}", r.into_string().unwrap_or_default()),
            e => e.to_string(),
        })?
        .into_json()
        .map_err(|e| e.to_string())?;
    Ok(v)
}

fn people(data: &Value) -> Vec<Person> {
    data["voice_states"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|vs| Person {
            id: vs["user"]["id"].as_str().unwrap_or("").to_string(),
            name: vs["nick"].as_str().or(vs["user"]["global_name"].as_str()).or(vs["user"]["username"].as_str()).unwrap_or("?").to_string(),
            volume: vs["volume"].as_f64().unwrap_or(100.0),
            muted: vs["mute"].as_bool().unwrap_or(false),
        })
        .collect()
}

/// Background worker: (re)connects, authenticates, follows your voice channel.
pub fn spawn(tx: Sender<Event>, rx: Receiver<Cmd>) {
    std::thread::spawn(move || {
        let mut pending: Option<(String, String)> = None;
        loop {
            let st = load();
            let (cid, secret) = match &pending {
                Some(p) => p.clone(),
                None => (st["client_id"].as_str().unwrap_or("").to_string(), st["client_secret"].as_str().unwrap_or("").to_string()),
            };
            if cid.is_empty() || secret.is_empty() {
                let _ = tx.send(Event::NeedSetup);
                match rx.recv() {
                    Ok(Cmd::Setup(a, b)) => pending = Some((a, b)),
                    Ok(_) => {}
                    Err(_) => return,
                }
                continue;
            }
            match session(&tx, &rx, &cid, &secret, &mut pending) {
                Ok(()) => return, // UI went away
                Err(e) => {
                    let _ = tx.send(Event::Status(e));
                    let _ = tx.send(Event::Call(None, vec![]));
                    // wait a bit, but still accept a new setup
                    let t = Instant::now();
                    while t.elapsed() < Duration::from_secs(5) {
                        match rx.recv_timeout(Duration::from_millis(200)) {
                            Ok(Cmd::Setup(a, b)) => {
                                pending = Some((a, b));
                                break;
                            }
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                            _ => {}
                        }
                    }
                }
            }
        }
    });
}

fn session(tx: &Sender<Event>, rx: &Receiver<Cmd>, cid: &str, secret: &str, pending: &mut Option<(String, String)>) -> Result<(), String> {
    let socks = sockets();
    if socks.is_empty() {
        return Err("Discord isn't running".into());
    }
    let mut st = load();
    // try each running Discord (regular, Canary…), preferring the one you're in a call on
    let mut chosen: Option<(Ipc, Value)> = None;
    let mut last_err = String::new();
    for path in &socks {
        let mut ipc = match Ipc::connect(path, cid) {
            Ok(i) => i,
            Err(e) => {
                last_err = e;
                continue;
            }
        };
        let mut noop = |_v: Value| {};
        // authenticate: saved token, refresh, or a fresh authorize prompt in Discord
        let mut access = st["access_token"].as_str().unwrap_or("").to_string();
        let mut authed = false;
        if !access.is_empty() {
            let n = ipc.cmd("AUTHENTICATE", json!({ "access_token": access }), None)?;
            authed = ipc.reply(&n, Duration::from_secs(5), &mut noop).is_ok();
        }
        if !authed {
            if let Some(rt) = st["refresh_token"].as_str().filter(|r| !r.is_empty()).map(String::from) {
                if let Ok(v) = token(&[("client_id", cid), ("client_secret", secret), ("grant_type", "refresh_token"), ("refresh_token", &rt)]) {
                    if let Some(a) = v["access_token"].as_str() {
                        access = a.to_string();
                        st["access_token"] = json!(a);
                        st["refresh_token"] = v["refresh_token"].clone();
                        let n = ipc.cmd("AUTHENTICATE", json!({ "access_token": access }), None)?;
                        authed = ipc.reply(&n, Duration::from_secs(5), &mut noop).is_ok();
                    }
                }
            }
        }
        if !authed {
            let _ = tx.send(Event::Status("Check Discord: click “Authorize” in the popup to let the Mixer control call volumes.".into()));
            let n = ipc.cmd("AUTHORIZE", json!({ "client_id": cid, "scopes": ["rpc", "rpc.voice.read", "rpc.voice.write"] }), None)?;
            let data = ipc.reply(&n, Duration::from_secs(120), &mut noop).map_err(|e| format!("Authorization failed: {e}"))?;
            let code = data["code"].as_str().ok_or("Discord returned no authorization code")?.to_string();
            let v = token(&[("client_id", cid), ("client_secret", secret), ("grant_type", "authorization_code"), ("code", &code), ("redirect_uri", "http://127.0.0.1")])?;
            access = v["access_token"].as_str().ok_or("Discord returned no access token")?.to_string();
            st["access_token"] = json!(access);
            st["refresh_token"] = v["refresh_token"].clone();
            let n = ipc.cmd("AUTHENTICATE", json!({ "access_token": access }), None)?;
            ipc.reply(&n, Duration::from_secs(5), &mut noop)?;
        }
        st["client_id"] = json!(cid);
        st["client_secret"] = json!(secret);
        save(&st);
        *pending = None;
        let n = ipc.cmd("GET_SELECTED_VOICE_CHANNEL", json!({}), None)?;
        let ch = ipc.reply(&n, Duration::from_secs(5), &mut noop).unwrap_or(Value::Null);
        let in_call = !ch.is_null();
        if chosen.is_none() || in_call {
            chosen = Some((ipc, ch));
        }
        if in_call {
            break;
        }
    }
    let (mut ipc, mut ch) = chosen.ok_or(last_err)?;
    let _ = tx.send(Event::Status(String::new()));
    let n = ipc.cmd("SUBSCRIBE", json!({}), Some("VOICE_CHANNEL_SELECT"))?;
    ipc.reply(&n, Duration::from_secs(5), &mut |_| {})?;
    let mut subscribed: Option<String> = None;
    let mut refresh = true;
    let mut last_poll = Instant::now();
    loop {
        // (re)subscribe to the call we're in
        let cur = ch["id"].as_str().map(String::from);
        if cur != subscribed {
            if let Some(id) = &cur {
                for e in ["SPEAKING_START", "SPEAKING_STOP", "VOICE_STATE_CREATE", "VOICE_STATE_UPDATE", "VOICE_STATE_DELETE"] {
                    ipc.cmd("SUBSCRIBE", json!({ "channel_id": id }), Some(e))?;
                }
            }
            subscribed = cur;
        }
        if refresh {
            let _ = tx.send(Event::Call(ch["name"].as_str().map(String::from).filter(|_| !ch.is_null()), people(&ch)));
            refresh = false;
        }
        // UI commands
        loop {
            match rx.try_recv() {
                Ok(Cmd::Volume(id, v)) => {
                    ipc.cmd("SET_USER_VOICE_SETTINGS", json!({ "user_id": id, "volume": v.clamp(0.0, 200.0) }), None)?;
                }
                Ok(Cmd::Mute(id, m)) => {
                    ipc.cmd("SET_USER_VOICE_SETTINGS", json!({ "user_id": id, "mute": m }), None)?;
                }
                Ok(Cmd::Setup(a, b)) => {
                    *pending = Some((a, b));
                    return Err("Reconnecting with new app details…".into());
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return Ok(()),
            }
        }
        // events
        if let Some((_, v)) = ipc.recv()? {
            match v["evt"].as_str().unwrap_or("") {
                "SPEAKING_START" => {
                    let _ = tx.send(Event::Speaking(v["data"]["user_id"].as_str().unwrap_or("").into(), true));
                }
                "SPEAKING_STOP" => {
                    let _ = tx.send(Event::Speaking(v["data"]["user_id"].as_str().unwrap_or("").into(), false));
                }
                "VOICE_STATE_CREATE" | "VOICE_STATE_UPDATE" | "VOICE_STATE_DELETE" | "VOICE_CHANNEL_SELECT" => {
                    let n = ipc.cmd("GET_SELECTED_VOICE_CHANNEL", json!({}), None)?;
                    ch = ipc.reply(&n, Duration::from_secs(5), &mut |_| {}).unwrap_or(Value::Null);
                    refresh = true;
                }
                _ => {}
            }
        }
        // safety poll in case an event was missed
        if last_poll.elapsed() > Duration::from_secs(10) {
            last_poll = Instant::now();
            let n = ipc.cmd("GET_SELECTED_VOICE_CHANNEL", json!({}), None)?;
            let fresh = ipc.reply(&n, Duration::from_secs(5), &mut |_| {}).unwrap_or(Value::Null);
            if fresh != ch {
                ch = fresh;
                refresh = true;
            }
        }
    }
}
