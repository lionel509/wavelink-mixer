//! Round-2 pins for #77: each goes red if its fix is reverted.
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_wavelink-mixer");

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("wl77-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A 16-bit stereo 44.1 kHz WAV of `secs` seconds of a plucked two-tone pattern.
fn write_wav(path: &std::path::Path, secs: usize) {
    let n = 44100 * secs;
    let mut d = Vec::with_capacity(n * 4);
    for i in 0..n {
        let t = i as f32 / 44100.0;
        let f = if (t * 4.0) as u32 % 2 == 0 { 220.0 } else { 330.0 };
        let v = ((t * f * std::f32::consts::TAU).sin() * 0.4 * (1.0 - (t * 4.0).fract())) * 32767.0;
        let b = (v as i16).to_le_bytes();
        d.extend_from_slice(&b);
        d.extend_from_slice(&b);
    }
    let mut h = Vec::new();
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&(36 + d.len() as u32).to_le_bytes());
    h.extend_from_slice(b"WAVEfmt ");
    h.extend_from_slice(&16u32.to_le_bytes());
    h.extend_from_slice(&1u16.to_le_bytes());
    h.extend_from_slice(&2u16.to_le_bytes());
    h.extend_from_slice(&44100u32.to_le_bytes());
    h.extend_from_slice(&(44100u32 * 4).to_le_bytes());
    h.extend_from_slice(&4u16.to_le_bytes());
    h.extend_from_slice(&16u16.to_le_bytes());
    h.extend_from_slice(b"data");
    h.extend_from_slice(&(d.len() as u32).to_le_bytes());
    h.extend_from_slice(&d);
    std::fs::write(path, h).unwrap();
}

/// Runs `listen args`, feeds each (delay, line), returns (stdout, stderr).
fn listen(args: &[&str], script: &[(u64, &str)]) -> (String, String) {
    let mut c = Command::new(BIN).arg("listen").args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let mut si = c.stdin.take().unwrap();
    for (ms, line) in script {
        std::thread::sleep(Duration::from_millis(*ms));
        let _ = writeln!(si, "{line}");
    }
    drop(si);
    let o = c.wait_with_output().unwrap();
    (String::from_utf8_lossy(&o.stdout).into(), String::from_utf8_lossy(&o.stderr).into())
}

#[test]
fn stop_line_includes_the_reaped_capture_child() {
    let d = tmp("a");
    let w = d.join("in.wav");
    write_wav(&w, 12);
    let (out, _) = listen(&["--wav", w.to_str().unwrap()], &[(12000, "stop"), (200, "quit")]);
    let line = out.lines().find(|l| l.starts_with("stats ")).expect("a stats line");
    let tap: f64 = line.split("cpu_tap_s=").nth(1).unwrap().split(' ').next().unwrap().parse().unwrap();
    assert!(tap > 0.0, "{line}");
}

#[test]
fn a_dead_capture_is_reported_and_the_loop_stops() {
    let t = std::time::Instant::now();
    let (_, err) = listen(&["--wav", "/nonexistent.wav"], &[(5000, "quit")]);
    assert!(err.contains("status: error: the capture exited 3 times"), "{err}");
    assert!(t.elapsed() < Duration::from_secs(15));
}

#[test]
fn start_refuses_over_unsaved_notes_then_save_and_start_new() {
    let d = tmp("start");
    let w = d.join("in.wav");
    write_wav(&w, 12);
    let p = d.join("x.mid");
    let save = format!("save {}", p.display());
    let (out, err) = listen(&["--wav", w.to_str().unwrap()], &[(12000, "stop"), (200, "start"), (200, &save), (200, "start new"), (200, "quit")]);
    assert!(err.contains("unsaved notes; save PATH first"), "{err}");
    assert!(out.contains("saved ") && p.exists(), "{out}");
    assert!(out.contains("started"), "{out}");
}

#[test]
fn demo_script_unmuted_with_no_flags_starts_listen() {
    let d = tmp("script");
    let rec = d.join("args.txt");
    let stub = d.join("stub.sh");
    std::fs::write(&stub, format!("#!/bin/sh\necho \"argc=$#\" > {}\nexit 1\n", rec.display())).unwrap();
    Command::new("chmod").arg("+x").arg(&stub).status().unwrap();
    let w = d.join("in.wav");
    write_wav(&w, 1);
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/listen-demo.sh");
    let o = Command::new("bash")
        .args([script, "live", "saves", w.to_str().unwrap(), d.join("o").to_str().unwrap(), "1"])
        .env("WAVELINK_BIN", &stub)
        .env("LISTEN_UNMUTED", "1")
        .output()
        .unwrap();
    let got = std::fs::read_to_string(&rec).unwrap_or_default();
    assert_eq!(got.trim(), "argc=1", "{got} / {}", String::from_utf8_lossy(&o.stdout));
}
