//! `wavelink-mixer listen`: a headless listening session for #77. One capture child, no playback child, commands on
//! stdin (`save PATH`, `stop`, `stats`, `quit`), status on stderr, numbers on stdout.
#![allow(dead_code)]
use crate::notes::save_once;
use crate::stemcfg::{Plays, SepEngine};
use crate::stems::{Opts, Session, Shared, MAC_TAP, MAC_TAP_MUTE, WAV_TAP};
use std::fs::OpenOptions;
use std::io::BufRead;
use std::path::PathBuf;
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

#[repr(C)]
struct Ru {
    utime: [i64; 2],
    stime: [i64; 2],
    maxrss: i64,
    rest: [i64; 13],
}

unsafe extern "C" {
    fn getrusage(who: i32, usage: *mut Ru) -> i32;
}

/// CPU seconds (user + system) and peak RSS in bytes. `who`: 0 for this process, -1 for reaped children.
pub fn usage(who: i32) -> (f64, u64) {
    let mut r = Ru { utime: [0; 2], stime: [0; 2], maxrss: 0, rest: [0; 13] };
    // SAFETY: `r` is a `repr(C)` rusage-shaped buffer (two timevals, then 14 longs), and getrusage only writes into it.
    unsafe { getrusage(who, &mut r) };
    // tv_usec sits in the low 32 bits of its i64 slot on macOS; it is always below 1e6, so the mask is right on Linux too.
    let secs = |t: [i64; 2]| t[0] as f64 + (t[1] & 0xffff_ffff) as f64 / 1e6;
    // macOS reports maxrss in bytes, Linux in KiB.
    let rss = if cfg!(target_os = "macos") { r.maxrss } else { r.maxrss * 1024 };
    (secs(r.utime) + secs(r.stime), rss.max(0) as u64)
}

/// The tap target and engine for the arguments: `--wav FILE`, or on macOS `--mute` or not. `--engine live|quality`.
pub fn parse(args: &[String]) -> Result<(String, SepEngine), String> {
    let mut wav: Option<String> = None;
    let mut mute = false;
    let mut eng = SepEngine::Live;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--wav" => wav = Some(it.next().ok_or("listen: --wav needs a FILE")?.clone()),
            "--mute" => mute = true,
            "--engine" => {
                eng = match it.next().map(|s| s.as_str()) {
                    Some("live") => SepEngine::Live,
                    Some("quality") => SepEngine::Quality,
                    _ => return Err("listen: --engine is live or quality".into()),
                }
            }
            other => return Err(format!("listen: unknown argument {other}")),
        }
    }
    let tap = match wav {
        Some(f) => format!("{WAV_TAP}{f}"),
        None if cfg!(target_os = "macos") => if mute { MAC_TAP_MUTE } else { MAC_TAP }.to_string(),
        None => return Err("listen: needs --wav FILE on this OS".into()),
    };
    Ok((tap, eng))
}

#[derive(Debug, PartialEq)]
pub enum Cmd {
    Save(String),
    Stop,
    Stats,
    Quit,
    Unknown(String),
}

/// One stdin line. `save` takes the rest of the line as the path.
pub fn cmd(line: &str) -> Cmd {
    let t = line.trim();
    let (w, rest) = t.split_once(' ').unwrap_or((t, ""));
    match w {
        "save" if !rest.is_empty() => Cmd::Save(rest.to_string()),
        "stop" => Cmd::Stop,
        "stats" => Cmd::Stats,
        "quit" => Cmd::Quit,
        _ => Cmd::Unknown(w.to_string()),
    }
}

/// The numbers the stats line reports.
pub struct Stats {
    pub audio_s: f64,
    pub wall_s: f64,
    pub cpu_self_s: f64,
    pub cpu_tap_s: f64,
    pub cpu_total_s: f64,
    pub rss_peak_mb: f64,
    pub tap_rss_mb: f64,
    pub state_bytes: usize,
    pub peak: f32,
}

/// `stats audio_s=.. wall_s=.. cpu_self_s=.. cpu_tap_s=.. cpu_per_min_s=.. cpu_total_s=.. rss_peak_mb=..
/// tap_rss_mb=.. state_bytes=.. state_bytes_per_hour=.. peak=..`, one line.
pub fn stats_line(s: &Stats) -> String {
    let per_min = if s.audio_s > 0.0 { (s.cpu_self_s + s.cpu_tap_s) / s.audio_s * 60.0 } else { 0.0 };
    let per_hour = if s.audio_s > 0.0 { s.state_bytes as f64 / s.audio_s * 3600.0 } else { 0.0 };
    format!(
        "stats audio_s={:.2} wall_s={:.2} cpu_self_s={:.3} cpu_tap_s={:.3} cpu_per_min_s={:.2} cpu_total_s={:.3} rss_peak_mb={:.1} tap_rss_mb={:.1} state_bytes={} state_bytes_per_hour={:.0} peak={:.4}",
        s.audio_s, s.wall_s, s.cpu_self_s, s.cpu_tap_s, per_min, s.cpu_total_s, s.rss_peak_mb, s.tap_rss_mb, s.state_bytes, per_hour, s.peak
    )
}

