//! System monitor: health of the audio system itself (read-only, never changes anything).
//! Audio CPU, PipeWire open files vs its limit, buffer/latency, dropout warnings, services.
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub cpu: f64,          // % of one core: pipewire + pipewire-pulse + wireplumber + mixer engine
    pub fds: u64,          // PipeWire server open files
    pub fd_limit: u64,     // its soft limit
    pub rate: u64,
    pub quantum: u64,
    pub warnings: u64,     // xrun/underrun/resync warnings since PipeWire started
    pub services: Vec<(&'static str, bool)>,
}

/// Stats line under the clock: the quantum is one cycle of the graph, not a latency figure.
pub fn cycle_label(quantum: u64, rate: u64) -> String {
    todo!()
}

fn out(cmd: &str, args: &[&str]) -> String {
    crate::pw::output_timeout(cmd, args, Duration::from_secs(3)).map(|b| String::from_utf8_lossy(&b).to_string()).unwrap_or_default()
}

fn ticks(pid: u32) -> Option<u64> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &s[s.rfind(')')? + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    Some(f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?)
}

fn main_pid(unit: &str) -> Option<u32> {
    out("systemctl", &["--user", "show", "-p", "MainPID", "--value", unit]).trim().parse().ok().filter(|p| *p > 0)
}

/// Mixer engine's child audio process (the strips/routes).
fn engine_child() -> Option<u32> {
    let s = out("pgrep", &["-f", "pipewire -c .*wavelink-mixer/engine.conf"]);
    s.lines().next()?.trim().parse().ok()
}

pub fn spawn(tx: Sender<Stats>) {
    std::thread::spawn(move || {
        let mut prev: std::collections::HashMap<u32, u64> = Default::default();
        let mut last = Instant::now();
        let mut warnings = 0u64;
        let mut n = 0u32;
        loop {
            let mut st = Stats::default();
            // audio CPU (delta of utime+stime over the interval, 100 ticks/s)
            let pids: Vec<u32> = ["pipewire.service", "pipewire-pulse.service", "wireplumber.service", "wavelink-engine.service"]
                .iter()
                .filter_map(|u| main_pid(u))
                .chain(engine_child())
                .collect();
            let dt = last.elapsed().as_secs_f64().max(0.1);
            last = Instant::now();
            let mut used = 0u64;
            for p in &pids {
                if let Some(t) = ticks(*p) {
                    if let Some(old) = prev.get(p) {
                        used += t.saturating_sub(*old);
                    }
                    prev.insert(*p, t);
                }
            }
            st.cpu = used as f64 / dt; // ticks per second at 100 Hz == percent of one core
            // PipeWire open files vs limit
            if let Some(pw) = main_pid("pipewire.service") {
                st.fds = std::fs::read_dir(format!("/proc/{pw}/fd")).map(|d| d.count() as u64).unwrap_or(0);
                st.fd_limit = std::fs::read_to_string(format!("/proc/{pw}/limits"))
                    .ok()
                    .and_then(|l| l.lines().find(|x| x.starts_with("Max open files")).and_then(|x| x.split_whitespace().nth(3)?.parse().ok()))
                    .unwrap_or(1024);
            }
            // clock settings
            let meta = out("pw-metadata", &["-n", "settings"]);
            let val = |k: &str| meta.lines().find(|l| l.contains(&format!("'{k}'"))).and_then(|l| l.split("value:'").nth(1)?.split('\'').next()?.parse::<u64>().ok());
            st.rate = val("clock.rate").unwrap_or(48000);
            st.quantum = val("clock.force-quantum").filter(|q| *q > 0).or(val("clock.quantum")).unwrap_or(1024);
            // dropout warnings (every 10 s; journal reads are slower)
            if n % 5 == 0 {
                let since = out("systemctl", &["--user", "show", "-p", "ActiveEnterTimestamp", "--value", "pipewire.service"]);
                let log = out("journalctl", &["--user", "-u", "pipewire.service", "--since", since.trim(), "--no-pager", "-o", "cat"]);
                warnings = log.lines().filter(|l| {
                    let l = l.to_lowercase();
                    l.contains("xrun") || l.contains("underrun") || l.contains("resync")
                }).count() as u64;
            }
            st.warnings = warnings;
            // services
            let units = ["pipewire", "wireplumber", "wavelink-engine", "easyeffects", "wavelink-spotify"];
            let act = out("systemctl", &["--user", "is-active", "pipewire", "wireplumber", "wavelink-engine", "easyeffects", "wavelink-spotify"]);
            let states: Vec<bool> = act.lines().map(|l| l.trim() == "active").collect();
            let labels = ["PIPEWIRE", "WIREPLUMBER", "ENGINE", "EASYEFFECTS", "SPOTIFY"];
            for (k, _) in units.iter().enumerate() {
                st.services.push((labels[k], states.get(k).copied().unwrap_or(false)));
            }
            let discord = Command::new("pgrep").args(["-f", "/app/discord/Discord|DiscordCanary"]).stdout(Stdio::null()).status().map(|s| s.success()).unwrap_or(false);
            st.services.push(("DISCORD", discord));
            let portal = std::fs::read_to_string("/proc/self/mountinfo").map(|m| m.contains("/doc ") && m.contains("portal")).unwrap_or(false);
            st.services.push(("PORTAL", portal));
            if tx.send(st).is_err() {
                return;
            }
            n += 1;
            std::thread::sleep(Duration::from_secs(2));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_says_per_cycle() {
        let l = cycle_label(256, 48000);
        assert!(l.ends_with("5.3 ms per cycle"), "{l}");
        assert!(!l.contains("latency"), "{l}");
    }
}
