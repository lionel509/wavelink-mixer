//! Background engine: builds and supervises the PipeWire graph described by config.json.
//!
//!  apps -> ch_<in> (null sink) -> strip_<in> (filter-chain: FX preset + 3-band EQ, gain = stream volume)
//!       -> route_<in>_<bus> (loopback, level = volume, on/off = mute) -> bus_<bus> (null sink)
//!       -> hardware device (pw-loopback, waits for the device)  or  virtual mic (loopback -> Audio/Source)
//!
//! Null sinks live in the PipeWire server (lingering), so apps never lose their target.
//! Strips, routes and virtual outputs live in one child `pipewire -c` process that is
//! rebuilt when the structure changes. Hardware outputs are separate pw-loopback processes.
use crate::config::{names, BusKind, Config, InputKind};
use crate::pw::{self, Graph};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

/// How long a freshly spawned child graph may go without a missing node appearing before it counts
/// as stuck. Scales with graph size (each node costs a few ms of creation plus its buffer
/// negotiation) and backs off exponentially after consecutive restarts that did not produce a
/// stable graph. Capped at 60 s.
pub fn graph_patience(expected_nodes: usize, consecutive_restarts: u32) -> Duration {
    let base_ms = 8_000 + 150 * expected_nodes as u64;
    Duration::from_millis((base_ms << consecutive_restarts.min(4)).min(60_000))
}

/// Grace before restarting after a node vanished from a graph that had already been complete
/// (no creation race to wait for): about the pre-#22 behaviour.
pub const LOSS_GRACE: Duration = Duration::from_secs(5);
/// A graph must stay complete this long before earlier failed restarts are forgotten.
pub const STABLE_AFTER: Duration = Duration::from_secs(30);
/// Never restart a child younger than this.
pub const MIN_CHILD_AGE: Duration = Duration::from_secs(5);
/// An alive child that owns none of its expected nodes after this long is treated like a dead one.
pub const ZERO_NODES_AFTER: Duration = Duration::from_secs(10);
/// Consecutive restarts that leave the same set missing before the engine gives up on it.
pub const SAME_SET_RESTARTS: u32 = 3;
/// Stuck restarts per rolling hour that did not demonstrably work: beyond it the engine gives up on
/// what is missing, and further restarts (for names outside the given-up set) wait PENALTY_GAP.
pub const RESTART_CAP: usize = 6;
pub const PENALTY_GAP: Duration = Duration::from_secs(600);
/// Hard cap on stuck restarts per rolling hour, whether or not they worked: none is permitted
/// beyond it until the oldest one leaves the window.
pub const HARD_CAP: usize = 10;
/// Minimum gap between restarts: starts at MIN_GAP, doubles per restart up to MAX_GAP, halves per
/// DECAY_EVERY of complete graph.
pub const MIN_GAP: Duration = Duration::from_secs(5);
pub const MAX_GAP: Duration = Duration::from_secs(60);
pub const DECAY_EVERY: Duration = Duration::from_secs(300);
/// A dependency that has been away this long is considered gone for good: per-name clocks that
/// were kept across its absence are dropped and the "everything is up" level is lowered.
pub const DEPENDENCY_AWAY: Duration = Duration::from_secs(300);
/// While degraded, retry (one restart) only this rarely; a structure change retries at once.
pub const DEGRADED_RETRY: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// no child yet, or it exited
    Dead,
    /// config structure changed
    Structure,
    /// child alive but owns none of its expected nodes
    ZeroNodes,
    /// some nodes missing and not coming
    Stuck,
}

/// The restart policy for the child graph, kept free of I/O so it can be driven by a simulated
/// timeline (`run()` calls `tick` once per loop and `spawned` after (re)starting the child).
///
/// Reasons only decide WHEN a restart is wanted; ONE limiter decides whether it may happen.
///  1. Wanted: child dead; child owns none of its eligible nodes (10/20/40/60 s); or a missing node
///     has stayed missing longer than its patience. Every missing name has its own first-seen
///     clock (`first_seen`). A clock outlives the name's dependency being away (so one flapping
///     dependency cannot freeze the repair of the others) for up to DEPENDENCY_AWAY, then goes.
///     Patience: 8 s + 150 ms/node on a fresh spawn, doubling per consecutive failed restart
///     (cap 60 s); 5 s for a node lost from a graph that had completed.
///  2. Limiter for every automatic restart: a minimum gap since the previous one (5 s doubling to
///     60 s, halving per 5 min of complete graph); for stuck restarts at most HARD_CAP per rolling
///     hour (none beyond it until the oldest leaves the window); and PENALTY_GAP between them once
///     RESTART_CAP restarts in the hour have not demonstrably worked. A restart "worked" if the
///     graph completed within the fresh patience and stayed complete for 30 s; only the
///     RESTART_CAP count forgets it, the gap and HARD_CAP still count it.
///     Dead / zero-node restarts obey the gap but not the caps: nothing plays then, and degrading
///     would leave the mixer silent; the 60 s gap bounds the rate.
///  3. Degraded = giving up on the current missing set (3 consecutive restarts left it unchanged,
///     or a cap was reached while restarts keep failing; a loss after a stable graph is not that) and keeping the partial graph. Names outside it are still handled by
///     1-2. It ends after 30 s of complete graph, on a structure change, when a cap-caused one
///     finds both caps below their limits again, and (one attempt) after an hour; when a dependency
///     comes back, the names it had gated are dropped from the set. A young child gets no verdict.
///  4. A structure change is the user's action: it restarts at once and resets the limiter.
#[derive(Debug, Default)]
pub struct Supervisor {
    spawned_at: Option<Instant>,
    /// rule 1: when each currently missing name was first seen missing
    first_seen: std::collections::HashMap<String, Instant>,
    /// most eligible nodes seen (every dependency up), the last count, and since when we are below it
    max_eligible: usize,
    last_eligible: usize,
    below_since: Option<Instant>,
    /// names that left `missing` when a dependency went away (gated, not necessarily present)
    prev_missing: Vec<String>,
    gated: std::collections::HashSet<String>,
    /// rule 1: the graph was complete at some point of this child's life (5 s grace for losses)
    seen_complete: bool,
    complete_since: Option<Instant>,
    /// rule 1: consecutive failed restarts (patience backoff) and zero-node restarts (wait backoff)
    pub stuck_restarts: u32,
    zero_restarts: u32,
    /// rule 2: previous restart, the gap now required, stuck restarts in the last hour (`restart_log`)
    /// and those not shown to have worked (`failed_log`; `slot_open`: the newest may still be refunded)
    last_restart: Option<Instant>,
    gap: Duration,
    decays_done: u64,
    restart_log: Vec<Instant>,
    failed_log: Vec<Instant>,
    slot_open: bool,
    first_complete_age: Option<Duration>,
    /// rule 3
    same_set: u32,
    last_set: Vec<String>,
    degraded: Option<Vec<String>>,
    degraded_by_cap: bool,
}

impl Supervisor {
    /// Missing nodes the engine gave up restarting for (partial graph kept), if any.
    pub fn degraded(&self) -> Option<&[String]> {
        self.degraded.as_deref().filter(|d| !d.is_empty())
    }

    fn gap(&self) -> Duration {
        self.gap.max(MIN_GAP)
    }

    /// The patience that applies to the next stuck decision.
    pub fn patience(&self, eligible: usize) -> Duration {
        if self.seen_complete && self.stuck_restarts == 0 {
            LOSS_GRACE
        } else {
            graph_patience(eligible, self.stuck_restarts)
        }
    }

    pub fn spawned(&mut self, now: Instant) {
        self.spawned_at = Some(now);
        self.first_seen.clear();
        self.seen_complete = false;
        self.complete_since = None;
        self.first_complete_age = None;
    }

    fn gap_ok(&self, now: Instant) -> bool {
        self.last_restart.map_or(true, |t| now.saturating_duration_since(t) >= self.gap())
    }

    /// Record a permitted restart.
    fn commit(&mut self, now: Instant, stuck: bool) {
        self.last_restart = Some(now);
        self.gap = (self.gap() * 2).min(MAX_GAP);
        if stuck {
            self.restart_log.push(now);
            self.failed_log.push(now);
            self.slot_open = true;
        }
    }

    fn give_up(&mut self, missing: &[String], by_cap: bool) {
        self.degraded = Some(missing.to_vec());
        self.degraded_by_cap = by_cap;
        self.stuck_restarts = 0;
        self.same_set = 0;
        self.last_set.clear();
    }

    /// `missing`: expected child nodes that are absent (dependencies present); `eligible`: how many
    /// child nodes have their dependencies present (see `eligible_and_missing`). Returns the reason
    /// to restart now, if any.
    pub fn tick(&mut self, now: Instant, alive: bool, structure_changed: bool, missing: &[String], eligible: usize) -> Option<Why> {
        let age = self.spawned_at.map(|t| now.saturating_duration_since(t));
        let old_enough = age.map_or(true, |a| a > MIN_CHILD_AGE);
        let fresh_patience = graph_patience(eligible, 0);
        let hour = Duration::from_secs(3600);
        self.restart_log.retain(|t| now.saturating_duration_since(*t) < hour);
        self.failed_log.retain(|t| now.saturating_duration_since(*t) < hour);
        let hard = self.restart_log.len() >= HARD_CAP;
        let capped = self.failed_log.len() >= RESTART_CAP;

        // per-name clocks. While some dependency is away (eligible below its maximum) a name that is
        // not missing may just be gated, so its clock keeps running; once everything is up again, or
        // the dependency has been away for DEPENDENCY_AWAY, absent names are really present.
        self.max_eligible = self.max_eligible.max(eligible);
        if eligible < self.max_eligible {
            let since = *self.below_since.get_or_insert(now);
            if now.saturating_duration_since(since) >= DEPENDENCY_AWAY {
                self.max_eligible = eligible;
                self.below_since = None;
            }
        } else {
            self.below_since = None;
        }
        if eligible >= self.max_eligible {
            self.first_seen.retain(|n, _| missing.contains(n));
        }
        for m in missing {
            self.first_seen.entry(m.clone()).or_insert(now);
        }
        // a dependency went away: remember which names it gated. It came back: those names are no
        // longer given up on (the rest of the degraded set stays; an empty set still marks "capped")
        if eligible < self.last_eligible {
            for n in &self.prev_missing {
                if !missing.contains(n) {
                    self.gated.insert(n.clone());
                }
            }
        } else if eligible > self.last_eligible && self.last_eligible > 0 {
            if let Some(d) = &mut self.degraded {
                d.retain(|n| !self.gated.contains(n));
            }
            self.same_set = 0;
            self.last_set.clear();
            self.stuck_restarts = 0; // a new situation, not one more failed restart
        }
        if eligible >= self.max_eligible {
            self.gated.clear();
        }
        self.prev_missing = missing.to_vec();
        self.last_eligible = eligible;
        if self.degraded_by_cap && !capped && !hard {
            self.degraded = None;
        }

        if missing.is_empty() {
            self.seen_complete = true;
            self.first_complete_age = self.first_complete_age.or(age);
            let since = *self.complete_since.get_or_insert(now);
            let done = now.saturating_duration_since(since);
            let due = done.as_secs() / DECAY_EVERY.as_secs();
            while self.decays_done < due {
                self.gap = (self.gap() / 2).max(MIN_GAP);
                self.decays_done += 1;
            }
            if done >= STABLE_AFTER {
                if self.slot_open && self.first_complete_age.map_or(false, |a| a <= fresh_patience) {
                    self.failed_log.pop(); // that restart worked
                }
                self.slot_open = false;
                self.stuck_restarts = 0;
                self.same_set = 0;
                self.last_set.clear();
                self.degraded = None;
            }
        } else {
            self.complete_since = None;
            self.decays_done = 0;
        }

        if structure_changed {
            // the user's action: fresh limiter, fresh patience, nothing given up on
            self.stuck_restarts = 0;
            self.zero_restarts = 0;
            self.same_set = 0;
            self.last_set.clear();
            self.degraded = None;
            self.restart_log.clear();
            self.failed_log.clear();
            self.slot_open = false;
            self.gap = MIN_GAP;
            self.max_eligible = eligible;
            if !old_enough {
                return None;
            }
            self.commit(now, false);
            self.gap = MIN_GAP;
            return Some(Why::Structure);
        }
        if self.spawned_at.is_none() {
            return Some(Why::Dead); // very first spawn: not a restart
        }
        if !alive {
            if old_enough && self.gap_ok(now) {
                self.commit(now, false);
                return Some(Why::Dead);
            }
            return None;
        }
        let all_missing = eligible > 0 && missing.len() == eligible;
        if !all_missing {
            self.zero_restarts = 0;
        }
        if all_missing && old_enough {
            // a graph that owns none of its nodes is dead: capped backoff, never degraded
            let wait = (ZERO_NODES_AFTER * (1 << self.zero_restarts.min(3))).min(Duration::from_secs(60));
            if age.map_or(false, |a| a >= wait) && self.gap_ok(now) {
                self.zero_restarts += 1;
                self.commit(now, false);
                return Some(Why::ZeroNodes);
            }
            return None;
        }

        if let Some(d) = &self.degraded {
            if age.map_or(false, |a| a < fresh_patience) {
                return None; // a young child is still creating its nodes: no verdict yet
            }
            if missing.iter().all(|m| d.contains(m)) {
                if self.last_restart.map_or(false, |t| now.saturating_duration_since(t) >= DEGRADED_RETRY) {
                    // one more attempt, then degraded again if it fails
                    self.degraded = None;
                    self.same_set = SAME_SET_RESTARTS - 1;
                    self.last_set = missing.to_vec();
                } else {
                    return None;
                }
            }
        }
        let fresh: Vec<String> = missing.iter().filter(|m| self.degraded.as_ref().map_or(true, |d| !d.contains(m))).cloned().collect();
        let oldest = fresh.iter().filter_map(|m| self.first_seen.get(m)).map(|t| now.saturating_duration_since(*t)).max();
        if !old_enough || oldest.map_or(true, |o| o < self.patience(eligible)) {
            return None;
        }
        if self.same_set >= SAME_SET_RESTARTS {
            self.give_up(missing, false);
            return None;
        }
        if (capped || hard) && self.degraded.is_none() && self.stuck_restarts > 0 {
            // consecutive restarts keep failing and a cap is reached: stop tearing the graph down
            self.give_up(missing, true);
            return None;
        }
        let need = if capped { PENALTY_GAP.max(self.gap()) } else { self.gap() };
        if hard || self.last_restart.map_or(false, |t| now.saturating_duration_since(t) < need) {
            return None;
        }
        self.same_set = if fresh == self.last_set { self.same_set + 1 } else { 1 };
        self.last_set = fresh;
        self.stuck_restarts += 1;
        self.commit(now, true);
        Some(Why::Stuck)
    }
}

