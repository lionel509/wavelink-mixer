//! The seam between the mixer window and wherever its state and PipeWire live (#44).
//! `LocalMixer` is what bazzite runs; `RemoteMixer` drives a hub over the #36 protocol.
#![allow(dead_code)]
use crate::config::{Config, Rack};
use crate::link::{self, Conn};
use crate::pw::{self, Graph, GraphWire, Side};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;
use wavelink_proto::{ClientMsg, Meters, PwCmd, ServerMsg};

pub enum Event {
    Graph(Graph),
    Config,
    Meters(Meters),
    Link(link::Event),
}

/// Which window-local engines may run. I3: every spawn site in ui.rs consults this.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Locals {
    pub taps: bool,
    pub sysmon: bool,
    pub midi: bool,
    pub norm: bool,
    pub leak: bool,
    pub restart: bool,
}

impl Locals {
    pub fn for_mixer(local: bool) -> Locals {
        Locals { taps: local, sysmon: local, midi: local, norm: local, leak: local, restart: local }
    }
}

type Prefer<'a> = &'a dyn Fn(&Config, &Config, &mut Config);

pub trait Mixer: 'static {
    fn is_local(&self) -> bool;
    fn locals(&self) -> Locals {
        Locals::for_mixer(self.is_local())
    }
    /// Window title suffix: "" locally, " - <server>" remote.
    fn title_suffix(&self) -> String;
    /// Where this window's config is on disk (local: Config::path(); remote: the mirror).
    fn cfg_path(&self) -> PathBuf;
    fn load_cfg(&self) -> Config;
    fn save_cfg(&self, mine: &mut Config, base: &mut Config, prefer: Prefer) -> Result<bool, String>;
    fn pull_cfg(&self, mine: &mut Config, base: &mut Config, prefer: Prefer) -> Result<Option<Config>, String>;
    fn graph(&self) -> Option<Graph>;
    /// Everything that arrived since the last call (graph polls, config pushes, meters, link events).
    fn events(&self) -> Vec<Event>;
    /// Host commands and anything else bound for the engine's server.
    fn send(&self, m: ClientMsg);
    /// True once the window has what it needs to build.
    fn ready(&self) -> bool;
    fn set_volume(&self, id: u64, v: f64);
    fn set_volume_mute(&self, id: u64, v: f64, mute: bool);
    fn set_mute(&self, id: u64, m: bool);
    fn set_eq(&self, id: u64, eq: &[f64]);
    fn set_rack(&self, id: u64, rack: &Rack);
    fn move_stream(&self, serial: u64, sink: &str);
    fn link(&self, out: &str, inp: &str);
    fn link_side(&self, out: &str, inp: &str, side: Side);
}

/// This machine's own PipeWire and config.json (bazzite).
pub struct LocalMixer {
    link: Option<Conn>,
    graph_rx: Receiver<Graph>,
}

