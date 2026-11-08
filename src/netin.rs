//! Network input (#15): pulls a WAV or LPCM stream from a URL into a Virtual input's channel sink.
use crate::config::{names, Config, InputKind};
use crate::pw::Graph;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
pub struct Format {
    pub rate: u32,
    pub channels: u16,
    pub big_endian: bool,
    pub header_len: usize,
}

/// Why a stream could not be played. A `Format` refusal waits longer before retrying.
#[derive(Debug)]
enum Failure {
    Net(String),
    Format(String),
}

/// One worker per wanted input: keeps a pw-play child fed from its URL.
#[derive(Default)]
pub struct In {
    workers: HashMap<String, (String, Arc<AtomicBool>, std::thread::JoinHandle<()>)>,
}

impl In {
    pub fn tick(&mut self, cfg: &Config, g: &Graph) {
        self.sync(wanted(cfg, &|n| g.has(n)).into_iter().collect());
    }

    /// Makes the running workers match `want` (input id -> URL).
    fn sync(&mut self, want: HashMap<String, String>) {
        // stop workers that are no longer wanted or whose URL changed
        self.workers.retain(|id, (url, stop, _)| {
            let keep = want.get(id) == Some(url);
            if !keep {
                stop.store(true, Ordering::Relaxed);
            }
            keep
        });
        for (id, url) in want {
            if self.workers.contains_key(&id) {
                continue;
            }
            let stop = Arc::new(AtomicBool::new(false));
            let (w_id, w_url, w_stop) = (id.clone(), url.clone(), stop.clone());
            let th = std::thread::spawn(move || worker(&w_id, &w_url, &w_stop));
            self.workers.insert(id, (url, stop, th));
        }
    }
}

/// True for an http(s) URL of a sane length with no whitespace or control characters.
pub fn valid_url(url: &str) -> bool {
    (url.starts_with("http://") || url.starts_with("https://"))
        && (9..2048).contains(&url.len())
        && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

pub fn cli(args: &[String]) -> Option<i32> {
    match args.first().map(String::as_str) {
        Some("net-in") => {
            let (Some(name), Some(url)) = (args.get(1), args.get(2)) else {
                eprintln!("usage: wavelink-mixer net-in <name> <url>");
                return Some(1);
            };
            if !valid_url(url) {
                eprintln!("not a valid http(s) URL: {url}");
                return Some(1);
            }
            let mut cfg = Config::load();
            let id = cfg.add_stream_input(name, url);
            cfg.save();
            println!("{id}");
            Some(0)
        }
        _ => None,
    }
}

/// Works out the PCM format from the Content-Type and the first bytes of the body.
fn sniff(content_type: Option<&str>, head: &[u8]) -> Result<Format, String> {
    if head.len() >= 12 && &head[0..4] == b"RIFF" && &head[8..12] == b"WAVE" {
        let mut off = 12;
        let mut fmt: Option<(u16, u32)> = None; // (channels, rate)
        while off + 8 <= head.len() {
            let id = &head[off..off + 4];
            let size = u32::from_le_bytes([head[off + 4], head[off + 5], head[off + 6], head[off + 7]]) as usize;
            if id == b"fmt " {
                if off + 8 + 16 > head.len() {
                    return Err("WAV header cut short".into());
                }
                let b = &head[off + 8..off + 24];
                let format = u16::from_le_bytes([b[0], b[1]]);
                let channels = u16::from_le_bytes([b[2], b[3]]);
                let rate = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
                let bits = u16::from_le_bytes([b[14], b[15]]);
                if format != 1 && format != 0xFFFE {
                    return Err(format!("WAV format tag {format} is not PCM"));
                }
                if bits != 16 {
                    return Err(format!("WAV has {bits}-bit audio: only 16-bit is supported"));
                }
                if channels == 0 || rate == 0 {
                    return Err("WAV has no channels or no sample rate".into());
                }
                fmt = Some((channels, rate));
            } else if id == b"data" {
                let Some((channels, rate)) = fmt else {
                    return Err("WAV data chunk before fmt chunk".into());
                };
                let header_len = off + 8;
                if header_len > head.len() {
                    return Err("WAV header cut short".into());
                }
                return Ok(Format { rate, channels, big_endian: false, header_len });
            }
            off += 8 + size + (size & 1);
        }
        return Err("WAV header incomplete".into());
    }
    if let Some(ct) = content_type.filter(|c| c.to_ascii_lowercase().starts_with("audio/l16")) {
        let (mut rate, mut channels) = (44100u32, 2u16);
        for p in ct.split(';').map(|p| p.trim().to_ascii_lowercase()) {
            if let Some(v) = p.strip_prefix("rate=") {
                rate = v.parse().ok().filter(|r| *r > 0).unwrap_or(rate);
            } else if let Some(v) = p.strip_prefix("channels=") {
                channels = v.parse().ok().filter(|c| *c > 0).unwrap_or(channels);
            }
        }
        return Ok(Format { rate, channels, big_endian: true, header_len: 0 });
    }
    Err(format!(
        "unsupported stream type {:?}: only WAV and LPCM (audio/L16) streams work",
        content_type.unwrap_or("none")
    ))
}

/// Swaps each 16-bit sample from big-endian (L16) to little-endian.
fn to_le(buf: &mut [u8]) {
    for p in buf.chunks_exact_mut(2) {
        p.swap(0, 1);
    }
}

fn play_args(id: &str, f: &Format) -> Vec<String> {
    vec![
        "--target".into(),
        names::ch(id),
        "-P".into(),
        format!("{{ node.name=netin_{id} node.dont-fallback=true node.dont-reconnect=true }}"),
        "--format".into(),
        "s16".into(),
        "--rate".into(),
        f.rate.to_string(),
        "--channels".into(),
        f.channels.to_string(),
        "--raw".into(),
        "-".into(),
    ]
}

/// (id, url) for each Virtual input with a valid URL whose channel sink is present.
fn wanted(cfg: &Config, present: &dyn Fn(&str) -> bool) -> Vec<(String, String)> {
    cfg.inputs
        .iter()
        .filter(|i| matches!(i.kind, InputKind::Virtual))
        .filter_map(|i| {
            let url = cfg.net_in.get(&i.id)?;
            (valid_url(url) && present(&names::ch(&i.id))).then(|| (i.id.clone(), url.clone()))
        })
        .collect()
}

/// Connects and reads the stream head. Returns the format, the body bytes already read past
/// the header, and the reader for the rest. Uses timeouts per read, not `.timeout()`, which
/// would cap the whole stream.
fn open(url: &str) -> Result<(Format, Vec<u8>, Box<dyn Read + Send>), Failure> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(Duration::from_secs(10))
        .build();
    let resp = agent.get(url).call().map_err(|e| Failure::Net(e.to_string()))?;
    let ct = resp.header("Content-Type").map(str::to_string);
    let mut reader = resp.into_reader();
    let mut buf = vec![0u8; 256];
    let mut n = 0;
    while n < buf.len() {
        match reader.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) => return Err(Failure::Net(e.to_string())),
        }
    }
    buf.truncate(n);
    let fmt = sniff(ct.as_deref(), &buf).map_err(Failure::Format)?;
    let left = buf[fmt.header_len..].to_vec();
    Ok((fmt, left, reader))
}

