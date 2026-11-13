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
///     or a cap was reached while restarts keep failing; a loss after a stable graph is not that)
///     and keeping the partial graph. Names outside it are still handled by 1-2. It ends after 30 s of complete graph, on a structure change, when a cap-caused one
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
    /// a wanted restart is waiting for the hard cap to free up (for logging)
    deferred: bool,
}

impl Supervisor {
    /// Missing nodes the engine gave up restarting for (partial graph kept), if any.
    pub fn degraded(&self) -> Option<&[String]> {
        self.degraded.as_deref().filter(|d| !d.is_empty())
    }

    /// A restart is wanted but the hourly hard cap forbids it for now.
    pub fn deferred_at_cap(&self) -> bool {
        self.deferred
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
        self.deferred = false;

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
            // the same-set count only forgets what this dependency gated; an unrelated node keeps counting
            self.last_set.retain(|n| !self.gated.contains(n));
            if self.last_set.is_empty() {
                self.same_set = 0;
            }
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
        if hard {
            self.deferred = true;
            return None;
        }
        if self.last_restart.map_or(false, |t| now.saturating_duration_since(t) < need) {
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
            // never above the bus ceiling (a3 Sonos), volume before any unmute (#33)
            let db = b.gain_db.min(b.ceiling_for(n.raop));
            pw::set_volume_mute(n.id, db_to_vol(if db.is_nan() { crate::config::MIN_DB } else { db }), b.mute);
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
    let mut was_deferred = false;
    let degraded_file = conf_dir.join("engine.degraded");
    let _ = std::fs::remove_file(&degraded_file); // never keep a stale state from a previous engine
    let mut hw: HashMap<String, (Child, String)> = HashMap::new();
    // piano synth (FluidSynth) feeding the "piano" input channel; stdin kept open = stays alive
    let mut piano: Option<(Child, std::process::ChildStdin)> = None;
    let mut piano_sent: Option<crate::config::PianoCfg> = None;
    let mut last_quantum: Option<u32> = None;
    eprintln!("wavelink engine: config {}", Config::path().display());
    crate::remote::start(); // LAN control server on its own threads (#26)

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
        if sup.deferred_at_cap() != was_deferred {
            was_deferred = sup.deferred_at_cap();
            if was_deferred {
                eprintln!("restart deferred: hourly restart cap reached, still missing {missing_names:?}; retrying when the oldest restart leaves the window");
                let _ = std::fs::write(&degraded_file, format!("{}\n", missing_names.join("\n")));
            } else if !was_degraded {
                let _ = std::fs::remove_file(&degraded_file);
            }
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
mod tests;

#[cfg(test)]
mod supervisor_tests;

/// Breaker round 1 for PR #27 (#22). `sim` drives the real `Supervisor` (the restart policy `run()`
/// uses) with a scripted missing-node timeline.
#[cfg(test)]
mod breaker27;

/// Breaker round 2 for PR #27: attacks the Supervisor (degraded mode, 30 s stability window, 5 s
/// grace vs the zero-node rule). `run2` drives the real Supervisor like run() does, with named
/// missing sets so set identity matters.
#[cfg(test)]
mod breaker27_r2;

/// Breaker round 3 for PR #27: rolling-hour budget, subset rule, eligible count.
#[cfg(test)]
mod breaker27_r3;

/// Breaker round 4 for PR #27: refund rule, eligible-count heuristic, free restarts.
#[cfg(test)]
mod breaker27_r4;

/// Breaker round 5 for PR #27: the single limiter, per-name clocks, property-test strength.
#[cfg(test)]
mod breaker27_r5;

/// Breaker round 6 for PR #27: gated-name laundering, max_eligible decay.
#[cfg(test)]
mod breaker27_r6;

/// Breaker round 7 for PR #27: R6-F1 sweep over flap periods/phases, the cap-path variant, and
/// the deferral flag that drives engine.degraded.
#[cfg(test)]
mod breaker27_r7;
