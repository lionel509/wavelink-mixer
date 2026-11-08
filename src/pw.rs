//! Thin helpers around the PipeWire command-line tools.
use std::collections::{HashMap, HashSet};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, OnceLock};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default)]
pub struct Node {
    pub id: u64,
    pub serial: u64,
    pub name: String,
    pub class: String,
    pub desc: String,
    pub app: String,
    pub media: String,
    pub vol: f64,
    pub mute: bool,
    pub owned: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Graph {
    pub nodes: Vec<Node>,
    by_name: HashMap<String, usize>,
    /// (output node id, input node id)
    pub links: HashSet<(u64, u64)>,
}

impl Graph {
    pub fn read() -> Option<Graph> {
        // time-limited: a wedged PipeWire must never hang the caller
        let out = output_timeout("pw-dump", &[], Duration::from_secs(3))?;
        let data: serde_json::Value = serde_json::from_slice(&out).ok()?;
        let mut g = Graph::default();
        for o in data.as_array()? {
            match o["type"].as_str() {
                Some("PipeWire:Interface:Node") => {
                    let p = &o["info"]["props"];
                    let s = |k: &str| p[k].as_str().unwrap_or("").to_string();
                    let (mut vol, mut mute) = (1.0, false);
                    if let Some(props) = o["info"]["params"]["Props"].as_array() {
                        for pr in props {
                            if let Some(cv) = pr["channelVolumes"].as_array() {
                                let v: Vec<f64> = cv.iter().filter_map(|x| x.as_f64()).collect();
                                if !v.is_empty() {
                                    vol = (v.iter().sum::<f64>() / v.len() as f64).cbrt();
                                }
                                mute = pr["mute"].as_bool().unwrap_or(false);
                                break;
                            }
                        }
                    }
                    let n = Node {
                        id: o["id"].as_u64().unwrap_or(0),
                        serial: p["object.serial"].as_u64().unwrap_or(0),
                        name: s("node.name"),
                        class: s("media.class"),
                        desc: s("node.description"),
                        app: s("application.name"),
                        media: s("media.name"),
                        vol,
                        mute,
                        owned: p["wavelink.owned"].as_bool() == Some(true) || s("wavelink.owned") == "true",
                    };
                    if !n.name.is_empty() {
                        g.by_name.insert(n.name.clone(), g.nodes.len());
                    }
                    g.nodes.push(n);
                }
                Some("PipeWire:Interface:Link") => {
                    let i = &o["info"];
                    if let (Some(a), Some(b)) = (i["output-node-id"].as_u64(), i["input-node-id"].as_u64()) {
                        g.links.insert((a, b));
                    }
                }
                _ => {}
            }
        }
        Some(g)
    }

    pub fn node(&self, name: &str) -> Option<&Node> {
        self.by_name.get(name).map(|&i| &self.nodes[i])
    }
    pub fn has(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }
    pub fn by_id(&self, id: u64) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }
    pub fn linked(&self, out: &str, inp: &str) -> bool {
        match (self.node(out), self.node(inp)) {
            (Some(a), Some(b)) => self.links.contains(&(a.id, b.id)),
            _ => false,
        }
    }
    /// Sinks a stream is currently linked to.
    pub fn targets_of(&self, id: u64) -> Vec<&Node> {
        self.links.iter().filter(|(a, _)| *a == id).filter_map(|(_, b)| self.by_id(*b)).collect()
    }
    pub fn hardware_sinks(&self) -> Vec<&Node> {
        self.nodes.iter().filter(|n| n.class == "Audio/Sink" && (n.name.starts_with("alsa_") || n.name.starts_with("bluez_"))).collect()
    }
    pub fn sources(&self) -> Vec<&Node> {
        self.nodes.iter().filter(|n| n.class.starts_with("Audio/Source") && !n.owned && !n.name.ends_with(".monitor")).collect()
    }
}

