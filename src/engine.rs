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
/// Failed restarts that leave the same set missing before the engine stops tearing the graph down.
pub const SAME_SET_RESTARTS: u32 = 3;
/// Failed restarts in a row (any missing set) before giving up, too.
pub const MAX_FAILED_RESTARTS: u32 = 6;
/// While degraded, retry (one restart) only this rarely; a structure change retries at once.
pub const DEGRADED_RETRY: Duration = Duration::from_secs(60 * 60);

/// Tracks progress of a freshly spawned child graph toward "every expected node exists".
#[derive(Debug, Default)]
pub struct Settle {
    best_missing: Option<usize>,
    progress_at: Option<Instant>,
}

impl Settle {
    pub fn reset(&mut self, now: Instant) {
        self.best_missing = None;
        self.progress_at = Some(now);
    }
    /// Feed the current number of missing nodes; true when the graph is stuck (the missing set
    /// has not shrunk for `patience`) and the child should be restarted. Never true while the
    /// set is still shrinking, and never true for a complete graph.
    pub fn stuck(&mut self, missing: usize, now: Instant, patience: Duration) -> bool {
        if missing == 0 {
            self.best_missing = Some(0);
            self.progress_at = Some(now);
            return false;
        }
        if self.best_missing.map_or(true, |b| missing < b) {
            self.best_missing = Some(missing);
            self.progress_at = Some(now);
            return false;
        }
        now.duration_since(self.progress_at.unwrap_or(now)) >= patience
    }
}

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
/// timeline. `run()` calls `tick` once per loop and `spawned` after (re)starting the child.
#[derive(Debug, Default)]
pub struct Supervisor {
    settle: Settle,
    spawned_at: Option<Instant>,
    seen_complete: bool,
    complete_since: Option<Instant>,
    /// consecutive stuck restarts without a stable complete graph (drives the backoff)
    pub stuck_restarts: u32,
    same_set: u32,
    failed_total: u32,
    last_set: Vec<String>,
    last_restart: Option<Instant>,
    degraded: Option<Vec<String>>,
}

impl Supervisor {
    /// Missing nodes the engine gave up restarting for (partial graph kept), if any.
    pub fn degraded(&self) -> Option<&[String]> {
        self.degraded.as_deref()
    }

    fn forget_failures(&mut self) {
        self.stuck_restarts = 0;
        self.same_set = 0;
        self.failed_total = 0;
        self.last_set.clear();
        self.degraded = None;
    }

    /// The patience that applies to the next stuck decision.
    pub fn patience(&self, expected_nodes: usize) -> Duration {
        if self.seen_complete && self.stuck_restarts == 0 {
            LOSS_GRACE
        } else {
            graph_patience(expected_nodes, self.stuck_restarts)
        }
    }

    pub fn spawned(&mut self, now: Instant) {
        self.settle.reset(now);
        self.spawned_at = Some(now);
        self.seen_complete = false;
        self.complete_since = None;
    }

