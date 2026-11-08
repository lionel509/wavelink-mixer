//! Breaker round 2, input side (#15). Each test pins one suspected break in netin.rs at 7e5a2b9.
//! Binds 127.0.0.1 port 0 only. A fake pw-play (writes its pid, then pipes stdin to cat) sits first on PATH.
use super::*;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

static LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn wait_until(secs: u64, mut f: impl FnMut() -> bool) -> bool {
    let end = std::time::Instant::now() + Duration::from_secs(secs);
    loop {
        if f() {
            return true;
        }
        if std::time::Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn alive(pid: &str) -> bool {
    std::process::Command::new("/bin/kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// A minimal 16-bit stereo 44.1 kHz WAV header with an empty data chunk.
fn wav_head() -> Vec<u8> {
    let mut v = b"RIFF".to_vec();
    v.extend(0u32.to_le_bytes());
    v.extend(b"WAVE");
    v.extend(b"fmt ");
    v.extend(16u32.to_le_bytes());
    v.extend(1u16.to_le_bytes());
    v.extend(2u16.to_le_bytes());
    v.extend(44100u32.to_le_bytes());
    v.extend((44100u32 * 4).to_le_bytes());
    v.extend(4u16.to_le_bytes());
    v.extend(16u16.to_le_bytes());
    v.extend(b"data");
    v.extend(0u32.to_le_bytes());
    v
}

/// Serves each accepted connection with `head`, then `body`, until the peer goes away.
fn serve_each(head: String, body: Vec<u8>, pace: Option<Duration>, limit: Duration) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || loop {
        let Ok((mut c, _)) = l.accept() else { return };
        let (head, body) = (head.clone(), body.clone());
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            let _ = c.read(&mut buf);
            let _ = c.write_all(head.as_bytes());
            let _ = c.write_all(&body);
            let t0 = std::time::Instant::now();
            while t0.elapsed() < limit {
                let Some(p) = pace else { break };
                if c.write_all(b"x").is_err() {
                    break;
                }
                std::thread::sleep(p);
            }
        });
    });
    port
}

/// A PATH holding only a fake pw-play that writes its pid to `<dir>/pid` and drains stdin.
fn fake_play(tag: &str) -> (PathBuf, Option<std::ffi::OsString>) {
    let dir = std::env::temp_dir().join(format!("breaker15r2-play-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("pw-play");
    std::fs::write(&p, "#!/bin/sh\necho $$ > \"${0%/*}/pid\"\nexec /bin/cat > /dev/null\n").unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    let old = std::env::var_os("PATH");
    std::env::set_var("PATH", &dir);
    (dir, old)
}

fn restore_path(dir: &PathBuf, old: Option<std::ffi::OsString>) {
    match old {
        Some(p) => std::env::set_var("PATH", p),
        None => std::env::remove_var("PATH"),
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// A stopped worker must release its pw-play child. The source here trickles one byte every
/// 100 ms, so `pump`'s `read_exact` (4 KiB) never returns and the stop flag is never checked.
/// The read timeout (10 s) never fires either, because each byte arrives in time.
/// Expected to fail: the player is still alive 4 s after stop.
#[test]
fn breaker_r2_stopped_worker_releases_player_on_trickling_source() {
    let _g = lock();
    // header plus 300 bytes at once (open() wants 256 bytes), then one byte every 100 ms
    let mut first = wav_head();
    first.extend(std::iter::repeat(0u8).take(300));
    let port = serve_each(
        "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nConnection: close\r\n\r\n".into(),
        first,
        Some(Duration::from_millis(100)),
        Duration::from_secs(30),
    );
    let (dir, old) = fake_play("trickle");
    let stop = Arc::new(AtomicBool::new(false));
    let (s2, url) = (stop.clone(), format!("http://127.0.0.1:{port}/s.wav"));
    let _th = std::thread::spawn(move || worker("n1", &url, &s2));
    let got = wait_until(5, || dir.join("pid").exists());
    let pid = std::fs::read_to_string(dir.join("pid")).unwrap_or_default().trim().to_string();
    std::thread::sleep(Duration::from_secs(1));
    stop.store(true, Ordering::Relaxed);
    let released = wait_until(4, || !alive(&pid));
    if alive(&pid) {
        let _ = std::process::Command::new("/bin/kill").args(["-9", &pid]).status();
    }
    restore_path(&dir, old);
    assert!(got && !pid.is_empty(), "fake pw-play never started");
    assert!(released, "worker was told to stop but pw-play pid {pid} is still running 4 s later");
}

/// Redirect variants beyond file://. Each must come back as an error (no panic, no hang, no Ok).
#[test]
fn breaker_r2_redirect_variants_are_errors() {
    let cases = [
        "gopher://127.0.0.1:1/x",
        "ftp://127.0.0.1:1/x",
        "data:audio/wav;base64,UklGRg==",
        "file:///etc/hosts?x=1",
        "//127.0.0.1:1/x",
    ];
    let mut bad = Vec::new();
    for loc in cases {
        let head = format!("HTTP/1.1 302 Found\r\nLocation: {loc}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let port = serve_each(head, vec![], None, Duration::from_secs(1));
        let url = format!("http://127.0.0.1:{port}/r");
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| open(&url).is_err()));
        if !matches!(r, Ok(true)) {
            bad.push(format!("{loc} -> {r:?}"));
        }
    }
    assert!(bad.is_empty(), "redirects not returned as errors: {bad:?}");
}

/// A redirect that points back at itself must end with an error in bounded time.
#[test]
fn breaker_r2_redirect_loop_ends_with_error() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let loc = format!("http://127.0.0.1:{port}/loop");
    std::thread::spawn(move || loop {
        let Ok((mut c, _)) = l.accept() else { return };
        let loc = loc.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            let _ = c.read(&mut buf);
            let _ = c.write_all(format!("HTTP/1.1 302 Found\r\nLocation: {loc}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes());
        });
    });
    let url = format!("http://127.0.0.1:{port}/loop");
    let t0 = std::time::Instant::now();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| open(&url).is_err()));
    let took = t0.elapsed();
    assert!(matches!(r, Ok(true)), "redirect loop: {r:?} after {took:?}");
    assert!(took < Duration::from_secs(10), "redirect loop took {took:?}");
}
