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
    let mut child_started = Instant::now() - Duration::from_secs(60);
    let mut levels_pending = false;
    let mut missing_ticks = 0u32;
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
        if last_quantum != Some(cfg.quantum) {
            pw::force_quantum(cfg.quantum);
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
        missing_ticks = if missing.is_empty() { 0 } else { missing_ticks + 1 };
        let need_restart = !alive || structure != child_structure || missing_ticks >= 4;
        if need_restart && child_started.elapsed() > Duration::from_secs(5) {
            if let Some(mut c) = child.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
            if missing_ticks >= 4 {
                eprintln!("restarting graph, missing: {:?}", missing.iter().map(|m| &m.0).collect::<Vec<_>>());
            }
            let _ = std::fs::write(&conf_file, child_conf(&cfg));
            child = Command::new("pipewire").arg("-c").arg(&conf_file).stdout(Stdio::null()).spawn().ok();
            child_structure = structure;
            child_started = Instant::now();
            levels_pending = true;
            missing_ticks = 0;
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