/// Run a command and collect stdout, killing it if it takes longer than `limit`.
pub fn output_timeout(cmd: &str, args: &[&str], limit: Duration) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut child = Command::new(cmd).args(args).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let mut out = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = vec![];
        let _ = out.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    match rx.recv_timeout(limit) {
        Ok(buf) => {
            let ok = child.wait().map(|s| s.success()).unwrap_or(false);
            ok.then_some(buf)
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            None
        }
    }
}

fn run(cmd: &str, args: &[&str]) {
    let _ = output_timeout(cmd, args, Duration::from_secs(3));
}
/// Fire-and-forget children the reaper still holds. A reaped child is never a zombie, but a child
/// that exited a moment ago is, until the reaper's next pass; the leak watch subtracts this.
static PENDING: AtomicU64 = AtomicU64::new(0);
pub fn pending() -> u64 {
    PENDING.load(Ordering::SeqCst)
}
/// Fire-and-forget command whose exit is always reaped (no zombies); hung ones are killed after 3 s.
pub fn spawn(cmd: &str, args: &[&str]) {
    let Ok(c) = Command::new(cmd).args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() else { return };
    PENDING.fetch_add(1, Ordering::SeqCst);
    if reaper().send(c).is_err() {
        PENDING.fetch_sub(1, Ordering::SeqCst);
    }
}
fn reaper() -> &'static mpsc::Sender<std::process::Child> {
    static R: OnceLock<mpsc::Sender<std::process::Child>> = OnceLock::new();
    R.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<std::process::Child>();
        let _ = std::thread::Builder::new().name("reaper".into()).spawn(move || {
            let mut live: Vec<(std::process::Child, Instant)> = vec![];
            loop {
                let got = if live.is_empty() { rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected) } else { rx.recv_timeout(Duration::from_millis(100)) };
                match got {
                    Ok(c) => live.push((c, Instant::now())),
                    Err(mpsc::RecvTimeoutError::Disconnected) if live.is_empty() => return,
                    Err(_) => {}
                }
                live.retain_mut(|(c, t0)| {
                    let keep = match c.try_wait() {
                        Ok(None) if t0.elapsed() < Duration::from_secs(3) => true,
                        Ok(None) => { let _ = c.kill(); let _ = c.wait(); false }
                        _ => false,
                    };
                    if !keep { PENDING.fetch_sub(1, Ordering::SeqCst); }
                    keep
                });
            }
        });
        tx
    })
}
/// The child gets SIGKILL when this process dies, however it dies (crash, SIGKILL).
/// Linux only (PR_SET_PDEATHSIG); elsewhere it is a passthrough. Linux fires this when the
/// *spawning thread* exits, so only call it from long-lived threads.
pub fn die_with_parent(cmd: &mut Command) -> &mut Command {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        extern "C" { fn prctl(option: std::ffi::c_int, ...) -> std::ffi::c_int; }
        const PR_SET_PDEATHSIG: std::ffi::c_int = 1;
        const SIGKILL: std::ffi::c_ulong = 9;
        let parent = std::process::id();
        unsafe {
            cmd.pre_exec(move || {
                if prctl(PR_SET_PDEATHSIG, SIGKILL) != 0 { return Err(std::io::Error::last_os_error()); }
                // parent died between fork and prctl: exit now (no allocation in the forked child)
                if std::os::unix::process::parent_id() != parent { return Err(std::io::Error::from_raw_os_error(3)); }
                Ok(())
            });
        }
    }
    cmd
}

/// Forces the graph quantum (frames per cycle) through PipeWire metadata; 0 releases the force.
/// Returns true once the value reads back, so the caller can retry a failed write.
pub fn force_quantum(q: u32) -> bool {
    run("pw-metadata", &["-n", "settings", "0", "clock.force-quantum", &q.to_string()]);
    let meta = output_timeout("pw-metadata", &["-n", "settings"], Duration::from_secs(3))
        .map(|b| String::from_utf8_lossy(&b).to_string())
        .unwrap_or_default();
    quantum_confirmed(q, &meta)
}