/// The child nodes the config expects, split the way the supervisor needs: how many have their
/// dependencies present (`eligible`), and which of those are absent (`missing`). One function so
/// run() and the tests cannot drift apart. A dependency on one of the child's own outputs is not a
/// dependency (it would appear and vanish with every spawn).
pub fn eligible_and_missing(cfg: &Config, has: impl Fn(&str) -> bool) -> (usize, Vec<String>) {
    let exp = expected(cfg);
    let own: std::collections::HashSet<&str> = exp.iter().map(|(n, _)| n.as_str()).collect();
    let mut eligible = 0;
    let mut missing = vec![];
    for (n, dep) in &exp {
        if dep.as_ref().map(|d| own.contains(d.as_str()) || has(d)).unwrap_or(true) {
            eligible += 1;
            if !has(n) {
                missing.push(n.clone());
            }
        }
    }
    (eligible, missing)
}

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
mod nofile {
    #[repr(C)]
    pub struct RLimit { pub cur: u64, pub max: u64 }
    extern "C" {
        pub fn getrlimit(res: std::ffi::c_int, r: *mut RLimit) -> std::ffi::c_int;
        pub fn setrlimit(res: std::ffi::c_int, r: *const RLimit) -> std::ffi::c_int;
    }
    pub const RLIMIT_NOFILE: std::ffi::c_int = 7;
    pub fn get() -> Option<(u64, u64)> {
        let mut r = RLimit { cur: 0, max: 0 };
        (unsafe { getrlimit(RLIMIT_NOFILE, &mut r) } == 0).then_some((r.cur, r.max))
    }
}

/// Make `cmd` start with the open-file soft limit raised to the hard limit (capped at 1<<20).
/// Every node and link of the graph costs several fds (memfds, eventfds, sockets) in the process
/// hosting it, and the default soft limit of 1024 is exhausted at ~8 inputs x 5 buses: "can't DUP
/// fd: Too many open files", then "invalid mem id" / "Buffer allocation failed". Only the command
/// is affected, so the engine and its other children keep the default (select()-safe) limit.
/// Returns the soft limit the child should get, and warns when that cannot be more than the
/// current one.
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
pub fn raise_nofile(cmd: &mut Command) -> Option<u64> {
    use std::os::unix::process::CommandExt;
    let (cur, max) = nofile::get()?;
    let want = max.min(1 << 20);
    if want <= cur {
        if cur < 4096 {
            eprintln!("warning: graph child fd limit stays {cur} (hard limit {max}); large graphs (>= 8 inputs x 5 buses) will fail with 'Too many open files'");
        }
        return Some(cur);
    }
    if max < 4096 {
        eprintln!("warning: hard fd limit is only {max}; large graphs may still run out of fds");
    }
    unsafe {
        cmd.pre_exec(|| {
            // only plain syscalls: safe between fork and exec
            if let Some((cur, max)) = nofile::get() {
                let want = max.min(1 << 20);
                if want > cur {
                    let r = nofile::RLimit { cur: want, max };
                    nofile::setrlimit(nofile::RLIMIT_NOFILE, &r);
                }
            }
            Ok(())
        });
    }
    Some(want)
}
#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
pub fn raise_nofile(_cmd: &mut Command) -> Option<u64> { None }

fn proc_nofile_soft(pid: u32) -> Option<u64> {
    let l = std::fs::read_to_string(format!("/proc/{pid}/limits")).ok()?;
    l.lines().find(|l| l.starts_with("Max open files"))?.split_whitespace().nth(3)?.parse().ok()
}

fn spawn_graph_child(conf: &std::path::Path) -> Option<Child> {
    let mut cmd = Command::new("pipewire");
    cmd.arg("-c").arg(conf).stdout(Stdio::null());
    // run() is on the long-lived main thread, so pdeathsig is safe: no orphan graph if we die
    pw::die_with_parent(&mut cmd);
    let want = raise_nofile(&mut cmd);
    match cmd.spawn() {
        Ok(c) => {
            if let (Some(w), Some(got)) = (want, proc_nofile_soft(c.id())) {
                if got < w {
                    eprintln!("warning: graph child fd soft limit is {got}, wanted {w} (setrlimit failed?)");
                }
            }
            Some(c)
        }
        Err(e) => {
            eprintln!("cannot spawn pipewire graph child: {e}");
            None
        }
    }
}

const CAPS: &str = "/usr/lib64/ladspa/caps.so";
const RNNOISE: &str = "/usr/lib64/ladspa/librnnoise_ladspa.so";
const DELAY: &str = "/usr/lib64/ladspa/delay.so";

