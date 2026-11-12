//! `wavelink-mixer listen`: a headless listening session for #77. One capture child, no playback child, commands on
//! stdin (`save PATH`, `stop`, `stats`, `quit`), status on stderr, numbers on stdout.
#![allow(dead_code)]
use crate::lat::Summary;
use crate::notes::{save_once, Saved};
use crate::stemcfg::{Plays, SepEngine};
use crate::stems::{Opts, Session, Shared, MAC_TAP, MAC_TAP_MUTE, WAV_TAP};
use std::fs::OpenOptions;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc;
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
    /// `start` refuses over unsaved notes; `start new` (true) drops them.
    Start(bool),
    Stop,
    Stats,
    /// `lat`: the measured latency of the current stop (#83).
    Lat,
    Quit,
    Unknown(String),
}

/// One stdin line. `save` takes the rest of the line as the path.
pub fn cmd(line: &str) -> Cmd {
    let t = line.trim();
    let (w, rest) = t.split_once(' ').unwrap_or((t, ""));
    match w {
        "save" if !rest.is_empty() => Cmd::Save(rest.to_string()),
        "start" => Cmd::Start(rest.trim() == "new"),
        "stop" => Cmd::Stop,
        "stats" => Cmd::Stats,
        "lat" => Cmd::Lat,
        "quit" => Cmd::Quit,
        _ => Cmd::Unknown(w.to_string()),
    }
}

/// Why `start` must not go ahead: it would throw away unsaved notes (#75).
pub fn start_refusal(unsaved: usize, new: bool) -> Option<String> {
    (unsaved > 0 && !new).then(|| format!("listen: {unsaved} unsaved notes; save PATH first, or \"start new\" to drop them"))
}