/// Whether a `pw-metadata` dump confirms that `force_quantum(q)` took effect.
pub fn quantum_confirmed(q: u32, meta: &str) -> bool {
    let got = meta_value(meta, "clock.force-quantum");
    if q == 0 {
        // releasing: the key reads 0, or is removed while the dump is still a settings dump
        got == Some(0) || (got.is_none() && meta_value(meta, "clock.quantum").is_some())
    } else {
        got == Some(q as u64)
    }
}

pub fn set_volume(id: u64, v: f64) {
    spawn("wpctl", &["set-volume", &id.to_string(), &format!("{v:.4}")]);
}
pub fn set_mute(id: u64, m: bool) {
    spawn("wpctl", &["set-mute", &id.to_string(), if m { "1" } else { "0" }]);
}
pub fn set_eq(id: u64, eq: &[f64]) {
    let params: String = eq.iter().enumerate().map(|(k, g)| format!("\"eq{k}:Gain\" {g:.2} ")).collect();
    spawn("pw-cli", &["set-param", &id.to_string(), "Props", &format!("{{ params = [ {params}] }}")]);
}
pub fn set_rack(id: u64, rack: &crate::config::Rack) {
    let params: String = rack.controls().iter().map(|(k, v)| format!("\"{k}\" {v:.4} ")).collect();
    spawn("pw-cli", &["set-param", &id.to_string(), "Props", &format!("{{ params = [ {params}] }}")]);
}
/// Move a playback stream to another sink (WirePlumber remembers it per app).
pub fn move_stream(serial: u64, sink: &str) {
    spawn("pactl", &["move-sink-input", &serial.to_string(), sink]);
}
/// Link two stereo nodes port by port (FL->FL, FR->FR).
pub fn link(out: &str, inp: &str) {
    for ch in ["FL", "FR"] {
        run("pw-link", &[&format!("{out}:output_{ch}"), &format!("{inp}:input_{ch}")]);
    }
}
/// Which side of a stereo pair the listener hears (L/R solo, monitor only). Left = the left signal in both ears.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Side { #[default] Stereo, Left, Right }
/// (keep, drop) port pairs `(output_X, input_Y)` for `side`; together they are the four FL/FR pairs.
pub fn side_ports(side: Side) -> (Vec<(&'static str, &'static str)>, Vec<(&'static str, &'static str)>) {
    let want: [(&str, &str); 2] = match side {
        Side::Stereo => [("FL", "FL"), ("FR", "FR")],
        Side::Left => [("FL", "FL"), ("FL", "FR")],
        Side::Right => [("FR", "FL"), ("FR", "FR")],
    };
    let all = [("FL", "FL"), ("FL", "FR"), ("FR", "FL"), ("FR", "FR")];
    (want.to_vec(), all.into_iter().filter(|p| !want.contains(p)).collect())
}
/// Rewire `out -> inp` for `side`: drop the pairs not wanted, then add the wanted ones (an existing link errors harmlessly). Links only: no node, no latency.
pub fn link_side(out: &str, inp: &str, side: Side) {
    let (keep, drop) = side_ports(side);
    for (o, i) in drop { run("pw-link", &["-d", &format!("{out}:output_{o}"), &format!("{inp}:input_{i}")]); }
    for (o, i) in keep { run("pw-link", &[&format!("{out}:output_{o}"), &format!("{inp}:input_{i}")]); }
}
pub fn destroy(id: u64) {
    run("pw-cli", &["destroy", &id.to_string()]);
}
pub fn create_null_sink(name: &str, desc: &str, priority: u32) {
    let props = format!(
        "{{ factory.name=support.null-audio-sink node.name=\"{name}\" node.description=\"{desc}\" \
         media.class=Audio/Sink audio.position=[FL FR] monitor.channel-volumes=true monitor.passthrough=true \
         object.linger=true priority.session={priority} wavelink.owned=true }}"
    );
    run("pw-cli", &["create-node", "adapter", &props]);
}

