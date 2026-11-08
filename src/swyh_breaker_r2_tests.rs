//! Breaker round 2, output side (#15). Each test pins one suspected break in swyh.rs at 7e5a2b9.
//! Binds 127.0.0.1 port 0 only. Fake pw-record scripts live in a temp dir put first on PATH.
use super::*;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
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
        std::thread::sleep(Duration::from_millis(50));
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

fn fake_pw(tag: &str, body: &str) -> Fake {
    let dir = std::env::temp_dir().join(format!("breaker15r2-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("pw-record");
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
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

/// Writes its pid, then floods audio as fast as the reader takes it (never idle).
const FLOOD: &str = r#"echo $$ > "${0%/*}/pid"
exec /bin/dd if=/dev/zero bs=4096 2>/dev/null"#;

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

/// Kills one specific pid (never by pattern).
fn kill_pid(pid: &str) {
    let _ = Command::new("/bin/kill").args(["-9", pid]).status();
}

/// The bus is removed while the client has stopped reading (a stalled renderer, or a
/// half-open link). Audio keeps flowing, so the handler sits in `write_all`. `still_allowed`
/// is only reached between writes, so the stream should still close within about 1 s. Expected
/// to fail: the write only gives up after the 10 s send timeout.
#[test]
fn breaker_r2_stalled_reader_bus_removed_closes_within_1s() {
    let _g = lock();
    let fake = fake_pw("stall", FLOOD);
    let s = srv(&["n1"]);
    let mut c = get(s.port, "/stream/n1.wav");
    assert!(read_head(&mut c).starts_with("HTTP/1.1 200"));
    let pid = pid_of(&fake.dir).expect("pw-record pid");
    // let the socket buffers fill so the handler is really blocked in write_all
    std::thread::sleep(Duration::from_secs(2));
    s.allowed.lock().unwrap().clear();
    let t0 = Instant::now();
    let closed = wait_for(20, || CLIENTS.load(Ordering::SeqCst) == 0);
    let took = t0.elapsed();
    kill_pid(&pid);
    drop(c);
    wait_for(5, || CLIENTS.load(Ordering::SeqCst) == 0);
    assert!(closed, "bus removed, client not reading: stream still open after 20 s");
    assert!(took < Duration::from_secs(3), "bus removed, client not reading: stream closed after {took:?}, expected about 1 s (pw-record pid {pid})");
}

/// Same as above, but the client reads a little every 200 ms (a slow renderer). The handler's
/// `write_all` keeps making progress, so the 10 s send timeout never fires and the bus check
/// is never reached. Expected to fail: the stream stays open for the whole window.
#[test]
fn breaker_r2_trickle_reader_bus_removed_closes_within_3s() {
    let _g = lock();
    let fake = fake_pw("trickle", FLOOD);
    let s = srv(&["n1"]);
    let mut c = get(s.port, "/stream/n1.wav");
    assert!(read_head(&mut c).starts_with("HTTP/1.1 200"));
    let pid = pid_of(&fake.dir).expect("pw-record pid");
    s.allowed.lock().unwrap().clear();
    c.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    let mut b = [0u8; 16];
    let t0 = Instant::now();
    while CLIENTS.load(Ordering::SeqCst) != 0 && t0.elapsed() < Duration::from_secs(8) {
        let _ = c.read(&mut b);
        std::thread::sleep(Duration::from_millis(200));
    }
    let took = t0.elapsed();
    let closed = CLIENTS.load(Ordering::SeqCst) == 0;
    let still = alive(&pid);
    kill_pid(&pid);
    drop(c);
    wait_for(5, || CLIENTS.load(Ordering::SeqCst) == 0);
    assert!(closed && took < Duration::from_secs(3), "bus removed, client trickling: stream closed after {took:?} (closed={closed}), expected within about 3 s (pw-record pid {pid} alive={still})");
}

/// A client that half-closes its write side after the request (FIN sent, still reading) is
/// treated as gone: `peer_closed` peeks Ok(0), so the stream ends within 500 ms. A valid HTTP
/// client can do this, and it should keep receiving audio. Expected to fail.
#[test]
fn breaker_r2_half_closed_client_keeps_receiving_audio() {
    let _g = lock();
    let _fake = fake_pw("halfclose", FLOOD);
    let s = srv(&["n1"]);
    let mut c = TcpStream::connect(("127.0.0.1", s.port)).unwrap();
    write!(c, "GET /stream/n1.wav HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    c.shutdown(Shutdown::Write).unwrap();
    c.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
    let t0 = Instant::now();
    let mut late = 0usize;
    let mut eof_at: Option<Duration> = None;
    let mut buf = [0u8; 8192];
    while t0.elapsed() < Duration::from_secs(3) {
        match c.read(&mut buf) {
            Ok(0) => {
                eof_at = Some(t0.elapsed());
                break;
            }
            Ok(n) => {
                if t0.elapsed() >= Duration::from_secs(2) {
                    late += n;
                }
            }
            Err(_) => {}
        }
    }
    assert!(late > 0, "client half-closed after GET; server ended the stream at {eof_at:?} and sent nothing in the 2-3 s window");
}