type Fx = (&'static str, &'static str, &'static str, &'static [(&'static str, f64)]);

fn fx(kind: &'static str, name: &'static str, label: &'static str, ctl: &'static [(&'static str, f64)]) -> Fx {
    (kind, name, label, ctl)
}

fn preset(name: &str) -> Vec<Fx> {
    match name {
        // subsonic clean-up, bass shelf, small mud dip, headroom, safety limiter
        "music" => vec![
            fx("builtin", "hp", "bq_highpass", &[("Freq", 28.0), ("Q", 0.707)]),
            fx("builtin", "bass", "bq_lowshelf", &[("Freq", 105.0), ("Q", 0.707), ("Gain", 4.0)]),
            fx("builtin", "mud", "bq_peaking", &[("Freq", 320.0), ("Q", 1.0), ("Gain", -1.5)]),
            fx("builtin", "pad", "linear", &[("Mult", 0.79), ("Add", 0.0)]),
            fx("ladspa", "lim", "Compress", &[("measure", 0.0), ("mode", 1.0), ("threshold", 0.85), ("strength", 1.0), ("attack", 0.1), ("release", 0.4), ("gain (dB)", 0.0)]),
        ],
        // FPS: cut boom, lift footsteps, gentle compression, limiter
        "game" => vec![
            fx("builtin", "hp", "bq_highpass", &[("Freq", 35.0), ("Q", 0.707)]),
            fx("builtin", "boom", "bq_lowshelf", &[("Freq", 150.0), ("Q", 0.707), ("Gain", -5.0)]),
            fx("builtin", "steps", "bq_peaking", &[("Freq", 3200.0), ("Q", 0.7), ("Gain", 4.0)]),
            fx("ladspa", "comp", "Compress", &[("measure", 1.0), ("mode", 0.0), ("threshold", 0.35), ("strength", 0.4), ("attack", 0.4), ("release", 0.5), ("gain (dB)", 4.0)]),
            fx("ladspa", "lim", "Compress", &[("measure", 0.0), ("mode", 1.0), ("threshold", 0.85), ("strength", 1.0), ("attack", 0.1), ("release", 0.4), ("gain (dB)", 0.0)]),
        ],
        // light voice clean-up (the main mic chain is in EasyEffects)
        // other people's voices (Discord): denoise bad mics, take off harshness/sibilance,
        // level loud vs quiet people, then a safety limiter
        "voicefix" => vec![
            fx("rnnoise", "dn", "noise_suppressor_mono", &[("VAD Threshold (%)", 60.0), ("VAD Grace Period (ms)", 300.0), ("Retroactive VAD Grace (ms)", 50.0), ("Dry Mix", 0.0)]),
            fx("builtin", "hp", "bq_highpass", &[("Freq", 90.0), ("Q", 0.707)]),
            fx("builtin", "mud", "bq_peaking", &[("Freq", 300.0), ("Q", 1.0), ("Gain", -3.0)]),
            fx("builtin", "harsh", "bq_peaking", &[("Freq", 4000.0), ("Q", 0.9), ("Gain", -3.0)]),
            fx("builtin", "ess", "bq_peaking", &[("Freq", 7500.0), ("Q", 2.0), ("Gain", -4.0)]),
            fx("ladspa", "lev", "Compress", &[("measure", 1.0), ("mode", 0.0), ("threshold", 0.12), ("strength", 0.7), ("attack", 0.3), ("release", 0.5), ("gain (dB)", 8.0)]),
            fx("ladspa", "lim", "Compress", &[("measure", 0.0), ("mode", 1.0), ("threshold", 0.85), ("strength", 1.0), ("attack", 0.1), ("release", 0.4), ("gain (dB)", 0.0)]),
        ],
        "voice" => vec![
            fx("builtin", "hp", "bq_highpass", &[("Freq", 80.0), ("Q", 0.707)]),
            fx("ladspa", "lim", "Compress", &[("measure", 0.0), ("mode", 1.0), ("threshold", 0.85), ("strength", 1.0), ("attack", 0.1), ("release", 0.4), ("gain (dB)", 0.0)]),
        ],
        _ => vec![],
    }
}

fn graph_conf(fx_name: &str, eq: &[f64], rack: &crate::config::Rack) -> String {
    use crate::config::EQ_BANDS;
    // outboard rack first (preamp -> gate -> comp -> exciter), then the FX preset, then EQ
    let mut chain = vec![
        fx("builtin", "rk_trim", "linear", &[("Add", 0.0)]),
        fx("ladspa", "rk_sat", "Saturate", &[("bias", 0.0)]),
        fx("builtin", "rk_out", "linear", &[("Add", 0.0)]),
        fx("ladspa", "rk_gate", "Noisegate", &[("attack (ms)", 1.0), ("mains (Hz)", 0.0)]),
        fx("ladspa", "rk_comp", "Compress", &[("measure", 1.0), ("mode", 0.0)]),
        fx("ladspa", "rk_exc", "Spice", &[("lo.f (Hz)", 120.0), ("lo.compress", 0.5), ("hi.f (Hz)", 2500.0)]),
    ];
    let rack_ctl = rack.controls();
    chain.extend(preset(fx_name));
    let n_fx = chain.len();
    let last = EQ_BANDS.len() - 1;
    // names/labels must be 'static for the Fx tuple; leak the few small strings once per rebuild
    for (k, (f, _)) in EQ_BANDS.iter().enumerate() {
        let label = if k == 0 { "bq_lowshelf" } else if k == last { "bq_highshelf" } else { "bq_peaking" };
        let q = if k == 0 || k == last { 0.707 } else { 1.6 };
        let name: &'static str = Box::leak(format!("eq{k}").into_boxed_str());
        let ctl: &'static [(&'static str, f64)] = Box::leak(vec![("Freq", *f), ("Q", q)].into_boxed_slice());
        chain.push(fx("builtin", name, label, ctl));
    }
    let ports = |kind: &str| match kind {
        "builtin" => ("In", "Out"),
        "rnnoise" => ("Input", "Output"),
        _ => ("in", "out"),
    };
    let mut nodes = String::new();
    for (k, (kind, name, label, ctl)) in chain.iter().enumerate() {
        let mut c: String = ctl.iter().map(|(n, v)| format!("\"{n}\" = {v:?} ")).collect();
        if k >= n_fx {
            write!(c, "\"Gain\" = {:?} ", eq.get(k - n_fx).copied().unwrap_or(0.0)).unwrap();
        }
        for (key, v) in &rack_ctl {
            if let Some((node, ctl)) = key.split_once(':') {
                if node == *name {
                    write!(c, "\"{ctl}\" = {v:?} ").unwrap();
                }
            }
        }
        let (ty, plug) = match *kind {
            "ladspa" => ("ladspa", format!(" plugin = \"{CAPS}\"")),
            "rnnoise" => ("ladspa", format!(" plugin = \"{RNNOISE}\"")),
            k => (k, String::new()),
        };
        writeln!(nodes, "            {{ type = {ty} name = {name}{plug} label = {label} control = {{ {c}}} }}").unwrap();
    }
    let mut links = String::new();
    for w in chain.windows(2) {
        writeln!(links, "            {{ output = \"{}:{}\" input = \"{}:{}\" }}", w[0].1, ports(w[0].0).1, w[1].1, ports(w[1].0).0).unwrap();
    }
    // echo at the end: dry + three chained delay taps summed in a mixer (repeats fade by fb)
    let ctl_for = |node: &str| -> String {
        rack_ctl.iter().filter_map(|(k, v)| k.split_once(':').filter(|(n, _)| *n == node).map(|(_, c)| format!("\"{c}\" = {v:?} "))).collect()
    };
    for d in ["rk_d1", "rk_d2", "rk_d3"] {
        writeln!(nodes, "            {{ type = ladspa name = {d} plugin = \"{DELAY}\" label = delay_5s control = {{ {}}} }}", ctl_for(d)).unwrap();
    }
    writeln!(nodes, "            {{ type = builtin name = rk_echo label = mixer control = {{ {}}} }}", ctl_for("rk_echo")).unwrap();
    let (last, last_kind) = (chain.last().unwrap().1, chain.last().unwrap().0);
    let src = format!("{last}:{}", ports(last_kind).1);
    for (o, i) in [
        (src.as_str(), "rk_echo:In 1"),
        (src.as_str(), "rk_d1:Input"),
        ("rk_d1:Output", "rk_d2:Input"),
        ("rk_d2:Output", "rk_d3:Input"),
        ("rk_d1:Output", "rk_echo:In 2"),
        ("rk_d2:Output", "rk_echo:In 3"),
        ("rk_d3:Output", "rk_echo:In 4"),
    ] {
        writeln!(links, "            {{ output = \"{o}\" input = \"{i}\" }}").unwrap();
    }
    let first = format!("{}:{}", chain[0].1, ports(chain[0].0).0);
    format!("        filter.graph = {{\n          nodes = [\n{nodes}          ]\n          links = [\n{links}          ]\n          inputs = [ \"{first}\" ]\n          outputs = [ \"rk_echo:Out\" ]\n        }}\n")
}

/// Child-process config: strips, routes and virtual outputs.
fn child_conf(cfg: &Config) -> String {
    let mut s = String::from(
        "# generated by wavelink-mixer engine - edit ~/.config/wavelink-mixer/config.json instead\n\
         context.properties = { log.level = 0 }\n\
         context.spa-libs = { audio.convert.* = audioconvert/libspa-audioconvert  support.* = support/libspa-support }\n\
         context.modules = [\n\
         { name = libpipewire-module-rt args = { } flags = [ ifexists nofail ] }\n\
         { name = libpipewire-module-protocol-native }\n\
         { name = libpipewire-module-client-node }\n\
         { name = libpipewire-module-adapter }\n",
    );
    for i in &cfg.inputs {
        let (target, capsink) = match &i.kind {
            InputKind::Virtual => (names::ch(&i.id), "stream.capture.sink = true"),
            InputKind::Source { target } => (target.clone(), ""),
        };
        write!(
            s,
            "{{ name = libpipewire-module-filter-chain args = {{\n\
             \x20       node.description = \"{n} strip\"\n        media.name = \"{n} strip\"\n{g}\
             \x20       audio.channels = 2\n        audio.position = [ FL FR ]\n\
             \x20       capture.props = {{ node.name = \"{si}\" node.description = \"{n} (strip)\" target.object = \"{target}\" {capsink} node.dont-fallback = true }}\n\
             \x20       playback.props = {{ node.name = \"{so}\" node.description = \"{n} (post-FX)\" node.autoconnect = false }}\n}} }}\n",
            n = i.name,
            g = graph_conf(&i.fx, &i.eq, &i.rack),
            si = names::strip_in(&i.id),
            so = names::strip(&i.id),
        )
        .unwrap();
        for b in &cfg.buses {
            write!(
                s,
                "{{ name = libpipewire-module-loopback args = {{\n\
                 \x20       node.description = \"{n} -> {bn}\"\n        audio.position = [ FL FR ]\n\
                 \x20       capture.props = {{ node.name = \"{ri}\" node.description = \"{n} -> {bn}\" node.autoconnect = false stream.dont-remix = true }}\n\
                 \x20       playback.props = {{ node.name = \"{ro}\" node.description = \"{n} -> {bn}\" media.name = \"{n} -> {bn}\" target.object = \"{bus}\" node.dont-fallback = true stream.dont-remix = true }}\n}} }}\n",
                n = i.name,
                bn = b.name,
                ri = names::route_in(&i.id, &b.id),
                ro = names::route(&i.id, &b.id),
                bus = names::bus(&b.id),
            )
            .unwrap();
        }
    }
    for b in &cfg.buses {
        if let BusKind::Virtual { node, description } = &b.kind {
            write!(
                s,
                "{{ name = libpipewire-module-loopback args = {{\n\
                 \x20       node.description = \"{description}\"\n        audio.position = [ FL FR ]\n\
                 \x20       capture.props = {{ node.name = \"{cap}\" node.description = \"{bn} (bus monitor)\" target.object = \"{bus}\" stream.capture.sink = true node.dont-fallback = true stream.dont-remix = true }}\n\
                 \x20       playback.props = {{ node.name = \"{node}\" node.description = \"{description}\" media.class = \"Audio/Source\" priority.session = 30 wavelink.child_out = true }}\n}} }}\n",
                bn = b.name,
                cap = names::bus_cap(&b.id),
                bus = names::bus(&b.id),
            )
            .unwrap();
        }
    }
    s.push_str("]\n");
    s
}

/// Nodes the child process must provide; (node, dependency that must exist first).
fn expected(cfg: &Config) -> Vec<(String, Option<String>)> {
    let mut v = vec![];
    for i in &cfg.inputs {
        let dep = match &i.kind {
            InputKind::Virtual => names::ch(&i.id),
            InputKind::Source { target } => target.clone(),
        };
        // Both sides of the strip only appear once its source exists (e.g. EasyEffects
        // not running): don't treat them as missing, or the whole graph restarts in a loop.
        v.push((names::strip_in(&i.id), Some(dep.clone())));
        v.push((names::strip(&i.id), Some(dep)));
        for b in &cfg.buses {
            v.push((names::route(&i.id, &b.id), None));
        }
    }
    for b in &cfg.buses {
        if let BusKind::Virtual { node, .. } = &b.kind {
            v.push((node.clone(), None));
        }
    }
    v
}

/// Push every level/mute/EQ from the config into the graph.
pub fn apply_levels(cfg: &Config, g: &Graph) {
    use crate::config::db_to_vol;
    for i in &cfg.inputs {
        if let Some(n) = g.node(&names::strip_in(&i.id)) {
            pw::set_volume(n.id, db_to_vol(i.gain_db));
            pw::set_mute(n.id, i.mute);
            pw::set_eq(n.id, &i.eq);
            pw::set_rack(n.id, &i.rack);
        }
        for b in &cfg.buses {
            if let Some(n) = g.node(&names::route(&i.id, &b.id)) {
                let r = i.route(&b.id);
                pw::set_volume(n.id, db_to_vol(r.level_db));
                pw::set_mute(n.id, !r.on);
            }
        }
    }
    for b in &cfg.buses {
        if let Some(n) = g.node(&bus_fader_node(b)) {
            pw::set_volume(n.id, db_to_vol(b.gain_db));
            pw::set_mute(n.id, b.mute);
        }
    }
}

/// Which node a bus fader controls: the device itself for hardware buses, else the bus sink.
pub fn bus_fader_node(b: &crate::config::Bus) -> String {
    match &b.kind {
        BusKind::Hardware { target } => target.clone(),
        BusKind::Virtual { .. } => names::bus(&b.id),
    }
}

fn spawn_hw(id: &str, target: &str) -> Option<Child> {
    let cap = format!(
        "node.name={} node.description=bus-{id}-monitor target.object={} stream.capture.sink=true node.dont-fallback=true stream.dont-remix=true",
        names::bus_cap(id), names::bus(id)
    );
    let play = format!(
        "node.name={} node.description=bus-{id}-out target.object={target} node.dont-fallback=true stream.dont-remix=true",
        names::bus_out(id)
    );
    Command::new("pw-loopback")
        .args(["-n", &format!("wavelink-{id}"), "--capture-props", &cap, "--playback-props", &play])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()
}

/// The child-process config the engine would run (debugging aid).
pub fn dump_conf() -> String {
    child_conf(&Config::load())
}

pub const PIANO_ID: &str = "piano";

fn piano_font() -> Option<std::path::PathBuf> {
    let p = crate::config::glib_home().join(".local/share/wavelink/soundfonts/FluidR3_GM.sf2");
    p.exists().then_some(p)
}

/// FluidSynth playing the SL MkIII (any MIDI keyboard) into the Piano channel.
fn spawn_piano() -> Option<(Child, std::process::ChildStdin)> {
    let font = piano_font()?;
    let bin = ["/home/linuxbrew/.linuxbrew/bin/fluidsynth", "/usr/bin/fluidsynth"].into_iter().find(|b| std::path::Path::new(b).exists()).unwrap_or("fluidsynth");
    let props = format!(
        "{{ target.object={} node.name=piano_synth node.description=\"Piano synth\" application.name=Piano node.dont-fallback=true }}",
        names::ch(PIANO_ID)
    );
    let mut child = Command::new(bin)
        .env("PIPEWIRE_PROPS", props)
        .args(["-a", "pipewire", "-m", "alsa_seq", "-o", "midi.autoconnect=1", "-r", "48000", "-g", "0.6"])
        .arg(font)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdin = child.stdin.take()?;
    Some((child, stdin))
}

pub fn run() {
    let conf_dir = crate::config::glib_home().join(".local/state/wavelink-mixer");
    let _ = std::fs::create_dir_all(&conf_dir);
    let conf_file = conf_dir.join("engine.conf");

    let mut cfg = Config::load();
    let mut mtime: Option<SystemTime> = None;
    let mut child: Option<Child> = None;
    let mut child_structure = String::new();
    let mut levels_pending = false;
    let mut sup = Supervisor::default();
    let mut was_degraded = false;
    let degraded_file = conf_dir.join("engine.degraded");
    let _ = std::fs::remove_file(&degraded_file); // never keep a stale state from a previous engine
    let mut hw: HashMap<String, (Child, String)> = HashMap::new();
    // piano synth (FluidSynth) feeding the "piano" input channel; stdin kept open = stays alive
    let mut piano: Option<(Child, std::process::ChildStdin)> = None;
    let mut piano_sent: Option<crate::config::PianoCfg> = None;
    let mut last_quantum: Option<u32> = None;
    eprintln!("wavelink engine: config {}", Config::path().display());

    loop {
        // reload config on change
        let m = std::fs::metadata(Config::path()).and_then(|m| m.modified()).ok();
        if m != mtime {
            mtime = m;
            cfg = Config::load();
        }
        // graph quantum: forced through metadata (live, no restart), re-applied when the value changes
        if last_quantum != Some(cfg.quantum) && pw::force_quantum(cfg.quantum) {
            last_quantum = Some(cfg.quantum);
        }
        let Some(g) = Graph::read() else {
            std::thread::sleep(Duration::from_secs(1));
            continue;
        };

        // 1. server-side null sinks (create missing, remove ones no longer configured)
        let mut want: Vec<(String, String, u32)> = vec![];
        for i in &cfg.inputs {
            if i.kind == InputKind::Virtual {
                want.push((names::ch(&i.id), format!("{} (channel)", i.name), if i.default_sink { 900 } else { 100 }));
            }
        }
        for b in &cfg.buses {
            want.push((names::bus(&b.id), format!("{} (bus)", b.name), 10));
        }
        let mut created = false;
        for (name, desc, prio) in &want {
            if !g.has(name) {
                eprintln!("creating sink {name}");
                pw::create_null_sink(name, desc, *prio);
                created = true;
            }
        }
        for n in g.nodes.iter().filter(|n| n.owned) {
            if !want.iter().any(|w| w.0 == n.name) {
                eprintln!("removing sink {}", n.name);
                pw::destroy(n.id);
            }
        }
        if created {
            std::thread::sleep(Duration::from_millis(500));
            continue;
        }

        // 2. child process: (re)start on structure change, exit, or missing nodes
        let structure = cfg.structure();
        let alive = child.as_mut().map(|c| matches!(c.try_wait(), Ok(None))).unwrap_or(false);
        let (eligible, missing_names) = eligible_and_missing(&cfg, |n| g.has(n));
        let why = sup.tick(Instant::now(), alive, structure != child_structure, &missing_names, eligible);
        match (sup.degraded(), was_degraded) {
            (Some(d), false) => {
                eprintln!("degraded: still missing {d:?} after repeated restarts; keeping the partial graph, retrying in {} min or on a config structure change", DEGRADED_RETRY.as_secs() / 60);
                let _ = std::fs::write(&degraded_file, format!("{}\n", d.join("\n")));
                was_degraded = true;
            }
            (None, true) => {
                eprintln!("degraded state cleared");
                let _ = std::fs::remove_file(&degraded_file);
                was_degraded = false;
            }
            _ => {}
        }
        if let Some(why) = why {
            if let Some(mut c) = child.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
            if why == Why::Stuck || why == Why::ZeroNodes {
                eprintln!("restarting graph ({why:?}, attempt {}, next patience {:?}), missing: {:?}", sup.stuck_restarts, sup.patience(eligible), missing_names);
            }
            let _ = std::fs::write(&conf_file, child_conf(&cfg));
            child = spawn_graph_child(&conf_file);
            child_structure = structure;
            levels_pending = true;
            sup.spawned(Instant::now());
            std::thread::sleep(Duration::from_millis(1500));
            continue;
        }

        // 3. manual links strip -> routes
        for i in &cfg.inputs {
            for b in &cfg.buses {
                let (s, r) = (names::strip(&i.id), names::route_in(&i.id, &b.id));
                if g.has(&s) && g.has(&r) && !g.linked(&s, &r) {
                    pw::link(&s, &r);
                }
            }
        }

        // 4. hardware outputs
        for b in &cfg.buses {
            if let BusKind::Hardware { target } = &b.kind {
                let present = g.has(target);
                let running = hw.get_mut(&b.id).map(|(c, t)| t == target && matches!(c.try_wait(), Ok(None))).unwrap_or(false);
                if present && !running {
                    if let Some((mut c, _)) = hw.remove(&b.id) {
                        let _ = c.kill();
                        let _ = c.wait();
                    }
                    if let Some(c) = spawn_hw(&b.id, target) {
                        eprintln!("bus {} -> {target}", b.id);
                        hw.insert(b.id.clone(), (c, target.clone()));
                        levels_pending = true;
                    }
                } else if !present && running {
                    if let Some((mut c, _)) = hw.remove(&b.id) {
                        let _ = c.kill();
                        let _ = c.wait();
                    }
                }
            }
        }
        // 4b. piano synth: runs while a "piano" input exists and its channel sink is up
        let want_piano = cfg.inputs.iter().any(|i| i.id == PIANO_ID) && g.has(&names::ch(PIANO_ID)) && piano_font().is_some();
        let piano_alive = piano.as_mut().map(|(c, _)| matches!(c.try_wait(), Ok(None))).unwrap_or(false);
        if want_piano && !piano_alive {
            piano = spawn_piano();
            piano_sent = None;
            if piano.is_some() {
                eprintln!("piano synth started");
            }
        } else if !want_piano && piano.is_some() {
            if let Some((mut c, _)) = piano.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
        if let Some((_, stdin)) = piano.as_mut() {
            if piano_sent.as_ref() != Some(&cfg.piano) {
                use std::io::Write as _;
                let mut cmds = String::new();
                for ch in (0..16).filter(|c| *c != 9) {
                    cmds.push_str(&format!("prog {ch} {}\n", cfg.piano.program));
                }
                cmds.push_str(&format!("set synth.reverb.level {:.2}\n", cfg.piano.reverb.clamp(0.0, 1.0)));
                cmds.push_str(&format!("set synth.reverb.room-size {:.2}\n", 0.3 + 0.6 * cfg.piano.reverb.clamp(0.0, 1.0)));
                if stdin.write_all(cmds.as_bytes()).and_then(|_| stdin.flush()).is_ok() {
                    piano_sent = Some(cfg.piano.clone());
                }
            }
        }
        let stale: Vec<String> = hw.keys().filter(|k| !cfg.buses.iter().any(|b| &b.id == *k && matches!(b.kind, BusKind::Hardware { .. }))).cloned().collect();
        for k in stale {
            if let Some((mut c, _)) = hw.remove(&k) {
                let _ = c.kill();
                let _ = c.wait();
            }
        }

        // 5. levels once everything exists
        if levels_pending && missing_names.is_empty() {
            apply_levels(&cfg, &g);
            levels_pending = false;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patience_scales_with_size_and_backs_off() {
        assert!(graph_patience(56, 0) > Duration::from_secs(5) + Duration::from_millis(1500));
        assert!(graph_patience(130, 0) > graph_patience(56, 0));
        assert_eq!(graph_patience(56, 1), graph_patience(56, 0) * 2);
        assert_eq!(graph_patience(56, 2), Duration::from_secs(60));
        assert_eq!(graph_patience(56, 40), Duration::from_secs(60));
        assert_eq!(graph_patience(500, 9), Duration::from_secs(60));
    }

    #[test]
    fn slow_creation_within_patience_never_restarts() {
        let t0 = Instant::now();
        let mut sup = Supervisor::default();
        sup.spawned(t0);
        // 100 nodes appear one per 0.12 s: all up after 12 s, well inside the fresh patience
        for k in 0..400u64 {
            let t = t0 + Duration::from_millis(k * 120);
            let up = ((k * 120) / 120) as usize;
            let missing: Vec<String> = (up.min(100)..100).map(|i| format!("n{i}")).collect();
            assert_eq!(sup.tick(t, true, false, &missing, 100), None, "k={k}");
        }
    }

    #[test]
    fn restarts_when_stuck_and_never_when_complete() {
        let t0 = Instant::now();
        let mut sup = Supervisor::default();
        sup.spawned(t0);
        let m = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let p = graph_patience(56, 0);
        assert_eq!(sup.tick(t0 + Duration::from_secs(1), true, false, &m, 56), None);
        assert_eq!(sup.tick(t0 + p / 2, true, false, &m, 56), None);
        assert_eq!(sup.tick(t0 + Duration::from_secs(1) + p, true, false, &m, 56), Some(Why::Stuck));
        let mut sup = Supervisor::default();
        sup.spawned(t0);
        for k in 1..500u64 { assert_eq!(sup.tick(t0 + Duration::from_secs(k), true, false, &[], 56), None); }
        // the old rule gave a 28-node graph ~5.5 s; the fresh patience must now outlast that
        assert!(graph_patience(56, 0) > Duration::from_millis(5500));
    }

    /// A large synthetic config: the child conf names every expected node exactly once.
    #[test]
    fn large_config_conf_covers_expected_nodes() {
        let mut cfg = Config::default_config();
        let tmpl = cfg.inputs[0].clone();
        let bus = cfg.buses[0].clone();
        while cfg.inputs.len() < 10 { let mut i = tmpl.clone(); i.id = format!("x{}", cfg.inputs.len()); i.name = i.id.clone(); cfg.inputs.push(i); }
        while cfg.buses.len() < 6 { let mut b = bus.clone(); b.id = format!("y{}", cfg.buses.len()); b.name = b.id.clone(); cfg.buses.push(b); }
        let conf = child_conf(&cfg);
        for (n, _) in expected(&cfg) {
            if n.starts_with("route_") || n.starts_with("strip_") {
                assert_eq!(conf.matches(&format!("node.name = \"{n}\"")).count(), 1, "{n}");
            }
        }
        // per input: strip.in + strip + 6 routes; plus the virtual buses' own nodes
        let virt = cfg.buses.iter().filter(|b| matches!(b.kind, BusKind::Virtual { .. })).count();
        assert_eq!(expected(&cfg).len(), 10 * (2 + 6) + virt);
    }
}

#[cfg(test)]
mod supervisor_tests {
    use super::*;

    fn names(n: usize) -> Vec<String> { (0..n).map(|i| format!("n{i}")).collect() }
    #[test]
    fn raise_nofile_child_sees_raised_limit_and_parent_is_untouched() {
        let before = nofile::get().unwrap();
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "ulimit -Sn"]);
        let want = raise_nofile(&mut cmd).unwrap();
        let out = cmd.output().unwrap();
        let got: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
        assert_eq!(got, want);
        assert_eq!(got, before.1.min(1 << 20));
        assert_eq!(nofile::get().unwrap(), before, "engine's own limit must not change");
        // a plain child (pw-cat, pw-loopback...) keeps the engine's limit
        let plain = Command::new("sh").args(["-c", "ulimit -Sn"]).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&plain.stdout).trim().parse::<u64>().unwrap(), before.0);
    }

    #[test]
    fn structure_change_resets_backoff() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut sup = Supervisor::default();
        assert_eq!(sup.tick(at(0), false, false, &[], 50), Some(Why::Dead));
        sup.spawned(at(0));
        let m = names(1);
        let mut t = 1;
        let mut got = 0;
        while got < 2 {
            if sup.tick(at(t), true, false, &m, 50) == Some(Why::Stuck) { got += 1; sup.spawned(at(t)); }
            t += 1;
        }
        assert_eq!(sup.stuck_restarts, 2);
        assert_eq!(sup.tick(at(t + 10), true, true, &m, 50), Some(Why::Structure));
        assert_eq!(sup.stuck_restarts, 0);
        assert_eq!(sup.patience(50), graph_patience(50, 0));
    }

    #[test]
    fn dead_child_restarts_back_off_to_60s_but_never_degrade() {
        let t0 = Instant::now();
        let mut sup = Supervisor::default();
        sup.spawned(t0);
        let mut times = vec![];
        for k in 1..1200u64 {
            let t = t0 + Duration::from_secs(k);
            if sup.tick(t, false, false, &[], 50) == Some(Why::Dead) { times.push(k); sup.spawned(t); }
        }
        let gaps: Vec<u64> = times.windows(2).map(|w| w[1] - w[0]).collect();
        assert_eq!(&gaps[..5], &[10, 20, 40, 60, 60], "{times:?}");
        assert_eq!(sup.stuck_restarts, 0);
        assert!(sup.degraded().is_none());
    }

    #[test]
    fn alive_child_with_zero_nodes_restarts_promptly_without_backoff() {
        let t0 = Instant::now();
        let mut sup = Supervisor::default();
        sup.spawned(t0);
        let all = names(50);
        assert_eq!(sup.tick(t0 + Duration::from_secs(6), true, false, &all, 50), None, "creation grace");
        assert_eq!(sup.tick(t0 + Duration::from_secs(10), true, false, &all, 50), Some(Why::ZeroNodes));
        assert_eq!(sup.stuck_restarts, 0);
    }

    #[test]
    fn node_lost_after_complete_graph_uses_short_grace() {
        let t0 = Instant::now();
        let mut sup = Supervisor::default();
        sup.spawned(t0);
        for k in 1..=20u64 { assert_eq!(sup.tick(t0 + Duration::from_secs(k), true, false, &[], 130), None); }
        let m = names(1);
        let lost = t0 + Duration::from_secs(21);
        assert_eq!(sup.tick(lost, true, false, &m, 130), None);
        assert_eq!(sup.tick(lost + Duration::from_secs(5), true, false, &m, 130), Some(Why::Stuck));
    }

    #[test]
    fn brief_complete_graph_does_not_reset_backoff_but_stable_one_does() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut sup = Supervisor::default();
        sup.spawned(at(0));
        sup.stuck_restarts = 3;
        // complete for 29 s: still remembered
        for k in 0..30u64 { sup.tick(at(k), true, false, &[], 50); }
        assert_eq!(sup.stuck_restarts, 3);
        sup.tick(at(30), true, false, &[], 50);
        assert_eq!(sup.stuck_restarts, 0);
    }

    #[test]
    fn gives_up_on_the_same_missing_set_and_retries_rarely() {
        let t0 = Instant::now();
        let mut sup = Supervisor::default();
        let m = names(2);
        let mut t = 0u64;
        sup.spawned(t0);
        let mut restarts = 0;
        while t < 3 * 3600 {
            t += 1;
            if let Some(w) = sup.tick(t0 + Duration::from_secs(t), true, false, &m, 50) {
                assert_eq!(w, Why::Stuck);
                restarts += 1;
                sup.spawned(t0 + Duration::from_secs(t));
            }
        }
        // 3 attempts, then one retry per hour
        assert!((3..=6).contains(&restarts), "{restarts}");
        assert_eq!(sup.degraded(), Some(m.as_slice()));
    }

    #[test]
    fn new_loss_while_degraded_gets_normal_path_and_zero_nodes_never_degrades() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut sup = Supervisor::default();
        sup.spawned(t0);
        let old = names(2);
        let mut t = 0;
        while sup.degraded().is_none() {
            t += 1;
            assert!(t < 1000);
            if sup.tick(at(t), true, false, &old, 50).is_some() { sup.spawned(at(t)); }
        }
        // subset keeps holding
        assert_eq!(sup.tick(at(t + 500), true, false, &old[..1], 50), None);
        assert!(sup.degraded().is_some());
        // a new name is missing: leaves degraded, restarts within the fresh patience
        let mut grown = old.clone();
        grown.push("n9".into());
        let mut got = None;
        for k in 1..=40u64 {
            if let Some(w) = sup.tick(at(t + 600 + k), true, false, &grown, 50) { got = Some((k, w)); break; }
        }
        assert!(matches!(got, Some((k, Why::Stuck)) if k <= 20), "{got:?}");
        // every eligible node missing: ZeroNodes with growing, capped waits; never degraded
        let mut sup = Supervisor::default();
        sup.spawned(t0);
        let all = names(5);
        let (mut last, mut gaps) = (0u64, vec![]);
        for k in 1..2000u64 {
            if let Some(w) = sup.tick(at(k), true, false, &all, 5) {
                assert_eq!(w, Why::ZeroNodes);
                gaps.push(k - last); last = k;
                sup.spawned(at(k));
            }
            assert!(sup.degraded().is_none());
        }
        assert!(gaps.windows(2).all(|w| w[1] >= w[0]), "{gaps:?}");
        assert_eq!(*gaps.last().unwrap(), 60);
        assert_eq!(sup.stuck_restarts, 0);
    }

    #[test]
    fn rolling_hour_budget_degrades_a_flapping_node() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut sup = Supervisor::default();
        sup.spawned(at(0));
        let m = names(1);
        let mut restarts = 0;
        for t in 1..3500u64 {
            // gone 40 s, back 40 s, regardless of restarts
            let miss: &[String] = if (t / 40) % 2 == 1 { &m } else { &[] };
            if sup.tick(at(t), true, false, miss, 50).is_some() { restarts += 1; sup.spawned(at(t)); }
        }
        // a restart that happens to coincide with the node returning looks like success and is refunded,
        // so a flapping node may cost a few more than the budget, but stays bounded
        assert!(restarts <= HARD_CAP, "{restarts}");
    }

    #[test]
    fn eligible_and_missing_filters_by_dependency() {
        let cfg = Config::default_config();
        let has = |x: &str| x.starts_with("ch_") || x.starts_with("bus_"); // EasyEffects absent
        let (eligible, missing) = eligible_and_missing(&cfg, has);
        assert!(eligible < expected(&cfg).len(), "mic strips are gated by easyeffects_source");
        assert_eq!(missing.len(), eligible, "nothing of the child is up");
        assert!(!missing.iter().any(|n| n == "strip_mic"));
        let has2 = |x: &str| has(x) || x == "easyeffects_source";
        let (e2, m2) = eligible_and_missing(&cfg, has2);
        assert_eq!(e2, eligible + 2);
        assert!(m2.iter().any(|n| n == "strip_mic"));
    }

    #[test]
    fn restart_that_worked_is_refunded_and_a_new_loss_is_always_fixed() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut sup = Supervisor::default();
        sup.spawned(at(0));
        let mut restarts = vec![];
        let mut fixed = 0;
        for t in 1..8000u64 {
            // a different fault every 10 min, each fixed by its own restart
            let k = t / 600;
            let broken = k >= 1 && fixed < k && t % 600 >= 1;
            let m = if broken { vec![format!("r{k}")] } else { vec![] };
            if sup.tick(at(t), true, false, &m, 50).is_some() {
                restarts.push(t);
                fixed = k;
                sup.spawned(at(t));
            }
            assert!(sup.degraded().is_none(), "t={t}");
        }
        assert!(restarts.len() >= 12, "{restarts:?}");
        assert!(restarts.iter().all(|t| t % 600 <= 8), "{restarts:?}");
    }

    #[test]
    fn dependency_returning_is_repaired_within_grace_each_time() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let strips = names(2);
        let mut sup = Supervisor::default();
        sup.spawned(at(0));
        let mut t = 1;
        let mut restarts = 0;
        // EasyEffects restarts every 480 s (absent 3 s); the strips die when it returns
        let mut alive_strips = true;
        while t < 4 * 3600 {
            let dep = t % 480 >= 3;
            if t % 480 == 3 { alive_strips = false; }
            let (elig, m): (usize, Vec<String>) = if dep { (50, if alive_strips { vec![] } else { strips.clone() }) } else { (48, vec![]) };
            if sup.tick(at(t), true, false, &m, elig).is_some() { restarts += 1; alive_strips = true; sup.spawned(at(t)); }
            assert!(sup.degraded().is_none(), "t={t}");
            t += 1;
        }
        assert_eq!(restarts, 4 * 3600 / 480);
        assert_eq!(sup.stuck_restarts, 0);
    }

    #[test]
    fn flapping_dependency_does_not_freeze_repair_of_other_nodes() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut sup = Supervisor::default();
        sup.spawned(at(0));
        let mut x_lost: Option<u64> = None;
        let mut fixed_at = None;
        let mut spawned = 0u64;
        let mut t = 0u64;
        while t < 2000 {
            t += 1;
            let dep = t % 20 < 15; // 15 s up / 5 s away, forever
            let eligible = if dep { 50 } else { 48 };
            let mut m = vec!["perm".to_string()]; // never comes back
            if t >= 1000 && fixed_at.is_none() { x_lost.get_or_insert(t); m.push("x".into()); }
            if t < spawned + 2 { m = names(50); } // a fresh child owns nothing yet
            if sup.tick(at(t), true, false, &m, eligible).is_some() {
                if x_lost.is_some() && fixed_at.is_none() { fixed_at = Some(t); }
                sup.spawned(at(t));
                spawned = t;
            }
        }
        let lost = x_lost.unwrap();
        // the limiter may be in its slow-down (one restart per PENALTY_GAP) by then; what matters is that
        // the flapping dependency does not freeze repair for good
        assert!(fixed_at.map_or(false, |f| f - lost <= PENALTY_GAP.as_secs() + 30), "x lost at {lost}, restarted at {fixed_at:?}");
    }

    /// xorshift64*: no new crate, reproducible failures (the seed is in the assertion message).
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545F4914F6CDD1D)
        }
        fn below(&mut self, n: u64) -> u64 { self.next() % n }
        fn chance(&mut self, one_in: u64) -> bool { self.below(one_in) == 0 }
    }

    /// A tiny world: 12 child nodes, the last two (the "strips") gated by dependency D. A restart
    /// revives everything except `bad` nodes, and D's return kills the strips (as EasyEffects does).
    struct World { rng: Rng, present: [bool; 12], dep: bool, alive: bool, bad: Vec<usize>, structure_pending: bool, spawned: f64 }

    impl World {
        fn state(&self, t: f64) -> (Vec<String>, usize) {
            let young = t - self.spawned < 2.0;
            let elig: Vec<usize> = (0..12).filter(|i| self.dep || *i < 10).collect();
            (elig.iter().filter(|i| young || !self.present[**i]).map(|i| format!("r{i}")).collect(), elig.len())
        }
        fn restarted(&mut self, t: f64) {
            self.alive = true;
            self.structure_pending = false;
            self.spawned = t;
            for i in 0..12 { self.present[i] = !self.bad.contains(&i); }
        }
    }

    struct Outcome { stuck: Vec<f64>, structure: Vec<f64>, repair: f64, bound: f64, hour_at_probe: usize }

    /// Phase 1: random chaos (losses, returns, dependency flaps, child deaths, structure changes).
    /// Phase 2, by seed: A) 65 healthy minutes then one fixable loss; B) 5 healthy minutes then a loss
    /// (often right after the cap); C) the dependency flaps for 5 min and then stays up with the strips
    /// dead (recovery after a flap). The repair time is measured from the start of the probe.
    fn simulate(seed: u64, chaos_secs: f64) -> Outcome {
        let base = Instant::now();
        let at = |t: f64| base + Duration::from_secs_f64(t);
        let mut rng = Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1);
        let bad: Vec<usize> = (0..rng.below(3)).map(|_| rng.below(12) as usize).collect();
        let mut w = World { rng, present: [true; 12], dep: true, alive: true, bad, structure_pending: false, spawned: -100.0 };
        let mut sup = Supervisor::default();
        let (mut stuck, mut structure) = (vec![], vec![]);
        let mut last: Option<f64> = None;
        let mut t = 0.0;
        let mut first = true;
        let step = |w: &mut World, sup: &mut Supervisor, t: &mut f64, alive: bool, pending: bool, stuck: &mut Vec<f64>, structure: &mut Vec<f64>, last: &mut Option<f64>, first: &mut bool| -> bool {
            let (miss, elig) = w.state(*t);
            if let Some(why) = sup.tick(at(*t), alive, pending, &miss, elig) {
                if !*first {
                    if why == Why::Stuck { stuck.push(*t); }
                    if why == Why::Structure { structure.push(*t); }
                    // (b) the policy's floor: no two restarts closer than MIN_GAP (except a user's structure change)
                    if let (Some(prev), true) = (*last, why != Why::Structure) {
                        assert!(*t - prev + 1e-6 >= MIN_GAP.as_secs_f64(), "seed {seed}: {why:?} only {:.1}s after the previous restart", *t - prev);
                    }
                }
                *first = false;
                *last = Some(*t);
                w.restarted(*t);
                sup.spawned(at(*t));
                *t += 1.5;
                return true;
            }
            *t += 1.2;
            false
        };
        while t < chaos_secs {
            if w.rng.chance(33) {
                match w.rng.below(6) {
                    0 | 1 => { let i = w.rng.below(12) as usize; w.present[i] = false; }
                    2 => { let i = w.rng.below(12) as usize; if !w.bad.contains(&i) { w.present[i] = true; } }
                    3 => { w.dep = !w.dep; if w.dep { w.present[10] = false; w.present[11] = false; } }
                    4 => if w.rng.chance(4) { w.alive = false; },
                    _ => if w.rng.chance(40) { w.structure_pending = true; },
                }
            }
            let (alive, pending) = (w.alive, w.structure_pending);
            step(&mut w, &mut sup, &mut t, alive, pending, &mut stuck, &mut structure, &mut last, &mut first);
        }
        // phase 2
        w.bad.clear();
        w.present = [true; 12];
        w.dep = true;
        w.alive = true;
        let variant = seed % 3;
        if variant == 2 {
            let end = t + 300.0;
            let mut up = true;
            let mut next = t;
            while t < end {
                if t >= next { up = !up; next = t + 12.5; w.dep = up; if up { w.present[10] = false; w.present[11] = false; } }
                step(&mut w, &mut sup, &mut t, true, false, &mut stuck, &mut structure, &mut last, &mut first);
            }
            w.dep = true;
            w.present[10] = false;
            w.present[11] = false;
        } else {
            let quiet_until = t + if variant == 0 { 3900.0 } else { 300.0 };
            while t < quiet_until {
                step(&mut w, &mut sup, &mut t, true, false, &mut stuck, &mut structure, &mut last, &mut first);
            }
            w.present[3] = false;
        }
        let probe = t;
        let window_start = structure.iter().cloned().fold(probe - 3600.0, f64::max); // a user's structure change resets the limiter
        let hour_at_probe = stuck.iter().filter(|u| **u > window_start).count();
        // worst wait the limiter may impose: until the oldest restart of the hour leaves the window
        // (hard cap), and one PENALTY_GAP after the previous restart (slow-down), plus slack
        let mut wait = 0.0f64;
        if hour_at_probe >= HARD_CAP {
            let oldest = stuck.iter().filter(|u| **u > window_start).cloned().fold(f64::INFINITY, f64::min);
            wait = oldest + 3600.0 - probe;
        }
        if hour_at_probe >= RESTART_CAP {
            wait = wait.max(last.unwrap_or(probe) + PENALTY_GAP.as_secs_f64() - probe);
        }
        let bound = if hour_at_probe >= RESTART_CAP { wait + 70.0 } else if variant == 0 { 10.0 } else { MAX_GAP.as_secs_f64() + 20.0 };
        let mut repair = f64::INFINITY;
        while t < probe + bound + 120.0 {
            if step(&mut w, &mut sup, &mut t, true, false, &mut stuck, &mut structure, &mut last, &mut first) { repair = t - 1.5 - probe; break; }
        }
        Outcome { stuck, structure, repair, bound, hour_at_probe }
    }

    #[test]
    fn property_limiter_holds_for_random_event_sequences() {
        let (mut capped_probes, mut flap_probes, mut max_per_hour) = (0, 0, 0);
        for seed in 1..=1500u64 {
            let o = simulate(seed, 3600.0 * (1.0 + (seed % 5) as f64));
            // (a) stuck restarts in any rolling hour never exceed the hard cap (one more cap per user structure change)
            for (i, t) in o.stuck.iter().enumerate() {
                let n = o.stuck[i..].iter().take_while(|u| **u < *t + 3599.9).count();
                let resets = o.structure.iter().filter(|u| **u >= *t && **u < *t + 3599.9).count();
                max_per_hour = max_per_hour.max(n);
                assert!(n <= HARD_CAP * (1 + resets), "seed {seed}: {n} stuck restarts within an hour of t={t:.0} ({resets} structure changes)");
            }
            // (c) after the chaos, a single fixable loss (or the strips dead after a dependency flap) is repaired
            // within ~10 s on a long-healthy graph, within the limiter's own bound otherwise
            assert!(o.repair <= o.bound, "seed {seed}: repaired after {:.1}s, bound {:.0}s ({} stuck restarts in the hour before the probe)", o.repair, o.bound, o.hour_at_probe);
            if o.hour_at_probe >= RESTART_CAP { capped_probes += 1; }
            if seed % 3 == 2 { flap_probes += 1; }
        }
        eprintln!("property: max stuck restarts in an hour {max_per_hour}; {capped_probes} probes after the cap, {flap_probes} after a dependency flap");
        assert!(capped_probes > 20, "the generator should reach the cap now and then");
    }

    /// R5-1: EasyEffects flaps (25 s cycle) for `flap` seconds, then stays up; the strips are dead after
    /// every return and only a restart revives them. Returns (seconds from the last return until the
    /// strips were revived, restarts in total).
    fn flap_then_stable(flap: u64) -> (u64, usize) {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let strips = names(2);
        let mut sup = Supervisor::default();
        sup.spawned(at(0));
        let (mut spawned, mut alive_strips, mut restarts) = (0u64, true, 0);
        let mut last_return = 0;
        for t in 1..(flap + 4000) {
            let dep = t >= flap || t % 25 < 15; // 15 s up / 10 s away while flapping
            if dep && (t == flap || (t < flap && t % 25 == 0)) { alive_strips = false; last_return = t; }
            let (elig, m) = match (dep, t < spawned + 2) {
                (_, true) => (50, names(50)),
                (true, _) => (50, if alive_strips { vec![] } else { strips.clone() }),
                (false, _) => (48, vec![]),
            };
            if sup.tick(at(t), true, false, &m, elig).is_some() {
                restarts += 1;
                spawned = t;
                alive_strips = dep;
                sup.spawned(at(t));
                if t >= flap && dep { return (t - last_return, restarts); }
            }
        }
        (u64::MAX, restarts)
    }

    #[test]
    fn mic_recovers_after_a_dependency_flap_stops() {
        for flap in [120, 300, 600, 1200] {
            let (back, restarts) = flap_then_stable(flap);
            assert!(back <= PENALTY_GAP.as_secs() + 70, "flap {flap}s: strips revived {back}s after the dependency settled");
            assert!(restarts <= HARD_CAP, "flap {flap}s: {restarts} restarts");
        }
        // a short flap leaves the limiter fresh: recovery inside gap + patience
        assert!(flap_then_stable(120).0 <= MAX_GAP.as_secs() + 20);
    }

    #[test]
    fn structure_change_clears_degraded() {
        let t0 = Instant::now();
        let mut sup = Supervisor::default();
        let m = names(2);
        sup.spawned(t0);
        let mut t = 0;
        while sup.degraded().is_none() {
            t += 1;
            assert!(t < 1000);
            if sup.tick(t0 + Duration::from_secs(t), true, false, &m, 50).is_some() { sup.spawned(t0 + Duration::from_secs(t)); }
        }
        assert_eq!(sup.tick(t0 + Duration::from_secs(t + 10), true, true, &m, 50), Some(Why::Structure));
        assert!(sup.degraded().is_none());
    }
}