/// Silence after five seconds of audio: the grant is probably missing.
pub fn silent(peak: f32, audio_s: f64) -> bool {
    audio_s >= 5.0 && peak < 1e-4
}

/// One point on the clock: wall time, process CPU, and the audio counter.
#[derive(Clone, Copy)]
struct Mark {
    wall: Instant,
    cpu: f64,
    audio: f64,
}

/// What the session has measured so far. `first` is the first chunk; `end` is set at `stop`.
#[derive(Default)]
struct Meter {
    first: Option<Mark>,
    end: Option<(Mark, usize)>,
    peak: f32,
}

fn mark(shared: &Shared) -> Mark {
    Mark { wall: Instant::now(), cpu: usage(0).0, audio: shared.now() }
}

fn timeline_bytes(shared: &Shared) -> usize {
    shared.timeline.lock().unwrap_or_else(|p| p.into_inner()).state_bytes()
}

/// The stats line's numbers: the span runs from the first chunk to `stop` (or now).
fn stats_of(m: &Meter, shared: &Shared) -> Stats {
    let now = mark(shared);
    let (end, state) = match m.end {
        Some(e) => e,
        None => (now, timeline_bytes(shared)),
    };
    let first = m.first.unwrap_or(end);
    let (cpu_total, rss) = usage(0);
    let (tap_cpu, tap_rss) = usage(-1);
    Stats {
        audio_s: (end.audio - first.audio).max(0.0),
        wall_s: end.wall.saturating_duration_since(first.wall).as_secs_f64(),
        cpu_self_s: (end.cpu - first.cpu).max(0.0),
        cpu_tap_s: tap_cpu,
        cpu_total_s: cpu_total,
        rss_peak_mb: rss as f64 / 1048576.0,
        tap_rss_mb: tap_rss as f64 / 1048576.0,
        state_bytes: state,
        peak: m.peak,
    }
}

/// Drains the chunk stream (peak and the first-chunk baseline) and prints new status lines.
fn poll(m: &mut Meter, shared: &Shared, chunks: &mpsc::Receiver<(u64, [Vec<f32>; 3])>, last: &mut String, warned: &mut bool) {
    while let Ok((_, stems)) = chunks.try_recv() {
        for v in &stems {
            for &x in v {
                m.peak = m.peak.max(x.abs());
            }
        }
        if m.first.is_none() {
            m.first = Some(mark(shared));
        }
    }
    let s = shared.status.lock().map(|g| g.clone()).unwrap_or_default();
    if s != *last {
        eprintln!("status: {s}");
        *last = s;
    }
    if !*warned {
        if let Some(f) = m.first {
            if silent(m.peak, shared.now() - f.audio) {
                eprintln!("status: capture is silent: grant System Audio Recording to this terminal app");
                *warned = true;
            }
        }
    }
}

