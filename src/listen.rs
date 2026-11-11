//! `wavelink-mixer listen`: a headless listening session for #77. One capture child, no playback child, commands on
//! stdin (`save PATH`, `stop`, `stats`, `quit`), status on stderr, numbers on stdout. Compile-only skeleton with the
//! tests first.
#![allow(dead_code)]
use crate::notes::Timeline;
use crate::stemcfg::SepEngine;

/// CPU seconds (user + system) and peak RSS in bytes. `who`: 0 for this process, -1 for reaped children.
pub fn usage(who: i32) -> (f64, u64) {
    let _ = who;
    unimplemented!("#77 U3: usage")
}

/// The tap target and engine for the arguments: `--wav FILE`, or on macOS `--mute` or not. `--engine live|quality`.
pub fn parse(args: &[String]) -> Result<(String, SepEngine), String> {
    let _ = args;
    unimplemented!("#77 U3: parse")
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
    let _ = line;
    unimplemented!("#77 U3: cmd")
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
    let _ = s;
    unimplemented!("#77 U3: stats_line")
}

/// Silence after five seconds of audio: the grant is probably missing.
pub fn silent(peak: f32, audio_s: f64) -> bool {
    let _ = (peak, audio_s);
    unimplemented!("#77 U3: silent")
}

/// The `listen` command. Exit code.
pub fn run(args: &[String]) -> i32 {
    let _ = (args, Timeline::new);
    unimplemented!("#77 U3: run")
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