/// Breaker round 1 for PR #27 (#22). `sim` drives the real `Supervisor` (the restart policy `run()`
/// uses) with a scripted missing-node timeline.
#[cfg(test)]
mod breaker27 {
    use super::*;

    const TICK: f64 = 1.2; // 1 s sleep + pw-dump
    const AFTER_RESTART: f64 = 1.5;

    struct Ev { t: f64, stuck: bool }

    /// `missing(t, since_spawn, restarts_so_far)`; `structure_change_at`: times at which the config
    /// structure changes (forces a restart like run() does).
    fn sim(n: usize, total: f64, structure_change_at: &[f64], mut missing: impl FnMut(f64, f64, usize) -> usize) -> Vec<Ev> {
        let base = Instant::now();
        let at = |t: f64| base + Duration::from_secs_f64(t);
        let mut sup = Supervisor::default();
        let mut child_started = -60.0f64;
        let mut pending_struct: Vec<f64> = structure_change_at.to_vec();
        let mut ev: Vec<Ev> = vec![];
        let mut t = 0.0;
        while t < total {
            let m = missing(t, t - child_started, ev.len());
            let names: Vec<String> = (0..m).map(|i| format!("n{i}")).collect();
            let struct_changed = pending_struct.first().map_or(false, |s| t >= *s);
            // alive: the first spawn happens when there is no child yet (spawned_at None => Dead)
            if let Some(why) = sup.tick(at(t), true, struct_changed, &names, n) {
                if why == Why::Structure { pending_struct.remove(0); }
                if child_started > -59.0 { ev.push(Ev { t, stuck: why == Why::Stuck }); }
                child_started = t;
                sup.spawned(at(t));
                t += AFTER_RESTART;
                continue;
            }
            t += TICK;
        }
        ev
    }

