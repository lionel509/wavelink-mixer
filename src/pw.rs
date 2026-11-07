//! Thin helpers around the PipeWire command-line tools.
use std::collections::{HashMap, HashSet};
use std::process::{Command, Stdio};
use std::time::Duration;

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
fn spawn(cmd: &str, args: &[&str]) {
    let _ = Command::new(cmd).args(args).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
}

/// Forces the graph quantum (frames per cycle) through PipeWire metadata; 0 releases the force.
pub fn force_quantum(q: u32) {
    run("pw-metadata", &["-n", "settings", "0", "clock.force-quantum", &q.to_string()]);
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
