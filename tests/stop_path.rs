//! #83 stop path: `mac-tap` must exit cleanly on SIGTERM, so the Listen page's Stop can ask first and kill only
//! after the grace. Ignored by default: it needs the System Audio grant, so run it by hand on the Mac with
//! `cargo test -p wavelink-mixer --test stop_path -- --ignored`.
#![cfg(target_os = "macos")]
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_wavelink-mixer");

#[test]
#[ignore]
fn mac_tap_exits_cleanly_on_sigterm() {
    let mut c = Command::new(BIN).arg("mac-tap").stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    let mut out = c.stdout.take().unwrap();
    // Drain the audio so the child never blocks on a full pipe.
    std::thread::spawn(move || {
        let mut buf = [0u8; 65536];
        while matches!(out.read(&mut buf), Ok(n) if n > 0) {}
    });
    std::thread::sleep(Duration::from_secs(1));
    Command::new("kill").args(["-TERM", &c.id().to_string()]).status().unwrap();
    let t = Instant::now();
    let status = loop {
        if let Some(s) = c.try_wait().unwrap() {
            break s;
        }
        if t.elapsed() >= Duration::from_millis(500) {
            let _ = c.kill();
            panic!("mac-tap still running 500 ms after SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // On main the child dies by the signal, so `code()` is None.
    assert_eq!(status.code(), Some(0), "{status:?}");
}