/// The `listen` command. Exit code.
pub fn run(args: &[String]) -> i32 {
    let (tap, engine) = match parse(args) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let (ltx, lrx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            match line {
                Ok(l) => {
                    if ltx.send(l).is_err() {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    });

    let shared = Shared::new();
    let (ctx, crx) = mpsc::channel::<(u64, [Vec<f32>; 3])>();
    let opts = Opts { tap, tap_sink: false, engine, plays: Plays::Nothing, out: String::new(), buf_ms: 0, tier: None };
    let mut session: Option<Session> = Some(Session::start(opts, shared.clone(), Some(ctx)));
    let mut chunks: Option<mpsc::Receiver<(u64, [Vec<f32>; 3])>> = Some(crx);
    let mut meter = Meter::default();
    let mut last_status = String::new();
    let mut warned = false;

    loop {
        let line = if session.is_some() {
            match lrx.recv_timeout(Duration::from_millis(200)) {
                Ok(l) => Some(l),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        } else {
            // Stopped: block on the command channel with no timeout, so an idle session costs no wakeups.
            match lrx.recv() {
                Ok(l) => Some(l),
                Err(_) => break,
            }
        };
        if let Some(line) = line {
            match cmd(&line) {
                Cmd::Save(path) => {
                    let running = session.is_some();
                    let res = save_once(&shared.timeline, running, || {
                        let f = OpenOptions::new().write(true).create_new(true).open(&path)?;
                        Ok((PathBuf::from(&path), f))
                    });
                    match res {
                        Ok(s) if s.path.as_os_str().is_empty() => println!("save: no notes (held={})", s.held),
                        Ok(s) => {
                            let [a, b, c] = s.per_stem;
                            let (first, last) = s.span.unwrap_or((0.0, 0.0));
                            println!(
                                "saved {} notes={} drums={a} bass={b} other={c} held={} first={first:.2} last={last:.2}",
                                s.path.display(),
                                a + b + c,
                                s.held
                            );
                        }
                        Err(e) => println!("save failed: {e} (notes kept)"),
                    }
                }
                Cmd::Stop => {
                    if let (Some(_), Some(rx)) = (session.as_ref(), chunks.as_ref()) {
                        poll(&mut meter, &shared, rx, &mut last_status, &mut warned);
                        let end = mark(&shared);
                        let state = timeline_bytes(&shared);
                        meter.end = Some((end, state));
                        let st = stats_of(&meter, &shared);
                        session = None;
                        chunks = None;
                        println!("{}", stats_line(&st));
                    } else {
                        eprintln!("listen: not running");
                    }
                }
                Cmd::Stats => {
                    if let (Some(_), Some(rx)) = (session.as_ref(), chunks.as_ref()) {
                        poll(&mut meter, &shared, rx, &mut last_status, &mut warned);
                    }
                    println!("{}", stats_line(&stats_of(&meter, &shared)));
                }
                Cmd::Quit => break,
                Cmd::Unknown(w) => eprintln!("listen: unknown command: {w}"),
            }
        }
        if let (Some(_), Some(rx)) = (session.as_ref(), chunks.as_ref()) {
            poll(&mut meter, &shared, rx, &mut last_status, &mut warned);
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn usage_reports_cpu_and_rss() {
        let (c0, _) = usage(0);
        let t = std::time::Instant::now();
        let mut x = 1u64;
        while t.elapsed() < std::time::Duration::from_millis(50) {
            x = std::hint::black_box(x.wrapping_mul(6364136223846793005).wrapping_add(1));
        }
        let (c1, rss) = usage(0);
        let d = c1 - c0;
        assert!((0.03..1.0).contains(&d), "a 50 ms busy loop cost {d} s");
        assert!((1 << 20..64u64 << 30).contains(&rss), "rss {rss} bytes");
    }

    #[test]
    fn parse_picks_the_tap() {
        let (tap, eng) = parse(&s(&["--wav", "/tmp/a.wav"])).unwrap();
        assert_eq!(tap, format!("{}/tmp/a.wav", crate::stems::WAV_TAP));
        assert_eq!(eng, SepEngine::Live);
        let (_, eng) = parse(&s(&["--wav", "/tmp/a.wav", "--engine", "quality"])).unwrap();
        assert_eq!(eng, SepEngine::Quality);
        if cfg!(target_os = "macos") {
            assert_eq!(parse(&s(&[])).unwrap().0, crate::stems::MAC_TAP);
            assert_eq!(parse(&s(&["--mute"])).unwrap().0, crate::stems::MAC_TAP_MUTE);
        } else {
            assert!(parse(&s(&[])).is_err(), "off macOS, listen needs --wav");
        }
    }

    #[test]
    fn cmd_parses_lines() {
        assert_eq!(cmd("save /tmp/out dir/x.mid"), Cmd::Save("/tmp/out dir/x.mid".to_string()));
        assert_eq!(cmd("stop"), Cmd::Stop);
        assert_eq!(cmd("stats"), Cmd::Stats);
        assert_eq!(cmd("quit"), Cmd::Quit);
        assert_eq!(cmd("dance"), Cmd::Unknown("dance".to_string()));
    }

    #[test]
    fn stats_line_format() {
        let st = Stats {
            audio_s: 60.0,
            wall_s: 60.0,
            cpu_self_s: 1.5,
            cpu_tap_s: 0.5,
            cpu_total_s: 2.25,
            rss_peak_mb: 120.0,
            tap_rss_mb: 30.0,
            state_bytes: 36000,
            peak: 0.5,
        };
        assert_eq!(
            stats_line(&st),
            "stats audio_s=60.00 wall_s=60.00 cpu_self_s=1.500 cpu_tap_s=0.500 cpu_per_min_s=2.00 cpu_total_s=2.250 rss_peak_mb=120.0 tap_rss_mb=30.0 state_bytes=36000 state_bytes_per_hour=2160000 peak=0.5000"
        );
    }

    #[test]
    fn silent_needs_five_seconds() {
        assert!(!silent(0.0, 4.9), "under five seconds is not judged");
        assert!(silent(0.0, 5.0));
        assert!(!silent(0.01, 10.0), "audible output is not silence");
    }
}