/// A typed path: stdin gets no shell, so a leading `~/` is expanded here.
pub fn expand_home(p: &str) -> PathBuf {
    match (p.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(p),
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

pub(crate) fn start_session(tap: &str, engine: SepEngine, shared: &Shared, ctx: mpsc::Sender<(u64, [Vec<f32>; 3])>) -> Session {
    let opts = Opts { tap: tap.to_string(), tap_sink: false, engine, plays: Plays::Nothing, out: String::new(), buf_ms: 0, tier: None };
    Session::start(opts, shared.clone(), Some(ctx))
}

/// The `listen` command. Exit code.
pub fn run(args: &[String]) -> i32 {
    let (rest, speed) = match parse_speed(args) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let (tap, engine) = match parse(&rest) {
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
    // Read once per session by `Session::start`, so it must be set before the first start (#83).
    shared.speed.store(speed, Ordering::Relaxed);
    let (ctx, crx) = mpsc::channel::<(u64, [Vec<f32>; 3])>();
    let mut session: Option<Session> = Some(start_session(&tap, engine, &shared, ctx));
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
                        let path = expand_home(&path);
                        let f = OpenOptions::new().write(true).create_new(true).open(&path)?;
                        Ok((path, f))
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
                Cmd::Start(new) => {
                    let unsaved = shared.timeline.lock().unwrap_or_else(|p| p.into_inner()).unsaved();
                    if session.is_some() {
                        eprintln!("listen: already running");
                    } else if let Some(m) = start_refusal(unsaved, new) {
                        eprintln!("{m}");
                    } else {
                        let (ctx, crx) = mpsc::channel::<(u64, [Vec<f32>; 3])>();
                        session = Some(start_session(&tap, engine, &shared, ctx));
                        chunks = Some(crx);
                        meter = Meter::default();
                        println!("started");
                    }
                }
                Cmd::Stop => {
                    if let (Some(_), Some(rx)) = (session.as_ref(), chunks.as_ref()) {
                        poll(&mut meter, &shared, rx, &mut last_status, &mut warned);
                        let end = mark(&shared);
                        let state = timeline_bytes(&shared);
                        meter.end = Some((end, state));
                        // Drop first, so the child is reaped and its CPU and RSS are in the line.
                        session = None;
                        chunks = None;
                        println!("{}", stats_line(&stats_of(&meter, &shared)));
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
                Cmd::Lat => println!("{}", lat_line(speed, summary_of(&shared))),
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

/// Where Save MIDI writes by default: the Listen page's folder (`expand_home` resolves the `~`).
pub const MIDI_DIR: &str = "~/Music/wavelink-midi";

/// The `--speed N` flag (#83), taken out of the args before `parse` sees them. 0 is full processing (today), 3 the
/// fastest stop; anything else is an error.
pub fn parse_speed(args: &[String]) -> Result<(Vec<String>, usize), String> {
    const ERR: &str = "listen: --speed is 0 (full processing, the default) to 3 (fastest)";
    let mut rest = Vec::with_capacity(args.len());
    let mut speed = 0;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--speed" {
            let n: usize = it.next().and_then(|v| v.parse().ok()).ok_or(ERR.to_string())?;
            if n > 3 {
                return Err(ERR.to_string());
            }
            speed = n;
        } else {
            rest.push(a.clone());
        }
    }
    Ok((rest, speed))
}

/// The latency summary of the current stop, if any note has been measured.
fn summary_of(shared: &Shared) -> Option<Summary> {
    shared.lat.lock().unwrap_or_else(|p| p.into_inner()).summary()
}

/// The `lat` line (#83): the measured latency of the current stop, or `n=0` before any note.
pub fn lat_line(speed: usize, s: Option<Summary>) -> String {
    let guard = (crate::lat::guard(speed) * 1000.0).round() as u32;
    match s {
        Some(s) => format!(
            "lat speed={speed} guard_ms={guard} n={} min_ms={:.0} p50_ms={:.0} p95_ms={:.0} max_ms={:.0}",
            s.n, s.min, s.p50, s.p95, s.max
        ),
        None => format!("lat speed={speed} guard_ms={guard} n=0"),
    }
}

/// A new `listen-STAMP.mid` in `dir`: `-2`, `-3`, .. when the name is taken; never an overwrite (#83).
pub fn create_in(dir: &Path, stamp: &str) -> std::io::Result<(PathBuf, std::fs::File)> {
    std::fs::create_dir_all(dir)?;
    for i in 1..=99 {
        let name = if i == 1 { format!("listen-{stamp}.mid") } else { format!("listen-{stamp}-{i}.mid") };
        let p = dir.join(name);
        match OpenOptions::new().write(true).create_new(true).open(&p) {
            Ok(f) => return Ok((p, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "listen-STAMP names up to -99 are taken"))
}

/// The tap for the Listen page (#83): the WAV when one is given, else the Mac system tap, else none.
pub fn page_tap(wav: Option<String>, macos: bool) -> Option<String> {
    match wav {
        Some(f) => Some(format!("{WAV_TAP}{f}")),
        None if macos => Some(MAC_TAP.to_string()),
        None => None,
    }
}

/// One listening session driven by the Listen page: start, stop, poll and save over one `Shared` (#83).
pub struct Ctl {
    pub shared: Shared,
    pub speed: usize,
    tap: String,
    session: Option<Session>,
    chunks: Option<mpsc::Receiver<(u64, [Vec<f32>; 3])>>,
    peak: f32,
}

impl Ctl {
    pub fn new(tap: String) -> Ctl {
        Ctl { shared: Shared::new(), speed: 0, tap, session: None, chunks: None, peak: 0.0 }
    }

    pub fn running(&self) -> bool {
        self.session.is_some()
    }

    /// Refuses over unsaved notes unless `new`, as `start_refusal` does for the CLI.
    pub fn start(&mut self, new: bool) -> Result<(), String> {
        if self.running() {
            return Err("already running".into());
        }
        if let Some(m) = start_refusal(self.unsaved(), new) {
            return Err(m);
        }
        self.shared.speed.store(self.speed.min(3), Ordering::Relaxed);
        let (ctx, crx) = mpsc::channel::<(u64, [Vec<f32>; 3])>();
        self.session = Some(start_session(&self.tap, SepEngine::Live, &self.shared, ctx));
        self.chunks = Some(crx);
        self.peak = 0.0;
        Ok(())
    }

    pub fn stop(&mut self) {
        self.session = None;
        self.chunks = None;
    }

    /// Drains the level meter; true when the capture has been silent for five seconds.
    pub fn poll(&mut self) -> bool {
        if let Some(rx) = &self.chunks {
            while let Ok((_, stems)) = rx.try_recv() {
                for v in &stems {
                    for &x in v {
                        self.peak = self.peak.max(x.abs());
                    }
                }
            }
        }
        self.running() && silent(self.peak, self.shared.now())
    }

    pub fn save(&mut self, dir: &Path, stamp: &str) -> Result<Saved, String> {
        let running = self.running();
        save_once(&self.shared.timeline, running, || create_in(dir, stamp))
    }

    pub fn unsaved(&self) -> usize {
        self.shared.timeline.lock().map(|t| t.unsaved()).unwrap_or(0)
    }

    pub fn lat(&self) -> Option<Summary> {
        summary_of(&self.shared)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notes::Note;

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
    fn start_refuses_over_unsaved_notes() {
        assert!(start_refusal(5, false).unwrap().contains("5 unsaved notes"));
        assert_eq!(start_refusal(5, true), None);
        assert_eq!(start_refusal(0, false), None);
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
        assert_eq!(cmd("start"), Cmd::Start(false));
        assert_eq!(cmd("start new"), Cmd::Start(true));
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
    fn save_path_expands_a_typed_home() {
        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(expand_home("~/x.mid"), PathBuf::from(&home).join("x.mid"));
        }
        assert_eq!(expand_home("/tmp/x.mid"), PathBuf::from("/tmp/x.mid"));
        assert_eq!(expand_home("~other/x.mid"), PathBuf::from("~other/x.mid"));
    }

    #[test]
    fn silent_needs_five_seconds() {
        assert!(!silent(0.0, 4.9), "under five seconds is not judged");
        assert!(silent(0.0, 5.0));
        assert!(!silent(0.01, 10.0), "audible output is not silence");
    }


    #[test]
    fn parse_speed_takes_the_flag_out_and_defaults_to_0() {
        assert_eq!(parse_speed(&s(&["--wav", "a.wav", "--speed", "2"])), Ok((s(&["--wav", "a.wav"]), 2)));
        assert_eq!(parse_speed(&s(&["--wav", "a.wav"])), Ok((s(&["--wav", "a.wav"]), 0)));
    }

    #[test]
    fn parse_speed_rejects_out_of_range() {
        assert!(parse_speed(&s(&["--speed", "4"])).is_err());
        assert!(parse_speed(&s(&["--speed"])).is_err());
        assert!(parse_speed(&s(&["--speed", "x"])).is_err());
    }

    #[test]
    fn cmd_lat() {
        assert_eq!(cmd("lat"), Cmd::Lat);
    }

    #[test]
    fn lat_line_format() {
        let sm = Summary { n: 41, min: 212.0, p50: 431.0, p95: 655.0, max: 702.0 };
        assert_eq!(lat_line(3, Some(sm)), "lat speed=3 guard_ms=150 n=41 min_ms=212 p50_ms=431 p95_ms=655 max_ms=702");
        assert_eq!(lat_line(3, None), "lat speed=3 guard_ms=150 n=0");
    }

    #[test]
    fn create_in_never_overwrites() {
        let d = std::env::temp_dir().join(format!("wl83-{}-create", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let (a, _fa) = create_in(&d, "20260101-000000").expect("first file");
        let (b, _fb) = create_in(&d, "20260101-000000").expect("second file");
        assert_ne!(a, b);
        assert!(a.exists() && b.exists());
    }

    #[test]
    fn page_tap_prefers_the_wav_then_the_mac_tap() {
        assert_eq!(page_tap(Some("a.wav".into()), true), Some("wav:a.wav".to_string()));
        assert_eq!(page_tap(None, true), Some(MAC_TAP.to_string()));
        assert_eq!(page_tap(None, false), None);
    }

    #[test]
    fn ctl_start_refuses_over_unsaved_notes() {
        let mut c = Ctl::new("mac:system".into());
        c.shared.timeline.lock().unwrap().add(1, &[Note { t: 0.5, dur: 0.2, pitch: 60, vel: 90 }]);
        let e = c.start(false).expect_err("must refuse over unsaved notes");
        assert!(e.contains("unsaved"), "{e}");
        assert!(!c.running());
    }

    #[test]
    fn ctl_save_writes_the_closed_notes_into_the_folder() {
        let d = std::env::temp_dir().join(format!("wl83-{}-save", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let mut c = Ctl::new("mac:system".into());
        c.shared.timeline.lock().unwrap().add(1, &[Note { t: 0.5, dur: 0.2, pitch: 60, vel: 90 }]);
        let saved = c.save(&d, "20260101-000000").expect("save");
        assert_eq!(saved.per_stem[1], 1);
        assert!(saved.path.starts_with(&d) && saved.path.exists(), "{:?}", saved.path);
    }
}