/// Feeds the bytes already read, then 4 KiB chunks, into pw-play until stopped or failed.
fn pump(stdin: &mut dyn Write, reader: &mut dyn Read, left: &[u8], f: &Format, stop: &AtomicBool) -> Result<(), String> {
    let mut head = left.to_vec();
    if f.big_endian {
        to_le(&mut head);
    }
    stdin.write_all(&head).map_err(|e| format!("pw-play: {e}"))?;
    let mut buf = vec![0u8; 4096];
    while !stop.load(Ordering::Relaxed) {
        reader.read_exact(&mut buf).map_err(|e| format!("stream ended: {e}"))?;
        if f.big_endian {
            to_le(&mut buf);
        }
        stdin.write_all(&buf).map_err(|e| format!("pw-play: {e}"))?;
    }
    Ok(())
}

/// One connection: open, spawn pw-play on the channel, pump until stopped or failed.
/// Always kills and reaps the child before returning.
fn play(id: &str, url: &str, stop: &AtomicBool, last: &mut String) -> Result<(), Failure> {
    let (f, left, mut reader) = open(url)?;
    let mut child = crate::pw::die_with_parent(&mut Command::new("pw-play"))
        .args(play_args(id, &f))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| Failure::Net(format!("pw-play: {e}")))?;
    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(Failure::Net("pw-play: no stdin".into()));
    };
    last.clear();
    eprintln!("net in {id} <- {url} ({} Hz, {} ch)", f.rate, f.channels);
    let r = pump(&mut stdin, reader.as_mut(), &left, &f, stop);
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    r.map_err(Failure::Net)
}

fn worker(id: &str, url: &str, stop: &AtomicBool) {
    let mut last = String::new();
    while !stop.load(Ordering::Relaxed) {
        match play(id, url, stop, &mut last) {
            Ok(()) => break,
            Err(f) => {
                let (msg, pause) = match f {
                    Failure::Net(e) => (e, 3),
                    Failure::Format(e) => (e, 30),
                };
                if msg != last {
                    eprintln!("net in {id}: {msg}");
                    last = msg;
                }
                let t = Instant::now();
                while t.elapsed() < Duration::from_secs(pause) && !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "netin_tests.rs"]
mod tests;