    fn cfg_nxm(nin: usize, nbus: usize) -> Config {
        let mut cfg = Config::default_config();
        let tmpl = cfg.inputs[0].clone();
        let bus = cfg.buses.iter().find(|b| matches!(b.kind, BusKind::Virtual { .. })).unwrap().clone();
        cfg.inputs.truncate(nin.min(cfg.inputs.len()));
        while cfg.inputs.len() < nin { let mut i = tmpl.clone(); i.id = format!("x{}", cfg.inputs.len()); i.name = i.id.clone(); cfg.inputs.push(i); }
        cfg.buses.truncate(nbus.min(cfg.buses.len()));
        while cfg.buses.len() < nbus {
            let mut b = bus.clone(); let k = cfg.buses.len();
            b.id = format!("y{k}"); b.name = b.id.clone();
            b.kind = BusKind::Virtual { node: format!("vmix{k}"), description: b.id.clone() };
            cfg.buses.push(b);
        }
        cfg
    }

    // ---------------- red: real defects ----------------

    /// F1: a node that a restart cannot bring back (server-side fd/mem failure, i.e. the server's
    /// own 1024 limit the PR leaves alone, or a child whose limit could not be raised because
    /// hard == soft) makes the engine tear the WHOLE working graph down forever, every <=120 s.
    /// Restarting is pointless after the same set came back missing a few times in a row.
    #[test]
    fn red_permanent_missing_node_restarts_forever() {
        let n = expected(&cfg_nxm(8, 5)).len();
        let ev = sim(n, 3600.0, &[], |_, since, _| if since < 1.0 { n } else { 1 });
        let stuck = ev.iter().filter(|e| e.stuck).count();
        assert!(stuck <= 4, "{stuck} full-graph restarts (audio cut each time) in 1 h for one permanently missing node; last at {:.0}s", ev.last().map_or(0.0, |e| e.t));
    }