/// Value of a `pw-metadata -n settings` key, e.g. `clock.force-quantum`.
pub fn meta_value(meta: &str, key: &str) -> Option<u64> {
    meta.lines()
        .find(|l| l.contains(&format!("'{key}'")))?
        .split("value:'")
        .nth(1)?
        .split('\'')
        .next()?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_value_reads_keys() {
        let meta = "update: id:0 key:'clock.quantum-limit' value:'8192' type:''\nupdate: id:0 key:'clock.quantum' value:'1024' type:''\nupdate: id:0 key:'clock.min-quantum' value:'32' type:''\nupdate: id:0 key:'clock.force-quantum' value:'256' type:''\n";
        assert_eq!(meta_value(meta, "clock.force-quantum"), Some(256));
        assert_eq!(meta_value(meta, "clock.quantum"), Some(1024));
        assert_eq!(meta_value(meta, "clock.rate"), None);
    }

    #[test]
    fn quantum_confirmed_rules() {
        let f0 = "update: id:0 key:'clock.quantum' value:'1024' type:''\nupdate: id:0 key:'clock.force-quantum' value:'0' type:''\n";
        let f256 = "update: id:0 key:'clock.quantum' value:'1024' type:''\nupdate: id:0 key:'clock.force-quantum' value:'256' type:''\n";
        let nokey = "update: id:0 key:'clock.quantum' value:'1024' type:''\n";
        let cases = [
            (0, "", false),
            (0, f0, true),
            (0, nokey, true),
            (0, f256, false),
            (256, f256, true),
            (256, "", false),
            (512, f256, false),
        ];
        for (q, meta, want) in cases {
            assert_eq!(quantum_confirmed(q, meta), want, "q={q} meta={meta:?}");
        }
    }

    /// (alive, zombie) children of this test process whose command basename is in `comms`.
    /// `ps -axo` prints the same columns on Linux and macOS; a macOS zombie has no name, so every
    /// zombie child counts (inside this test binary any zombie child is a reaper bug).
    fn kids(comms: &[&str]) -> (usize, usize) {
        let me = std::process::id().to_string();
        let out = Command::new("ps").args(["-axo", "pid=,ppid=,stat=,comm="]).output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
        let (mut alive, mut zombie) = (0, 0);
        for l in out.lines() {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() < 4 || f[1] != me { continue; }
            let comm = f[3].trim_matches(|c| c == '(' || c == ')').rsplit('/').next().unwrap_or("");
            if f[2].starts_with('Z') { if comm == "<defunct>" || comms.contains(&comm) { zombie += 1; } }
            else if comms.contains(&comm) { alive += 1; }
        }
        (alive, zombie)
    }

    fn lcg(x: &mut u64) -> u64 {
        *x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *x >> 33
    }

    #[test]
    fn spawn_reaps_every_child() {
        let _serial = child_lock();
        for _ in 0..50 { spawn("true", &[]); }
        let t = std::time::Instant::now();
        loop {
            let (_, z) = kids(&["true"]);
            if z == 0 { break; }
            assert!(t.elapsed() < Duration::from_secs(2), "{z} zombie children left");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn spawn_reaps_mixed_children() {
        let _serial = child_lock();
        // the real caller shapes: fails fast, finishes slowly, hangs (capped at 5 hangs)
        let mut x: u64 = 7;
        let mut hangs = 0;
        for _ in 0..40 {
            match lcg(&mut x) % 3 {
                0 => spawn("false", &[]),
                1 => spawn("sh", &["-c", "sleep 0.2"]),
                _ if hangs < 5 => { hangs += 1; spawn("sleep", &["30"]); }
                _ => spawn("false", &[]),
            }
        }
        let t = std::time::Instant::now();
        loop {
            if kids(&["false", "sh", "sleep"]) == (0, 0) { break; }
            assert!(t.elapsed() < Duration::from_secs(5), "left {:?}", kids(&["false", "sh", "sleep"]));
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Tests that count this process's children run one at a time (they share the same child set).
    fn child_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }
    /// Zombie children of this test process as `ps` lines (macOS and Linux).
    fn my_zombies() -> Vec<String> {
        let me = std::process::id().to_string();
        let out = Command::new("ps").args(["-axo", "pid=,ppid=,stat=,comm="]).output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
        out.lines()
            .filter(|l| { let f: Vec<&str> = l.split_whitespace().collect(); f.len() >= 3 && f[1] == me && f[2].starts_with('Z') })
            .map(|l| l.trim().to_string())
            .collect()
    }

    #[test]
    fn steady_spawns_do_not_trip_zombie_rule() {
        let _serial = child_lock();
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        // phase 1: a burst of spawns drains out of the reaper's pending count
        for _ in 0..20 { spawn("true", &[]); }
        let t = Instant::now();
        loop {
            if pending() == 0 { break; }
            assert!(t.elapsed() < Duration::from_secs(10), "reaper still holds {} spawned children", pending());
            std::thread::sleep(Duration::from_millis(50));
        }
        // phase 2: ~30 spawns/s while the leak watch samples zombies every 10 s
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        let feeder = std::thread::spawn(move || {
            while !s2.load(Ordering::Relaxed) {
                spawn("true", &[]);
                std::thread::sleep(Duration::from_millis(33));
            }
        });
        let mut w = crate::leak::Watch::new("window");
        let base = crate::leak::ProcStats::default();
        w.update(&base, None, 60.0);
        let mut warns: Vec<String> = vec![];
        let mut seen = vec![];
        for k in 1..=2u64 {
            std::thread::sleep(Duration::from_secs(10));
            let p0 = pending();
            let z = my_zombies().len() as u64;
            let p = p0.max(pending());
            seen.push((z, p));
            let s = crate::leak::ProcStats { zombies: z.saturating_sub(p), ..base };
            warns.extend(w.update(&s, None, 60.0 + 10.0 * k as f64));
        }
        stop.store(true, Ordering::Relaxed);
        let _ = feeder.join();
        assert!(warns.is_empty(), "zombie warning under steady spawns: {warns:?} (zombies, pending at scans {seen:?})");
    }

    #[test]
    fn midi_cc_send_failure_reaps_child() {
        let _serial = child_lock();
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("wavelink-midi-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stdbuf = dir.join("stdbuf");
        std::fs::write(
            &stdbuf,
            "#!/bin/sh\necho 'Waiting for data. Press Ctrl+C to end.'\nsleep 0.3\necho '  36:0   Control change          0, controller 41, value 79'\nexec sleep 30\n",
        ).unwrap();
        std::fs::set_permissions(&stdbuf, std::fs::Permissions::from_mode(0o755)).unwrap();
        let old = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{old}", dir.display()));
        std::env::remove_var("MIXER_NO_AUDIO");
        let (tx, rx) = std::sync::mpsc::channel();
        crate::midi::spawn(tx);
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(crate::midi::Midi::Connected(true)) => {}
            _ => panic!("no Connected(true) from fake aseqdump"),
        }
        drop(rx); // the next CC send fails, so the reader gives up
        std::thread::sleep(Duration::from_secs(2));
        let t = Instant::now();
        let left = loop {
            let z = my_zombies();
            if z.is_empty() || t.elapsed() >= Duration::from_secs(3) { break z; }
            std::thread::sleep(Duration::from_millis(50));
        };
        let _ = std::fs::remove_dir_all(&dir);
        assert!(left.is_empty(), "zombie children after midi reader gave up: {left:?}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn tap_dies_when_parent_is_sigkilled() {
        use std::io::{BufRead, BufReader};
        let mut mid = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "pw::tests::deathsig_helper", "--ignored", "--nocapture", "--test-threads=1"])
            .env("WAVELINK_DEATHSIG_HELPER", "1")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let out = mid.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for l in BufReader::new(out).lines().map_while(Result::ok) { let _ = tx.send(l); }
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let pid: u32 = loop {
            let rem = deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(rem) {
                Ok(l) => { if let Some(p) = l.strip_prefix("PID ") { break p.trim().parse().unwrap(); } }
                Err(_) => panic!("helper printed no PID line"),
            }
        };
        assert!(std::path::Path::new(&format!("/proc/{pid}")).exists(), "helper child {pid} not running");
        mid.kill().unwrap();
        mid.wait().unwrap();
        let t = std::time::Instant::now();
        loop {
            let gone = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                Err(_) => true,
                Ok(s) => s.rsplit(')').next().and_then(|r| r.trim().chars().next()) == Some('Z'),
            };
            if gone { break; }
            assert!(t.elapsed() < Duration::from_secs(3), "tap {pid} survived its parent's SIGKILL");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Re-exec target for `tap_dies_when_parent_is_sigkilled`. Does nothing unless the env var is set.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore]
    fn deathsig_helper() {
        if std::env::var_os("WAVELINK_DEATHSIG_HELPER").is_none() { return; }
        use std::io::Write;
        let child = die_with_parent(&mut Command::new("sleep")).arg("30").stdout(Stdio::null()).spawn().unwrap();
        println!("PID {}", child.id());
        std::io::stdout().flush().unwrap();
        std::thread::sleep(Duration::from_secs(30));
    }

    #[test]
    fn pw_record_spawned_only_by_meter() {
        assert_eq!(include_str!("dash.rs").matches("\"pw-record\"").count(), 0, "dash.rs must not spawn pw-record");
        assert_eq!(include_str!("ui.rs").matches("Command::new(\"pw-record\")").count(), 1, "only the meter tap spawns pw-record");
        assert!(include_str!("ui.rs").contains("die_with_parent(&mut cmd)"));
        assert!(include_str!("midi.rs").contains("die_with_parent(&mut cmd)"));
        assert!(include_str!("ui.rs").contains("m.area.set_halign(gtk::Align::Center)"));
        assert!(include_str!("ui.rs").contains("format!(\"{name}.{}\", std::process::id())"));
        assert_eq!(include_str!("ui.rs").matches("meter(&").count(), 3);
    }

    #[test]
    fn side_ports_partition_the_four_pairs() {
        let all = [("FL", "FL"), ("FL", "FR"), ("FR", "FL"), ("FR", "FR")];
        for side in [Side::Stereo, Side::Left, Side::Right] {
            let (keep, drop) = side_ports(side);
            assert_eq!(keep.len(), 2, "{side:?}");
            assert_eq!(drop.len(), 2, "{side:?}");
            assert!(keep.iter().all(|p| !drop.contains(p)), "{side:?} keeps and drops the same pair");
            let mut u: Vec<_> = keep.iter().chain(drop.iter()).copied().collect();
            u.sort();
            let mut want = all.to_vec();
            want.sort();
            assert_eq!(u, want, "{side:?}");
        }
    }

    #[test]
    fn side_left_and_right_feed_both_ears_from_one_side() {
        let (keep, _) = side_ports(Side::Left);
        assert!(keep.iter().all(|p| p.0 == "FL"), "{keep:?}");
        let mut ins: Vec<_> = keep.iter().map(|p| p.1).collect();
        ins.sort();
        assert_eq!(ins, vec!["FL", "FR"]);
        let (keep, _) = side_ports(Side::Right);
        assert!(keep.iter().all(|p| p.0 == "FR"), "{keep:?}");
        let mut ins: Vec<_> = keep.iter().map(|p| p.1).collect();
        ins.sort();
        assert_eq!(ins, vec!["FL", "FR"]);
        assert_eq!(side_ports(Side::Stereo).0, vec![("FL", "FL"), ("FR", "FR")]);
    }
}