impl LocalMixer {
    pub fn new(link: Option<Conn>) -> Rc<Self> {
        let (tx, graph_rx) = mpsc::channel::<Graph>();
        std::thread::spawn(move || loop {
            if let Some(g) = Graph::read() {
                if tx.send(g).is_err() {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        });
        Rc::new(LocalMixer { link, graph_rx })
    }
}

impl Mixer for LocalMixer {
    fn is_local(&self) -> bool {
        true
    }
    fn title_suffix(&self) -> String {
        String::new()
    }
    fn cfg_path(&self) -> PathBuf {
        Config::path()
    }
    fn load_cfg(&self) -> Config {
        Config::load()
    }
    fn save_cfg(&self, mine: &mut Config, base: &mut Config, prefer: Prefer) -> Result<bool, String> {
        mine.save_merged_with(base, &Config::path(), prefer)
    }
    fn pull_cfg(&self, mine: &mut Config, base: &mut Config, prefer: Prefer) -> Result<Option<Config>, String> {
        mine.pull_merged_with(base, &Config::path(), prefer)
    }
    fn graph(&self) -> Option<Graph> {
        Graph::read()
    }
    fn events(&self) -> Vec<Event> {
        let mut out: Vec<Event> = self.graph_rx.try_iter().map(Event::Graph).collect();
        if let Some(c) = &self.link {
            out.extend(c.rx.try_iter().map(Event::Link));
        }
        out
    }
    fn send(&self, m: ClientMsg) {
        if let Some(c) = &self.link {
            c.send(m);
        }
    }
    fn ready(&self) -> bool {
        true
    }
    fn set_volume(&self, id: u64, v: f64) {
        pw::set_volume(id, v)
    }
    fn set_volume_mute(&self, id: u64, v: f64, mute: bool) {
        pw::set_volume_mute(id, v, mute)
    }
    fn set_mute(&self, id: u64, m: bool) {
        pw::set_mute(id, m)
    }
    fn set_eq(&self, id: u64, eq: &[f64]) {
        pw::set_eq(id, eq)
    }
    fn set_rack(&self, id: u64, rack: &Rack) {
        pw::set_rack(id, rack)
    }
    fn move_stream(&self, serial: u64, sink: &str) {
        pw::move_stream(serial, sink)
    }
    fn link(&self, out: &str, inp: &str) {
        pw::link(out, inp)
    }
    fn link_side(&self, out: &str, inp: &str, side: Side) {
        pw::link_side(out, inp, side)
    }
}

enum Src {
    Conn(Conn),
    #[cfg(test)]
    Chan(Receiver<link::Event>, Sender<ClientMsg>),
}

/// A hub driven over the wire: config and graph arrive as snapshots, writes go back as messages.
pub struct RemoteMixer {
    src: Src,
    config: RefCell<Option<Config>>,
    graph: RefCell<Option<Graph>>,
    server: RefCell<String>,
    fresh: RefCell<bool>,
    next: RefCell<u64>,
    mirror: RefCell<Option<PathBuf>>,
}

impl RemoteMixer {
    pub fn new(opts: crate::console::Opts) -> Rc<Self> {
        let host = crate::console::host_source(&opts);
        let conn = link::connect(opts.addr.clone(), opts.token.clone(), true, host, true, || {});
        Rc::new(Self::with_src(Src::Conn(conn)))
    }

    fn with_src(src: Src) -> Self {
        RemoteMixer {
            src,
            config: RefCell::new(None),
            graph: RefCell::new(None),
            server: RefCell::new(String::new()),
            fresh: RefCell::new(false),
            next: RefCell::new(1),
            mirror: RefCell::new(None),
        }
    }

    #[cfg(test)]
    pub fn for_test() -> (Rc<Self>, TestFeed, Receiver<ClientMsg>) {
        let (etx, erx) = mpsc::channel();
        let (otx, orx) = mpsc::channel();
        (Rc::new(Self::with_src(Src::Chan(erx, otx))), TestFeed(etx), orx)
    }

    fn id(&self) -> u64 {
        let mut n = self.next.borrow_mut();
        *n += 1;
        *n
    }

    fn pw(&self, cmd: PwCmd) {
        let id = self.id();
        self.send(ClientMsg::Pw { id, cmd });
    }

    fn mirror_path(&self) -> PathBuf {
        let server = self.server.borrow();
        let name = crate::console::sanitize_host_id(&server);
        let name = if name.is_empty() { "hub".to_string() } else { name };
        let base = Config::path();
        base.parent().map(|p| p.to_path_buf()).unwrap_or_default().join("remote").join(name).join("config.json")
    }
}

#[cfg(test)]
pub struct TestFeed(Sender<link::Event>);
#[cfg(test)]
impl TestFeed {
    pub fn send(&self, m: ServerMsg) -> Result<(), String> {
        self.0.send(link::Event::Msg(m)).map_err(|e| e.to_string())
    }
}

impl Mixer for RemoteMixer {
    fn is_local(&self) -> bool {
        false
    }
    fn title_suffix(&self) -> String {
        format!(" \u{2014} {}", self.server.borrow())
    }
    fn cfg_path(&self) -> PathBuf {
        self.mirror.borrow().clone().unwrap_or_else(|| self.mirror_path())
    }
    fn load_cfg(&self) -> Config {
        self.config.borrow().clone().unwrap_or_else(Config::default_config)
    }
    fn save_cfg(&self, mine: &mut Config, base: &mut Config, _prefer: Prefer) -> Result<bool, String> {
        let id = self.id();
        let (b, m) = (serde_json::to_value(&*base).map_err(|e| e.to_string())?, serde_json::to_value(&*mine).map_err(|e| e.to_string())?);
        self.send(ClientMsg::SaveConfig { id, base: b, mine: m });
        *base = mine.clone();
        Ok(false)
    }
    fn pull_cfg(&self, mine: &mut Config, base: &mut Config, prefer: Prefer) -> Result<Option<Config>, String> {
        if !self.fresh.replace(false) {
            return Ok(None);
        }
        let Some(theirs) = self.config.borrow().clone() else { return Ok(None) };
        mine.pull_merged_from(base, theirs, prefer)
    }
    fn graph(&self) -> Option<Graph> {
        self.graph.borrow().clone()
    }
    fn events(&self) -> Vec<Event> {
        let raw: Vec<link::Event> = match &self.src {
            Src::Conn(c) => c.rx.try_iter().collect(),
            #[cfg(test)]
            Src::Chan(rx, _) => rx.try_iter().collect(),
        };
        let mut out = Vec::new();
        for e in raw {
            match e {
                link::Event::Msg(ServerMsg::Config { config }) => match serde_json::from_value::<Config>(config) {
                    Ok(c) => {
                        let p = self.mirror_path();
                        if let Some(d) = p.parent() {
                            let _ = std::fs::create_dir_all(d);
                        }
                        c.save_to(&p);
                        *self.mirror.borrow_mut() = Some(p);
                        *self.config.borrow_mut() = Some(c);
                        *self.fresh.borrow_mut() = true;
                        out.push(Event::Config);
                    }
                    Err(e) => eprintln!("wavelink: hub config did not parse: {e}"),
                },
                link::Event::Msg(ServerMsg::Graph { graph }) => match serde_json::from_value::<GraphWire>(graph) {
                    Ok(w) => {
                        let g = Graph::from(w);
                        *self.graph.borrow_mut() = Some(g.clone());
                        out.push(Event::Graph(g));
                    }
                    Err(e) => eprintln!("wavelink: hub graph did not parse: {e}"),
                },
                link::Event::Msg(ServerMsg::Meters { meters: m }) => out.push(Event::Meters(m)),
                link::Event::Msg(m @ ServerMsg::Ack { ok: false, .. }) => {
                    eprintln!("wavelink: hub refused a write: {m:?}");
                    out.push(Event::Link(link::Event::Msg(m)));
                }
                link::Event::Connected { server } => {
                    *self.server.borrow_mut() = server.clone();
                    out.push(Event::Link(link::Event::Connected { server }));
                }
                e => out.push(Event::Link(e)),
            }
        }
        out
    }
    fn send(&self, m: ClientMsg) {
        match &self.src {
            Src::Conn(c) => c.send(m),
            #[cfg(test)]
            Src::Chan(_, tx) => {
                let _ = tx.send(m);
            }
        }
    }
    fn ready(&self) -> bool {
        self.config.borrow().is_some() && self.graph.borrow().is_some()
    }
    fn set_volume(&self, id: u64, v: f64) {
        self.pw(PwCmd::SetVolume { id, v })
    }
    fn set_volume_mute(&self, id: u64, v: f64, mute: bool) {
        self.pw(PwCmd::SetVolumeMute { id, v, mute })
    }
    fn set_mute(&self, id: u64, mute: bool) {
        self.pw(PwCmd::SetMute { id, mute })
    }
    fn set_eq(&self, id: u64, eq: &[f64]) {
        self.pw(PwCmd::SetEq { id, eq: eq.to_vec() })
    }
    fn set_rack(&self, id: u64, rack: &Rack) {
        self.pw(PwCmd::SetRack { id, rack: serde_json::to_value(rack).unwrap_or_default() })
    }
    fn move_stream(&self, serial: u64, sink: &str) {
        self.pw(PwCmd::MoveStream { serial, sink: sink.to_string() })
    }
    fn link(&self, out: &str, inp: &str) {
        self.pw(PwCmd::Link { out: out.to_string(), inp: inp.to_string() })
    }
    fn link_side(&self, out: &str, inp: &str, side: Side) {
        let side = match side {
            Side::Stereo => "stereo",
            Side::Left => "left",
            Side::Right => "right",
        };
        self.pw(PwCmd::LinkSide { out: out.to_string(), inp: inp.to_string(), side: side.to_string() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Rack};
    use crate::pw::{GraphWire, Side};
    use wavelink_proto::{ClientMsg, PwCmd, ServerMsg};
    use super::Mixer;

    #[test]
    fn locals_for_remote_are_all_false_and_local_all_true() {
        // I3: a remote window starts no window-local engine
        assert_eq!(
            Locals::for_mixer(false),
            Locals { taps: false, sysmon: false, midi: false, norm: false, leak: false, restart: false }
        );
        assert_eq!(
            Locals::for_mixer(true),
            Locals { taps: true, sysmon: true, midi: true, norm: true, leak: true, restart: true }
        );
    }

    #[test]
    fn remote_save_sends_save_config_with_base_and_mine_and_advances_base() {
        let (m, _feed, out) = RemoteMixer::for_test();
        let mut base = Config::default_config();
        let mut mine = base.clone();
        mine.inputs[0].name = "Renamed".into();
        m.save_cfg(&mut mine, &mut base, &|_, _, _| {}).unwrap();
        match out.try_recv().unwrap() {
            ClientMsg::SaveConfig { base: b, mine: mi, .. } => {
                assert_eq!(b, serde_json::to_value(Config::default_config()).unwrap(), "base is the common ancestor");
                assert_eq!(mi, serde_json::to_value(&mine).unwrap());
            }
            m => panic!("{m:?}"),
        }
        assert_eq!(base, mine, "base advances to what was sent");
    }

    #[test]
    fn remote_pull_merges_the_cached_config_three_way() {
        let (m, feed, _out) = RemoteMixer::for_test();
        let mut hub = Config::default_config();
        hub.inputs[0].name = "Hub".into();
        feed.send(ServerMsg::Config { config: serde_json::to_value(&hub).unwrap() }).unwrap();
        m.events();
        let mut mine = Config::default_config();
        mine.inputs[0].gain_db = -3.0;
        let mut base = Config::default_config();
        let got = m.pull_cfg(&mut mine, &mut base, &|_, _, _| {}).unwrap();
        assert!(got.is_some(), "a new hub config is pulled");
        assert_eq!(mine.inputs[0].name, "Hub", "their rename");
        assert_eq!(mine.inputs[0].gain_db, -3.0, "my gain");
    }

    #[test]
    fn remote_writes_map_to_pw_cmds() {
        let (m, _feed, out) = RemoteMixer::for_test();
        m.set_volume(1, 0.5);
        m.set_volume_mute(2, 0.25, true);
        m.set_mute(3, false);
        m.set_eq(4, &[1.0, 2.0]);
        m.set_rack(5, &Rack::default());
        m.move_stream(6, "sink");
        m.link("o", "i");
        m.link_side("o", "i", Side::Left);
        let got: Vec<PwCmd> = (0..8)
            .map(|_| match out.try_recv().unwrap() {
                ClientMsg::Pw { cmd, .. } => cmd,
                m => panic!("{m:?}"),
            })
            .collect();
        assert_eq!(
            got,
            vec![
                PwCmd::SetVolume { id: 1, v: 0.5 },
                PwCmd::SetVolumeMute { id: 2, v: 0.25, mute: true },
                PwCmd::SetMute { id: 3, mute: false },
                PwCmd::SetEq { id: 4, eq: vec![1.0, 2.0] },
                PwCmd::SetRack { id: 5, rack: serde_json::to_value(Rack::default()).unwrap() },
                PwCmd::MoveStream { serial: 6, sink: "sink".into() },
                PwCmd::Link { out: "o".into(), inp: "i".into() },
                PwCmd::LinkSide { out: "o".into(), inp: "i".into(), side: "left".into() },
            ]
        );
    }

    #[test]
    fn remote_is_ready_only_after_config_and_graph() {
        let (m, feed, _out) = RemoteMixer::for_test();
        assert!(!m.ready());
        feed.send(ServerMsg::Config { config: serde_json::to_value(Config::default_config()).unwrap() }).unwrap();
        m.events();
        assert!(!m.ready(), "config alone is not enough");
        feed.send(ServerMsg::Graph { graph: serde_json::to_value(GraphWire::default()).unwrap() }).unwrap();
        m.events();
        assert!(m.ready());
    }
}