    /// F2: a node lost from a graph that has been COMPLETE for minutes (not a slow fresh spawn,
    /// so there is no creation race to wait for) is now left broken 3x longer than before at
    /// 8x5, and >2x longer even on the small default config. Old rule: ~4 ticks (~5 s).
    #[test]
    fn red_steady_state_loss_restart_latency() {
        for (label, cfg) in [("default", Config::default_config()), ("8x5", cfg_nxm(8, 5))] {
            let n = expected(&cfg).len();
            let mut fixed = false;
            let ev = sim(n, 900.0, &[], |t, _, r| {
                if r >= 1 { fixed = true; }
                if t >= 600.0 && !fixed { 1 } else { 0 }
            });
            let first = ev.iter().find(|e| e.t >= 600.0).map(|e| e.t - 600.0).unwrap_or(f64::INFINITY);
            assert!(first <= 6.5, "{label} ({n} nodes): node lost at t=600 after a complete graph, restart only after {first:.1}s (old rule ~5 s)");
        }
    }

    /// F3: stuck_restarts (the x2..x16 backoff) survives a structure change. A user who edits the
    /// config after a stuck period gets a brand-new graph that inherits up to 120 s patience.
    #[test]
    fn red_backoff_survives_structure_change() {
        let n = expected(&cfg_nxm(8, 5)).len();
        // graph A is permanently stuck for 10 min, structure change at t=600; graph B also has a
        // missing node (different cause) that a restart would fix
        let mut b_spawned_at: Option<usize> = None;
        let ev = sim(n, 1200.0, &[600.0], |t, since, r| {
            if since < 1.0 { return n; }
            if t < 600.0 { return 1; }
            // r0 = restarts before the structure restart; B is the graph after it
            let r0 = *b_spawned_at.get_or_insert(r);
            if r > r0 + 1 { 0 } else { 1 }
        });
        let sc = ev.iter().find(|e| e.t >= 600.0).expect("structure restart").t;
        let next = ev.iter().find(|e| e.t > sc).map(|e| e.t - sc).unwrap_or(f64::INFINITY);
        let fresh = graph_patience(n, 0).as_secs_f64() + 3.0;
        assert!(next <= fresh, "new structure stuck for {next:.0}s before its first restart; a fresh graph should get ~{:.0}s", fresh - 3.0);
    }

    /// F0 (live-confirmed): in the #22 failure mode itself (child still at 1024 fds, e.g. hard ==
    /// soft so the pre_exec raise is a no-op, or the server's own limit at large sizes) every node
    /// first APPEARS (graph complete ~2 s after spawn), then the fd-starved ones die. The brief
    /// complete graph resets stuck_restarts, so the x2..x16 backoff never engages: a restart every
    /// ~18 s at 8x5 forever, each one "attempt 1". Private PipeWire, `ulimit -n 1024`, 90 s:
    /// 58/58 nodes present at t=2,20,39,57,77 s, 4 restarts all "(stuck, attempt 1)".
    #[test]
    fn red_transient_complete_graph_defeats_backoff() {
        let n = expected(&cfg_nxm(8, 5)).len();
        let ev = sim(n, 600.0, &[], |_, since, _| if since < 1.0 { n } else if since < 3.5 { 0 } else { 7 });
        let stuck = ev.iter().filter(|e| e.stuck).count();
        // with the intended backoff (17, 34, 68, 120, 120 ...) 10 min holds at most ~7 restarts
        assert!(stuck <= 7, "{stuck} restarts in 10 min; intervals {:?}", ev.windows(2).map(|w| (w[1].t - w[0].t).round()).collect::<Vec<_>>());
    }

    // ---------------- green probes (regression tests) ----------------

    #[test]
    fn probe_zero_nodes_and_complete_never_stuck() {
        assert_eq!(graph_patience(0, 0), Duration::from_secs(8));
        let cfg = Config { inputs: vec![], buses: vec![], ..Config::default_config() };
        assert!(expected(&cfg).is_empty());
    }

    #[test]
    fn probe_clock_earlier_than_spawn_no_panic() {
        let t0 = Instant::now() + Duration::from_secs(100);
        let mut sup = Supervisor::default();
        sup.spawned(t0);
        let m = vec!["a".to_string()];
        assert_eq!(sup.tick(t0, true, false, &m, 5), None);
        assert_eq!(sup.tick(t0 - Duration::from_secs(50), true, false, &m, 5), None);
    }

    #[test]
    fn probe_patience_caps_and_monotone() {
        for r in [0u32, 1, 4, 5, 63, 64, u32::MAX] {
            assert!(graph_patience(10_000, r) <= Duration::from_secs(120));
        }
        let mut prev = Duration::ZERO;
        for n in 0..800 { let p = graph_patience(n, 0); assert!(p >= prev); prev = p; }
        for n in [1usize, 61, 135] {
            for r in 0..6 { assert!(graph_patience(n, r + 1) >= graph_patience(n, r)); }
        }
    }

    /// Transient fresh-spawn race (all nodes up within a few seconds) at 8x5: never restarts.
    #[test]
    fn probe_slow_spawn_8x5_no_restart() {
        let n = expected(&cfg_nxm(8, 5)).len();
        let ev = sim(n, 600.0, &[], |_, since, _| if since < 12.0 { (n as f64 * (1.0 - since / 12.0)).max(1.0) as usize } else { 0 });
        assert!(ev.is_empty(), "{} restarts", ev.len());
    }

    /// A fault that one restart clears (the #22 fd race once the limit is raised) recovers with
    /// exactly one restart and the backoff resets.
    #[test]
    fn probe_one_restart_clears_fault() {
        let n = expected(&cfg_nxm(8, 5)).len();
        let ev = sim(n, 900.0, &[], |_, since, r| if since < 1.0 { n } else if r == 0 { 3 } else { 0 });
        assert_eq!(ev.len(), 1);
        assert!(ev[0].t < graph_patience(n, 0).as_secs_f64() + 4.0);
    }
}

/// Breaker round 2 for PR #27: attacks the Supervisor (degraded mode, 30 s stability window, 5 s
/// grace vs the zero-node rule). `run2` drives the real Supervisor like run() does, with named
/// missing sets so set identity matters.
#[cfg(test)]
mod breaker27_r2 {
    use super::*;

    const TICK: f64 = 1.2;
    const AFTER_RESTART: f64 = 1.5;

    #[derive(Debug)]
    struct Ev { t: f64, why: Why }

    /// `missing(t, since_spawn, restarts_so_far) -> names`. Returns (restarts, degraded-entry times).
    fn run2(total_expected: usize, total: f64, mut missing: impl FnMut(f64, f64, usize) -> Vec<String>) -> (Vec<Ev>, Vec<f64>) {
        let base = Instant::now();
        let at = |t: f64| base + Duration::from_secs_f64(t);
        let mut sup = Supervisor::default();
        let mut started = -60.0f64;
        let (mut ev, mut deg) = (vec![], vec![]);
        let mut was = false;
        let mut t = 0.0;
        while t < total {
            let m = missing(t, t - started, ev.len());
            let why = sup.tick(at(t), true, false, &m, total_expected);
            match (sup.degraded().is_some(), was) { (true, false) => { deg.push(t); was = true } (false, true) => was = false, _ => {} }
            if let Some(why) = why {
                if started > -59.0 { ev.push(Ev { t, why }); }
                started = t;
                sup.spawned(at(t));
                t += AFTER_RESTART;
                continue;
            }
            t += TICK;
        }
        (ev, deg)
    }
    fn s(v: &[&str]) -> Vec<String> { v.iter().map(|x| x.to_string()).collect() }

    // ---------------- red ----------------

    /// R2-F1: once degraded (one node a restart cannot fix), the engine ignores every NEW,
    /// restart-fixable loss for up to an hour, e.g. the mic strip dying later. Only the original
    /// set was given up on; a different set should still get the normal policy.
    #[test]
    fn r2_degraded_ignores_new_fixable_loss() {
        let n = 59;
        let mut lost_at_restarts: Option<usize> = None;
        let (ev, deg) = run2(n, 4000.0, |t, since, r| {
            if since < 1.0 { return (0..n).map(|i| format!("n{i}")).collect(); }
            let mut m = s(&["route_x_b9"]); // permanent
            if t >= 600.0 {
                let r0 = *lost_at_restarts.get_or_insert(r);
                if r == r0 { m.push("strip_mic".into()); m.push("strip_mic.in".into()); } // one restart fixes it
            }
            m
        });
        assert!(!deg.is_empty() && deg[0] < 600.0, "precondition: degraded before t=600 ({deg:?})");
        let first = ev.iter().find(|e| e.t >= 600.0).map(|e| e.t - 600.0).unwrap_or(f64::INFINITY);
        assert!(first <= 61.0, "mic strip lost at t=600 while degraded on another node: first restart after {first:.0}s");
    }

    /// R2-F2: run() passes expected(&cfg).len() as expected_total, but `missing` excludes
    /// dependency-filtered nodes. With any dependency absent (default config: mic on
    /// easyeffects_source, EasyEffects not running) missing.len() can never equal expected_total,
    /// so the zero-node rule is dead. An alive child owning none of its nodes then goes down the
    /// Stuck path and ends DEGRADED with the whole mixer silent for an hour.
    #[test]
    fn r2_zero_node_rule_dead_when_dependency_absent() {
        let cfg = Config::default_config();
        let has = |x: &str| x.starts_with("ch_") || x.starts_with("bus_"); // server sinks only
        let missing: Vec<String> = expected(&cfg).into_iter()
            .filter(|(n, dep)| !has(n) && dep.as_ref().map(|d| has(d)).unwrap_or(true))
            .map(|(n, _)| n).collect();
        let total = expected(&cfg).len();
        assert!(missing.len() < total, "precondition: the default config has a Source input");
        let (ev, deg) = run2(missing.len(), 900.0, |_, _, _| missing.clone()); // eligible count, as run() now passes
        let first = ev.first().map(|e| (e.t, e.why));
        assert!(matches!(first, Some((t, Why::ZeroNodes)) if t <= 13.0), "child owns 0 nodes: first restart {first:?}, degraded at {deg:?}");
        assert!(deg.is_empty(), "whole graph missing, engine went degraded at {deg:?} (silent mixer, next retry in 1 h)");
    }

    // ---------------- green probes ----------------

    /// Degraded recovers by itself when the node comes back (30 s stable) and the next loss gets
    /// the short grace.
    #[test]
    fn probe_degraded_recovers_when_node_returns() {
        let n = 59;
        let base = Instant::now(); let at = |t: f64| base + Duration::from_secs_f64(t);
        let mut sup = Supervisor::default();
        let (mut t, mut started) = (0.0, 0.0);
        sup.tick(at(t), false, false, &[], n); sup.spawned(at(t));
        let m = s(&["route_x_b9"]);
        while sup.degraded().is_none() {
            t += TICK;
            if sup.tick(at(t), true, false, &m, n).is_some() { sup.spawned(at(t)); started = t; }
            assert!(t < 2000.0);
        }
        let _ = started;
        let back = t;
        while sup.degraded().is_some() { t += TICK; assert!(sup.tick(at(t), true, false, &[], n).is_none()); assert!(t - back < 32.0); }
        assert_eq!(sup.patience(n), LOSS_GRACE);
    }

    /// R2-F3: a node that flaps on its own (gone 40 s / back 40 s, restarts do not change it).
    /// Every 40 s complete stretch passes STABLE_AFTER, forgets all failures and re-arms the 5 s
    /// grace, so the engine never degrades: 45 full-graph restarts (all audio cut) per hour, forever.
    #[test]
    fn r2_flapping_node_never_degrades() {
        let n = 59;
        let (ev, deg) = run2(n, 3600.0, |t, since, _| {
            if since < 1.0 { return (0..n).map(|i| format!("n{i}")).collect(); }
            if (t as u64 / 40) % 2 == 1 { s(&["strip_mic", "strip_mic.in"]) } else { vec![] }
        });
        eprintln!("flap: {} restarts/h, degraded entries {:?}", ev.len(), deg);
        assert!(ev.len() <= 12, "{} full-graph restarts in 1 h, degraded entries {deg:?}", ev.len());
    }

    /// Complete for 29 s after earlier failed restarts, then a loss: still counted as part of the
    /// failure streak (F1 design); the patience is the backoff, not the 5 s grace.
    #[test]
    fn probe_29s_window_after_failures_uses_backoff() {
        let n = 59;
        let (ev, _) = run2(n, 400.0, |_, since, r| {
            if since < 1.0 { return (0..n).map(|i| format!("n{i}")).collect(); }
            if r < 2 { return s(&["a"]); }
            if since < 29.0 { vec![] } else if r == 2 { s(&["b"]) } else { vec![] }
        });
        assert!(ev.len() >= 3);
        let gap = ev[2].t - (ev[1].t + 29.0);
        eprintln!("29s window: loss -> restart {gap:.1}s");
        assert!(gap <= 61.0);
    }
}

/// Breaker round 3 for PR #27: rolling-hour budget, subset rule, eligible count.
#[cfg(test)]
mod breaker27_r3 {
    use super::*;

    const TICK: f64 = 1.2;
    const AFTER_RESTART: f64 = 1.5;
    const N: usize = 59;

    #[derive(Debug)]
    struct Ev { t: f64, why: Why }

    fn run3(total: f64, mut missing: impl FnMut(f64, f64, usize) -> Vec<String>) -> (Vec<Ev>, Vec<(f64, Vec<String>)>) {
        let base = Instant::now();
        let at = |t: f64| base + Duration::from_secs_f64(t);
        let mut sup = Supervisor::default();
        let mut started = -60.0f64;
        let (mut ev, mut deg) = (vec![], vec![]);
        let mut was = false;
        let mut t = 0.0;
        while t < total {
            let m = missing(t, t - started, ev.len());
            let why = sup.tick(at(t), true, false, &m, N);
            match (sup.degraded(), was) { (Some(d), false) => { deg.push((t, d.to_vec())); was = true } (None, true) => was = false, _ => {} }
            if let Some(why) = why {
                if started > -59.0 { ev.push(Ev { t, why }); }
                started = t;
                sup.spawned(at(t));
                t += AFTER_RESTART;
                continue;
            }
            t += TICK;
        }
        (ev, deg)
    }
    fn all() -> Vec<String> { (0..N).map(|i| format!("n{i}")).collect() }