    /// `missing`: expected child nodes that are absent (dependencies present); `expected_total`:
    /// how many child nodes the config expects. Returns the reason to restart now, if any.
    pub fn tick(&mut self, now: Instant, alive: bool, structure_changed: bool, missing: &[String], expected_total: usize) -> Option<Why> {
        let age = self.spawned_at.map(|t| now.saturating_duration_since(t));
        let old_enough = age.map_or(true, |a| a > MIN_CHILD_AGE);
        if missing.is_empty() {
            self.seen_complete = true;
            let since = *self.complete_since.get_or_insert(now);
            if now.saturating_duration_since(since) >= STABLE_AFTER {
                self.forget_failures();
            }
        } else {
            self.complete_since = None;
        }
        if structure_changed {
            // a different graph deserves fresh patience and a clean slate
            self.forget_failures();
            return old_enough.then_some(Why::Structure);
        }
        if !alive || self.spawned_at.is_none() {
            return old_enough.then_some(Why::Dead);
        }
        if expected_total > 0 && missing.len() == expected_total && age.map_or(false, |a| a >= ZERO_NODES_AFTER) && old_enough {
            return Some(Why::ZeroNodes);
        }
        if self.degraded.is_some() {
            if self.last_restart.map_or(false, |t| now.saturating_duration_since(t) >= DEGRADED_RETRY) {
                // one more attempt, then back to degraded if it did not help
                self.degraded = None;
                self.same_set = SAME_SET_RESTARTS - 1;
                self.failed_total = MAX_FAILED_RESTARTS - 1;
            } else {
                return None;
            }
        }
        let stuck = self.settle.stuck(missing.len(), now, self.patience(expected_total));
        if !stuck || !old_enough {
            return None;
        }
        if self.same_set >= SAME_SET_RESTARTS || self.failed_total >= MAX_FAILED_RESTARTS {
            self.degraded = Some(missing.to_vec());
            return None;
        }
        self.same_set = if missing == self.last_set.as_slice() { self.same_set + 1 } else { 1 };
        self.last_set = missing.to_vec();
        self.failed_total += 1;
        self.stuck_restarts += 1;
        self.last_restart = Some(now);
        Some(Why::Stuck)
    }
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
        eprintln!("warning: graph child fd limit stays {cur} (hard limit {max}); large graphs (>= 8 inputs x 5 buses) will fail with 'Too many open files'");
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
                 \x20       playback.props = {{ node.name = \"{node}\" node.description = \"{description}\" media.class = \"Audio/Source\" priority.session = 30 }}\n}} }}\n",
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
        let missing: Vec<_> = expected(&cfg)
            .into_iter()
            .filter(|(n, dep)| !g.has(n) && dep.as_ref().map(|d| g.has(d)).unwrap_or(true))
            .collect();
        let missing_names: Vec<String> = missing.iter().map(|m| m.0.clone()).collect();
        let why = sup.tick(Instant::now(), alive, structure != child_structure, &missing_names, expected(&cfg).len());
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
                eprintln!("restarting graph ({why:?}, attempt {}, next patience {:?}), missing: {:?}", sup.stuck_restarts, sup.patience(expected(&cfg).len()), missing_names);
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
        if levels_pending && missing.is_empty() {
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
    fn never_restarts_while_missing_set_shrinks() {
        let t0 = Instant::now();
        let p = Duration::from_secs(10);
        let mut s = Settle::default();
        s.reset(t0);
        // one node appears every 9 s for 10 minutes: always progress, never stuck
        for k in 0..60u64 {
            assert!(!s.stuck(100 - k as usize, t0 + Duration::from_secs(9 * k), p));
        }
    }

    #[test]
    fn restarts_when_stuck_and_never_when_complete() {
        let t0 = Instant::now();
        let p = Duration::from_secs(10);
        let mut s = Settle::default();
        s.reset(t0);
        assert!(!s.stuck(3, t0, p));
        assert!(!s.stuck(3, t0 + Duration::from_secs(9), p));
        assert!(s.stuck(3, t0 + Duration::from_secs(10), p));
        assert!(!s.stuck(0, t0 + Duration::from_secs(500), p));
        // the old rule gave a 28-node graph ~5.5 s; it must now outlast that
        s.reset(t0);
        assert!(!s.stuck(28, t0 + Duration::from_millis(5500), graph_patience(56, 0)));
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
    fn dead_child_does_not_advance_backoff() {
        let t0 = Instant::now();
        let mut sup = Supervisor::default();
        sup.spawned(t0);
        for k in 1..10u64 {
            let t = t0 + Duration::from_secs(10 * k);
            assert_eq!(sup.tick(t, false, false, &[], 50), Some(Why::Dead));
            sup.spawned(t);
        }
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
    fn probe_shrink_then_grow_is_stuck_from_best_point() {
        let t0 = Instant::now(); let p = Duration::from_secs(10);
        let mut s = Settle::default(); s.reset(t0);
        assert!(!s.stuck(5, t0, p));
        assert!(!s.stuck(2, t0 + Duration::from_secs(3), p));
        assert!(!s.stuck(4, t0 + Duration::from_secs(4), p));
        assert!(!s.stuck(4, t0 + Duration::from_secs(12), p));
        assert!(s.stuck(4, t0 + Duration::from_secs(13), p));
    }

    #[test]
    fn probe_oscillation_does_not_count_as_progress() {
        let t0 = Instant::now(); let p = Duration::from_secs(10);
        let mut s = Settle::default(); s.reset(t0);
        let mut hit = None;
        for k in 0..30u64 {
            if s.stuck(if k % 2 == 0 { 3 } else { 4 }, t0 + Duration::from_secs(k), p) { hit = Some(k); break; }
        }
        assert_eq!(hit, Some(10));
    }

    #[test]
    fn probe_zero_nodes_and_complete_never_stuck() {
        let t0 = Instant::now();
        let mut s = Settle::default();
        for k in 0..1000u64 { assert!(!s.stuck(0, t0 + Duration::from_secs(k), Duration::ZERO)); }
        assert_eq!(graph_patience(0, 0), Duration::from_secs(8));
        let cfg = Config { inputs: vec![], buses: vec![], ..Config::default_config() };
        assert!(expected(&cfg).is_empty());
    }

    #[test]
    fn probe_clock_earlier_than_progress_no_panic() {
        let t0 = Instant::now() + Duration::from_secs(100);
        let mut s = Settle::default(); s.reset(t0);
        assert!(!s.stuck(3, t0, Duration::from_secs(1)));
        assert!(!s.stuck(3, t0 - Duration::from_secs(50), Duration::from_secs(1)));
        // default (never reset) Settle: first observation is progress
        let mut d = Settle::default();
        assert!(!d.stuck(7, Instant::now(), Duration::ZERO));
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