    // ---------------- red ----------------

    /// R3-F1: restarts that WORKED (graph complete right after, stable for minutes) still use up
    /// the rolling-hour budget. Six separate losses, each fixed by one restart, then a seventh at
    /// t=1860 that one restart would also fix: the engine goes degraded and leaves it broken until
    /// an hour after the 6th restart (~50 min). A restart followed by a stable complete graph is
    /// proof restarts help; it should not count like a flap.
    #[test]
    fn r3_successful_restarts_exhaust_budget_and_block_next_fix() {
        let losses = [60.0, 360.0, 660.0, 960.0, 1260.0, 1560.0, 1860.0];
        let mut fixed_by: Vec<Option<usize>> = vec![None; losses.len()];
        let (ev, deg) = run3(4000.0, |t, since, r| {
            if since < 1.0 { return all(); }
            for (k, l) in losses.iter().enumerate() {
                if t >= *l && (k + 1 == losses.len() || t < losses[k + 1]) {
                    let r0 = *fixed_by[k].get_or_insert(r);
                    return if r == r0 { vec![format!("route_{k}")] } else { vec![] };
                }
            }
            vec![]
        });
        let first = ev.iter().find(|e| e.t >= 1860.0).map(|e| e.t - 1860.0).unwrap_or(f64::INFINITY);
        assert!(first <= 15.0, "7th fixable loss: first restart after {first:.0}s, degraded {deg:?}; restarts at {:?}", ev.iter().map(|e| e.t.round()).collect::<Vec<_>>());
    }

    /// R3-F2: the subset rule (R2-F1 fix) is a no-op once degraded was entered through the budget:
    /// the new node clears degraded, but restart_log still holds 6 entries, so the next stuck
    /// decision re-degrades with {old, new} without a single restart.
    #[test]
    fn r3_subset_rule_defeated_by_full_budget() {
        // 6 stuck restarts with a different (unfixable) set each time -> budget path to degraded
        let (ev, deg) = run3(3000.0, |t, since, r| {
            if since < 1.0 { return all(); }
            let mut m = vec![format!("x{}", r.min(6))];
            if t >= 1200.0 && r <= 7 { m.push("strip_mic".into()); } // new, one restart fixes it
            m
        });
        assert!(!deg.is_empty() && deg[0].0 < 1200.0, "precondition: degraded via budget before t=1200: {deg:?}");
        let first = ev.iter().find(|e| e.t >= 1200.0).map(|e| e.t - 1200.0).unwrap_or(f64::INFINITY);
        assert!(first <= 61.0, "new node lost at t=1200 while degraded on the budget: first restart after {first:.0}s; degraded entries {deg:?}");
    }

    // ---------------- green probes ----------------

    /// Exactly-1 h edge: an entry exactly 3600 s old no longer counts; 6 in the window block the 7th.
    #[test]
    fn probe_budget_window_edge() {
        let base = Instant::now(); let at = |t: f64| base + Duration::from_secs_f64(t);
        let mut sup = Supervisor::default();
        sup.tick(at(0.0), false, false, &[], N); sup.spawned(at(0.0));
        let mut restarts = vec![];
        let mut t = 0.0;
        let mut k = 0;
        while t < 3700.0 {
            t += 1.0;
            let m = vec![format!("u{k}")]; // new set every child: same_set never trips
            if sup.tick(at(t), true, false, &m, N) == Some(Why::Stuck) { restarts.push(t); sup.spawned(at(t)); k += 1; }
            if sup.degraded().is_some() { break; }
        }
        assert_eq!(restarts.len(), RESTART_CAP, "{restarts:?}");
    }

    /// ZeroNodes never consumes the budget nor degrades, and waits 10/20/40/60/60 s.
    #[test]
    fn probe_zero_nodes_independent_of_budget() {
        let (ev, deg) = run3(600.0, |_, _, _| all());
        assert!(deg.is_empty());
        assert!(ev.iter().all(|e| e.why == Why::ZeroNodes));
        let gaps: Vec<f64> = ev.windows(2).map(|w| (w[1].t - w[0].t).round()).collect();
        assert!(gaps.iter().all(|g| *g <= 62.0), "{gaps:?}");
        assert!(ev.len() >= 8);
    }

    /// R3-F3 (live-confirmed): when easyeffects_source goes away and comes back (EasyEffects
    /// restarted / crashed), the child's mic strip nodes are gone and only a graph restart brings
    /// them back. Private PipeWire, default config, source toggled every 15 s: the strips vanished
    /// on the 2nd return and stayed gone for 90+ s with no restart, because each dep-absent window
    /// filters them out (missing = 0 = "complete") and resets Settle before the 27 s patience.
    /// Here EasyEffects restarts every 8 min (absent 3 s): each one costs a full-graph restart and
    /// a budget slot, so from the 7th the engine goes degraded and the mic stays dead ~1 h.
    #[test]
    fn r3_easyeffects_restarts_exhaust_budget_mic_dead() {
        let base = Instant::now(); let at = |t: f64| base + Duration::from_secs_f64(t);
        let mut sup = Supervisor::default();
        let strips = vec!["strip_mic.in".to_string(), "strip_mic".to_string()];
        let (mut t, mut dep_prev, mut alive_strips) = (0.0f64, true, true);
        let mut started = -60.0f64;
        let mut outage_start: Option<f64> = None;
        let mut worst: f64 = 0.0;
        while t < 3600.0 {
            let dep = (t % 480.0) >= 3.0 || t < 480.0;
            if dep && !dep_prev { alive_strips = false; } // strip dies when the target comes back
            dep_prev = dep;
            let m: Vec<String> = if t - started < 1.0 { (0..N).map(|i| format!("n{i}")).collect() }
                else if dep && !alive_strips { strips.clone() } else { vec![] };
            let eligible = if dep { N } else { N - 2 };
            if dep && !alive_strips { outage_start.get_or_insert(t); } else if let Some(o) = outage_start.take() { worst = worst.max(t - o); }
            if let Some(_) = sup.tick(at(t), true, false, &m, eligible) {
                started = t; sup.spawned(at(t)); if dep { alive_strips = true; }
                t += AFTER_RESTART; continue;
            }
            t += TICK;
        }
        if let Some(o) = outage_start { worst = worst.max(t - o); }
        assert!(worst <= 60.0, "worst mic outage after an EasyEffects restart: {worst:.0}s (degraded: {:?})", sup.degraded());
    }

    /// Dependency fast toggle (period 15 s) after one restart: the strip stays dead as long as
    /// the toggling continues (each absent window resets Settle); documents the live run.
    #[test]
    fn r3_fast_dependency_toggle_never_repairs() {
        let base = Instant::now(); let at = |t: f64| base + Duration::from_secs_f64(t);
        let mut sup = Supervisor::default();
        sup.tick(at(0.0), false, false, &[], N); sup.spawned(at(0.0));
        sup.stuck_restarts = 1; // as in the live run: one earlier restart, patience 27 s
        let strips = vec!["strip_mic.in".to_string(), "strip_mic".to_string()];
        let mut t = 10.0; let mut restarted = false;
        while t < 600.0 {
            let dep = ((t / 15.0) as u64) % 2 == 0;
            let m = if dep { strips.clone() } else { vec![] };
            if sup.tick(at(t), true, false, &m, if dep { N } else { N - 2 }).is_some() { restarted = true; break; }
            t += TICK;
        }
        assert!(restarted, "mic strip dead whenever EasyEffects is up, for 10 min, never restarted");
    }
}

/// Breaker round 4 for PR #27: refund rule, eligible-count heuristic, free restarts.
#[cfg(test)]
mod breaker27_r4 {
    use super::*;

    const TICK: f64 = 1.2;
    const AFTER_RESTART: f64 = 1.5;
    const N: usize = 59;

    /// Drive the real Supervisor. `state(t, since_spawn) -> (missing, eligible)`.
    /// Returns (restart times, degraded ever).
    fn drive(total: f64, mut state: impl FnMut(f64, f64, bool) -> (Vec<String>, usize)) -> (Vec<f64>, bool) {
        let base = Instant::now();
        let at = |t: f64| base + Duration::from_secs_f64(t);
        let mut sup = Supervisor::default();
        let mut started = -60.0f64;
        let (mut ev, mut deg) = (vec![], false);
        let mut t = 0.0;
        let mut just = false;
        while t < total {
            let (m, e) = state(t, t - started, just);
            just = false;
            let why = sup.tick(at(t), true, false, &m, e);
            deg |= sup.degraded().is_some();
            if why.is_some() {
                if started > -59.0 { ev.push(t); }
                started = t; sup.spawned(at(t)); just = true;
                t += AFTER_RESTART; continue;
            }
            t += TICK;
        }
        (ev, deg)
    }
    fn all(n: usize) -> Vec<String> { (0..n).map(|i| format!("n{i}")).collect() }

    // ---------------- red ----------------

    /// R4-F1: the refund rule loops. A node that dies ~35 s after every spawn (a plugin or
    /// buffer failure that takes a while, the F1 "appears then dies" pattern with a slower fuse):
    /// each restart "worked" (complete within patience, complete for 30 s), so every slot is
    /// refunded, stuck_restarts resets and the 5 s grace applies: a full-graph restart every
    /// ~42 s forever, never degraded.
    #[test]
    fn r4_refund_loop_node_dies_after_35s() {
        let (ev, deg) = drive(3600.0, |_, since, _| {
            if since < 2.0 { (all(N), N) } else if since < 35.0 { (vec![], N) } else { (vec!["route_x_b1".into()], N) }
        });
        assert!(ev.len() <= 10, "{} full-graph restarts in 1 h (degraded {deg}); first gaps {:?}", ev.len(), ev.windows(2).take(4).map(|w| (w[1] - w[0]).round()).collect::<Vec<_>>());
    }

    /// EasyEffects crash-looping (source present `on` s / absent `off` s); the mic strip dies whenever
    /// its target returns (ee_toggle.sh shows that live) and only a graph restart revives it.
    /// Returns (restarts, seconds the mic was dead while its source was up).
    fn flap_dep(on: f64, off: f64, perm: bool) -> (usize, f64, bool) {
        let mut strips_alive = true;
        let mut dep_prev = true;
        let mut dead = 0.0;
        let mut last_t = 0.0;
        let (ev, deg) = drive(3600.0, |t, since, just| {
            let dep = (t % (on + off)) < on;
            if dep && !dep_prev { strips_alive = false; }
            dep_prev = dep;
            if just && dep { strips_alive = true; }
            if dep && !strips_alive { dead += t - last_t; }
            last_t = t;
            let e = if dep { N } else { N - 2 };
            if since < 2.0 { return (all(e), e); }
            let mut m = if perm { vec!["route_perm".to_string()] } else { vec![] };
            if dep && !strips_alive { m.push("strip_mic.in".into()); m.push("strip_mic".into()); }
            (m, e)
        });
        (ev.len(), dead, deg)
    }

    /// R4-F2a: dependency flapping (20 s on / 5 s off): every return earns an unbudgeted free
    /// restart (full-graph, all audio cut), forever, never degraded.
    #[test]
    fn r4_flapping_dependency_unbudgeted_restart_loop() {
        let (n, dead, deg) = flap_dep(20.0, 5.0, false);
        assert!(n <= 12, "{n} full-graph restarts in 1 h from a flapping dependency (degraded ever: {deg}, mic dead {dead:.0}s)");
    }

    /// R4-F2b: same flap (15 s on / 5 s off) plus one unrelated permanently missing node: every
    /// eligible change resets Settle, the backoff patience (>15 s) never elapses, so NOTHING is
    /// ever repaired: the mic is dead every time EasyEffects is up, for the whole hour.
    #[test]
    fn r4_flapping_dependency_freezes_all_repair() {
        let (n, dead, deg) = flap_dep(15.0, 5.0, true);
        // ROUND 5: `dead <= 600.0` cannot hold together with R4-F2a (<= 12 restarts/h for the same flap):
        // the source returns every 20 s and each return kills the strip, so keeping the mic alive for
        // >= 83% of the hour needs a graph restart (all audio cut) per cycle, i.e. ~170/h. The property
        // kept here is the one the finding is about: repair is not frozen (restarts happen, up to the
        // limiter) and stays bounded. That other nodes are not frozen by the flap is covered by
        // supervisor_tests::flapping_dependency_does_not_freeze_repair_of_other_nodes.
        assert!(n >= 3 && n <= HARD_CAP, "mic dead {dead:.0}s of the hour while its source was up; {n} restarts, degraded ever {deg}");
    }

    /// R4-F3: an input whose Source target is one of the child's OWN outputs (pw::Graph::sources()
    /// does not filter the child's virtual mics, so the GUI offers "Voice Chat Mix" etc.). Every
    /// spawn makes eligible grow when that output appears => free_restart. With one permanently
    /// missing node every stuck restart is free: unbudgeted loop, never degraded.
    #[test]
    fn r4_self_dependency_grants_free_restart_every_spawn() {
        let mut cfg = Config::default_config();
        let mut i = cfg.inputs[0].clone();
        i.id = "loopmix".into(); i.name = "Loop".into();
        i.kind = InputKind::Source { target: "voice_chat_mix".into() };
        cfg.inputs.push(i);
        let perm = names::route(&cfg.inputs[0].id, &cfg.buses[0].id);
        let (ev, deg) = drive(3600.0, |_, since, _| {
            let has = |x: &str| {
                if x.starts_with("ch_") || x.starts_with("bus_") || x == "easyeffects_source" { return true; }
                if x == perm { return false; }
                // child nodes come up over the first 3 s (voice_chat_mix last)
                if x == "voice_chat_mix" { since >= 3.0 } else { since >= 1.5 }
            };
            let (e, m) = eligible_and_missing(&cfg, has); (m, e)
        });
        assert!(deg || ev.len() <= 8, "{} restarts in 1 h for one permanently missing node, never degraded", ev.len());
    }

    // ---------------- green probes ----------------

    /// Refund only for a restart whose graph completed within patience: a node that dies 10 s
    /// after spawn (< STABLE_AFTER) is not refunded and ends degraded.
    #[test]
    fn probe_fast_dying_node_degrades() {
        let (ev, deg) = drive(3600.0, |_, since, _| {
            if since < 2.0 { (all(N), N) } else if since < 10.0 { (vec![], N) } else { (vec!["route_x_b1".into()], N) }
        });
        assert!(deg && ev.len() <= 6, "{} restarts, degraded {deg}", ev.len());
    }

    /// Two dependencies swapping in the same tick (net eligible unchanged): no free restart,
    /// normal budget applies.
    #[test]
    fn probe_two_dependencies_swap_is_budgeted() {
        let (ev, deg) = drive(3600.0, |t, since, _| {
            if since < 2.0 { return (all(N), N); }
            if t > 100.0 { (vec!["strip_a".into(), "strip_a.in".into()], N) } else { (vec![], N) }
        });
        assert!(deg && ev.len() <= 4, "{} restarts", ev.len());
    }
}

/// Breaker round 5 for PR #27: the single limiter, per-name clocks, property-test strength.
#[cfg(test)]
mod breaker27_r5 {
    use super::*;

    const TICK: f64 = 1.2;
    const AFTER_RESTART: f64 = 1.5;
    const N: usize = 59;

    fn all(n: usize) -> Vec<String> { (0..n).map(|i| format!("n{i}")).collect() }

    /// Drive the real Supervisor. `state(t, since_spawn, restarts) -> (alive, missing, eligible)`.
    fn drive(total: f64, mut state: impl FnMut(f64, f64, usize) -> (bool, Vec<String>, usize)) -> (Vec<(f64, Why)>, Supervisor) {
        let base = Instant::now();
        let at = |t: f64| base + Duration::from_secs_f64(t);
        let mut sup = Supervisor::default();
        let mut started = -60.0f64;
        let mut ev = vec![];
        let mut t = 0.0;
        while t < total {
            let (alive, m, e) = state(t, t - started, ev.len());
            if let Some(why) = sup.tick(at(t), alive, false, &m, e) {
                if started > -59.0 { ev.push((t, why)); }
                started = t; sup.spawned(at(t));
                t += AFTER_RESTART; continue;
            }
            t += TICK;
        }
        (ev, sup)
    }

    // ---------------- red ----------------

    /// R5-F1: the hard cap gives up (degraded) on the 11th loss, but degraded then only retries an
    /// hour after the LAST restart, not when the rolling cap frees. Ten separate losses 5 min
    /// apart, each fixed by one restart; the 11th (t=3000, also restart-fixable) waits until
    /// ~6300 s although the cap window frees at ~3600 s: ~45 min of outage beyond the cap itself.
    #[test]
    fn r5_degraded_outlives_hard_cap_window() {
        let losses: Vec<f64> = (0..11).map(|k| 1.0 + 300.0 * k as f64).collect();
        let mut fixed_by: Vec<Option<usize>> = vec![None; losses.len()];
        let (ev, sup) = drive(7000.0, |t, since, r| {
            if since < 1.5 { return (true, all(N), N); }
            for (k, l) in losses.iter().enumerate() {
                if t >= *l && (k + 1 == losses.len() || t < losses[k + 1]) {
                    let r0 = *fixed_by[k].get_or_insert(r);
                    return (true, if r == r0 { vec![format!("route_{k}")] } else { vec![] }, N);
                }
            }
            (true, vec![], N)
        });
        let first_free = ev.iter().filter(|e| e.1 == Why::Stuck).next().map(|e| e.0 + 3600.0).unwrap();
        let fix = ev.iter().find(|e| e.0 >= losses[10]).map(|e| e.0).unwrap_or(f64::INFINITY);
        assert!(fix <= first_free + 70.0, "11th loss at {:.0}: cap frees at {first_free:.0}, repaired at {fix:.0} (degraded {:?}); restarts {:?}", losses[10], sup.degraded(), ev.iter().map(|e| e.0.round()).collect::<Vec<_>>());
    }

    /// R5-F2: `max_eligible` only ever grows, so once a dependency has been seen and then goes
    /// away for good (EasyEffects stopped, a USB mic unplugged) `eligible < max_eligible` forever
    /// and no clock is ever pruned. A route that blinked out at t=200 keeps its t=200 clock; when
    /// it blinks again at t=1000 for ~2 s, the 5 s grace is skipped and the whole graph is
    /// restarted at once (all audio cut) for a node that was coming back by itself.
    #[test]
    fn r5_stale_clock_after_permanent_dependency_loss() {
        let (ev, _) = drive(1100.0, |t, since, _| {
            if since < 1.5 { return (true, all(N), N); }
            let e = if t < 100.0 { N } else { N - 2 }; // EasyEffects stops at t=100 and stays off
            let blink = (200.0..202.5).contains(&t) || (1000.0..1002.5).contains(&t);
            (true, if blink { vec!["route_spotify_a1".into()] } else { vec![] }, e)
        });
        assert!(ev.is_empty(), "a 2 s blink (< 5 s grace) restarted the whole graph at {:?}", ev);
    }

    // ---------------- probes ----------------

    /// A stronger random world than property_limiter_holds_for_random_event_sequences: adds the
    /// round-1/4 spawn-phase-locked faults (nodes that appear then die `fuse` s after every spawn),
    /// and checks LIVENESS during the chaos, not only after 65 healthy minutes: a fixable outage
    /// must end within 20 min unless the hard cap was reached in the hour before.
    #[test]
    fn probe_fuzz_phase_locked_faults_liveness() {
        struct R(u64);
        impl R { fn n(&mut self) -> u64 { self.0 ^= self.0 >> 12; self.0 ^= self.0 << 25; self.0 ^= self.0 >> 27; self.0.wrapping_mul(0x2545F4914F6CDD1D) } fn b(&mut self, k: u64) -> u64 { self.n() % k } }
        let mut worst = 0.0f64;
        let mut worst_seed = 0;
        let mut max_hour = 0;
        for seed in 1..=400u64 {
            let mut r = R(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1);
            let fuse_node = r.b(12) as usize;
            let fuse = [0.0, 3.0, 12.0, 31.0, 35.0, 62.0][r.b(6) as usize]; // 0 = no fuse
            let bad = if r.b(3) == 0 { Some(r.b(12) as usize) } else { None };
            let base = Instant::now(); let at = |t: f64| base + Duration::from_secs_f64(t);
            let mut sup = Supervisor::default();
            let mut present = [true; 12];
            let (mut t, mut spawned) = (0.0f64, -100.0f64);
            let mut stuck: Vec<f64> = vec![];
            let mut outage: [Option<f64>; 12] = [None; 12];
            let mut first = true;
            while t < 3.0 * 3600.0 {
                if r.b(40) == 0 { let i = r.b(12) as usize; present[i] = false; }
                if fuse > 0.0 && t - spawned >= fuse { present[fuse_node] = false; }
                let young = t - spawned < 2.0;
                let miss: Vec<String> = (0..12).filter(|i| young || !present[*i]).map(|i| format!("r{i}")).collect();
                for i in 0..12 {
                    let fixable = !present[i] && Some(i) != bad && !young;
                    match (fixable, outage[i]) {
                        (true, None) => outage[i] = Some(t),
                        (false, Some(s)) => {
                            let d = t - s;
                            let capped = stuck.iter().filter(|u| **u > s - 3600.0 && **u <= t).count() >= HARD_CAP;
                            if !capped && d > worst { worst = d; worst_seed = seed; }
                            outage[i] = None;
                        }
                        _ => {}
                    }
                }
                if let Some(why) = sup.tick(at(t), true, false, &miss, 12) {
                    if !first && why == Why::Stuck { stuck.push(t); }
                    first = false;
                    spawned = t; sup.spawned(at(t));
                    for i in 0..12 { present[i] = Some(i) != bad; }
                    t += AFTER_RESTART; continue;
                }
                t += TICK;
            }
            for (i, s) in stuck.iter().enumerate() { max_hour = max_hour.max(stuck[i..].iter().take_while(|u| **u < s + 3599.9).count()); }
        }
        eprintln!("fuzz: worst uncapped fixable outage {worst:.0}s (seed {worst_seed}), max stuck/h {max_hour}");
        assert!(max_hour <= HARD_CAP);
        assert!(worst <= 1200.0, "seed {worst_seed}: a fixable outage lasted {worst:.0}s without the hard cap being reached");
    }

    /// A crash every 61 s (dead child, exempt from the cap): restarted each time (60 s gap
    /// ceiling), never degraded. Restarting is the only way back to audio, so this is accepted.
    #[test]
    fn probe_crash_every_61s_keeps_restarting() {
        let (ev, sup) = drive(3600.0, |_, since, _| (since < 61.0, if since < 1.5 { all(N) } else { vec![] }, N));
        assert!(ev.iter().all(|e| e.1 == Why::Dead) && ev.len() >= 50 && sup.degraded().is_none(), "{} restarts", ev.len());
    }
}

/// Breaker round 6 for PR #27: gated-name laundering, max_eligible decay.
#[cfg(test)]
mod breaker27_r6 {
    use super::*;

    const TICK: f64 = 1.2;
    const AFTER_RESTART: f64 = 1.5;
    const N: usize = 59;
    fn all(n: usize) -> Vec<String> { (0..n).map(|i| format!("n{i}")).collect() }
    fn strips() -> Vec<String> { vec!["strip_mic.in".into(), "strip_mic".into()] }

    /// `state(t, since_spawn, just_restarted) -> (missing, eligible)`; returns (restarts, degraded ever, sup)
    fn drive(total: f64, mut state: impl FnMut(f64, f64, bool) -> (Vec<String>, usize)) -> (Vec<f64>, bool, Supervisor) {
        let base = Instant::now();
        let at = |t: f64| base + Duration::from_secs_f64(t);
        let mut sup = Supervisor::default();
        let (mut started, mut t, mut just) = (-60.0f64, 0.0, false);
        let (mut ev, mut deg) = (vec![], false);
        while t < total {
            let (m, e) = state(t, t - started, just);
            just = false;
            let why = sup.tick(at(t), true, false, &m, e);
            deg |= sup.degraded().is_some();
            if why.is_some() {
                if started > -59.0 { ev.push(t); }
                started = t; sup.spawned(at(t)); just = true;
                t += AFTER_RESTART; continue;
            }
            t += TICK;
        }
        (ev, deg, sup)
    }

    /// One permanently missing UNGATED route plus EasyEffects restarting every `period` s (absent
    /// 5 s; strips healthy, they are not the problem). Every return resets same_set and
    /// stuck_restarts, so neither give-up rule can fire.
    fn perm_with_flap(period: f64) -> (usize, bool) {
        let (ev, deg, _) = drive(3.0 * 3600.0, |t, since, _| {
            let dep = (t % period) >= 5.0 || t < period;
            let e = if dep { N } else { N - 2 };
            if since < 1.5 { return (all(e), e); }
            (vec!["route_spotify_b1".into()], e)
        });
        (ev.iter().filter(|t| **t >= 3600.0).count() / 2, deg)
    }

    // ---------------- red ----------------

    /// R6-F1: a flapping dependency launders a permanent, ungated node out of the give-up rules:
    /// `eligible > last_eligible` zeroes same_set and stuck_restarts on every return, so
    /// `same_set >= 3` never accumulates and the cap give-up (`stuck_restarts > 0`) never holds.
    /// Steady state (hours 2-3): restarts at the penalty/hard-cap pace forever, never degraded.
    /// Without the flap the same node degrades after 3 restarts and is retried once an hour.
    #[test]
    fn r6_flapping_dependency_keeps_permanent_node_out_of_degraded() {
        let (without, deg0) = perm_with_flap(1e9);
        assert!(deg0 && without <= 1, "baseline: no flap -> degraded, ~1 retry/h ({without}/h)");
        // measured: period 60 s -> 6/h, 120 s -> 6/h, >= 300 s -> 1/h (fine)
        let (with, deg) = perm_with_flap(120.0);
        assert!(deg && with <= 2, "EasyEffects restarting every 2 min: {with} full-graph restarts/h for a node no restart fixes (degraded ever: {deg}); baseline {without}/h");
    }

    // ---------------- probes ----------------

    /// Dependency away for exactly DEPENDENCY_AWAY (decay fires) then back with the strips dead:
    /// they are retried promptly (the decay does not strand them in degraded / without a clock).
    #[test]
    fn probe_dependency_away_exactly_5min_then_back() {
        for away in [295.0, 300.0, 301.2, 305.0] {
            let mut alive = true;
            let mut was = true;
            let ret = 600.0 + away;
            let (ev, _, _) = drive(1500.0, |t, since, just| {
                let dep = !(600.0..ret).contains(&t);
                if dep && !was { alive = false; }
                was = dep;
                if just && dep { alive = true; }
                let e = if dep { N } else { N - 2 };
                if since < 1.5 { return (all(e), e); }
                (if dep && !alive { strips() } else { vec![] }, e)
            });
            let fix = ev.iter().find(|t| **t >= ret).map(|t| t - ret).unwrap_or(f64::INFINITY);
            assert!(fix <= 15.0, "away {away}s: strips dead after return, restart after {fix:.0}s");
        }
    }

    /// A strip that no restart fixes, EasyEffects restarting every 3 min: laundering gives it
    /// attempts after every return, but the hard cap bounds it.
    #[test]
    fn probe_broken_gated_strip_bounded_by_cap() {
        let (ev, _, _) = drive(3.0 * 3600.0, |t, since, _| {
            let dep = (t % 180.0) >= 5.0 || t < 180.0;
            let e = if dep { N } else { N - 2 };
            if since < 1.5 { return (all(e), e); }
            (if dep { strips() } else { vec![] }, e)
        });
        let per_h = ev.iter().filter(|t| **t >= 3600.0).count() / 2;
        eprintln!("broken gated strip + 3 min flap: {per_h} restarts/h");
        assert!(per_h <= HARD_CAP);
    }
}
