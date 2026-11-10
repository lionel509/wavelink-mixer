//! `wavelink-mixer remote` (#37): the GTK console for a second computer. It is a client of the engine's protocol
//! (State/Delta/Meters in, Command out) built from the same `dash` widgets and `ui::CSS` as the main window.
//! On a Mac it also registers itself as a host and drives the Mac's own output volume (`macvol`).
#![allow(dead_code)]
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::{gdk, glib};
use wavelink_proto::{ClientMsg, Command as Cmd, Delta, Host, HostHello, Meters, ServerMsg, State};

use crate::dash::{self, db_to_pos, pos_to_db, Vu};
use crate::link::{self, Conn, Event, HostSource};
use crate::macvol::{self, Volume, Watch};
use crate::ui::{self, CSS};

/// Fader floor shown as "-∞" (matches the engine's MIN_DB).
const DB_FLOOR: f64 = -60.0;
/// Host strip colour (the Mac has no configured colour).
const HOST_COLOUR: &str = "#8ec5ff";
/// Minimum gap between two fader sends.
const SEND_GAP: Duration = Duration::from_millis(40);
/// Redraw/poll period on the GTK thread.
const TICK: Duration = Duration::from_millis(33);

/// Parsed `remote` arguments.
#[derive(Clone)]
struct Opts {
    addr: String,
    token: String,
    label: String,
    host: bool,
}

/// `~/.config/wavelink-mixer/remote-client.json` on the Mac only (never `config.json`).
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct ClientFile {
    addr: String,
    token: String,
    label: String,
}

fn client_file_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config/wavelink-mixer/remote-client.json")
}

fn save_client_file(f: &ClientFile) -> std::io::Result<()> {
    save_client_file_to(&client_file_path(), f)
}

/// Writes the client file at `p` with mode 0600. `mode()` only applies on create, so the
/// permissions are set explicitly, which also tightens a file that already existed.
fn save_client_file_to(p: &std::path::Path, f: &ClientFile) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(p)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    let body = serde_json::to_string_pretty(f).unwrap_or_default();
    file.write_all(body.as_bytes())?;
    file.write_all(b"\n")
}

/// `wavelink-mixer remote [ADDR] [--label NAME] [--no-host] [--save]`. Returns the process exit code.
pub fn run(args: Vec<String>) -> i32 {
    let mut args = args;
    if args.first().map(|s| s.as_str()) == Some("remote") {
        args.remove(0);
    }
    let mut addr: Option<String> = None;
    let mut label: Option<String> = None;
    let mut no_host = false;
    let mut save = false;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--label" => label = it.next(),
            "--no-host" => no_host = true,
            "--save" => save = true,
            s if s.starts_with("--") => eprintln!("remote: ignoring unknown flag {s}"),
            _ => addr = Some(a),
        }
    }
    let stored: Option<ClientFile> = std::fs::read_to_string(client_file_path()).ok().and_then(|s| serde_json::from_str(&s).ok());
    let addr = match addr.or_else(|| stored.as_ref().map(|s| s.addr.clone()).filter(|s| !s.is_empty())) {
        Some(a) => a,
        None => {
            eprintln!("usage: wavelink-mixer remote <ADDR> [--label NAME] [--no-host] [--save]");
            return 2;
        }
    };
    let token = match std::env::var("WAVELINK_REMOTE_TOKEN").ok().filter(|t| !t.is_empty()).or_else(|| stored.as_ref().map(|s| s.token.clone()).filter(|t| !t.is_empty())) {
        Some(t) => t,
        None => {
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
            line.trim().to_string()
        }
    };
    if token.is_empty() {
        eprintln!("remote: no token (WAVELINK_REMOTE_TOKEN, remote-client.json, or stdin)");
        return 2;
    }
    let default_label = if cfg!(target_os = "macos") { "MacBook" } else { "Remote" };
    let label = label.or_else(|| stored.as_ref().map(|s| s.label.clone()).filter(|s| !s.is_empty())).unwrap_or_else(|| default_label.to_string());
    if save {
        let f = ClientFile { addr: addr.clone(), token: token.clone(), label: label.clone() };
        match save_client_file(&f) {
            Ok(()) => eprintln!("remote: saved {}", client_file_path().display()),
            Err(e) => {
                eprintln!("remote: could not save client file: {e}");
                return 1;
            }
        }
    }
    let opts = Opts { addr, token, label, host: cfg!(target_os = "macos") && !no_host };
    let app = adw::Application::builder().application_id("local.wavelink.Remote").build();
    app.connect_startup(|_| adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark));
    app.connect_activate(move |app| activate(app, opts.clone()));
    let code = app.run_with_args::<&str>(&[]);
    if code == glib::ExitCode::SUCCESS { 0 } else { 1 }
}

/// Host id from a hostname: lowercase, anything not `[a-z0-9]` becomes `-`, runs collapsed, `-` trimmed.
pub fn sanitize_host_id(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// This computer's host id: `hostname -s`, sanitised; "mac" when that gives nothing.
fn host_id() -> String {
    let raw = std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    let id = sanitize_host_id(raw.trim());
    if id.is_empty() { "mac".to_string() } else { id }
}

/// A host strip's edit, sent as `SetHostVolume` / `SetHostMute`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostEdit {
    Volume(u8),
    Mute(bool),
}

/// The edit a delta asks of host `my_id`, if it is for us (volume or mute). Deltas for other hosts give None.
pub fn host_edit_for(d: &Delta, my_id: &str) -> Option<HostEdit> {
    match d {
        Delta::HostVolume { host, volume } if host == my_id => Some(HostEdit::Volume(*volume)),
        Delta::HostMute { host, mute } if host == my_id => Some(HostEdit::Mute(*mute)),
        _ => None,
    }
}

/// One MacBook strip in the OUTPUTS row.
pub struct HostStrip {
    pub root: gtk::Box,
    pub id: String,
    scale: gtk::Scale,
    readout: gtk::Label,
    mute: gtk::ToggleButton,
    quiet: Rc<Cell<bool>>,
    dragging: Rc<Cell<bool>>,
}

impl HostStrip {
    /// Show a new state of this host. OFFLINE: insensitive, readout "OFFLINE".
    pub fn set(&self, h: &Host) {
        self.quiet.set(true);
        if !self.dragging.get() {
            self.scale.set_value(fader_pos(h.volume));
        }
        self.mute.set_active(h.mute);
        self.scale.set_sensitive(h.online);
        self.mute.set_sensitive(h.online);
        self.root.set_opacity(if h.online { 1.0 } else { 0.5 });
        self.readout.set_text(&if h.online { format!("{}%", h.volume) } else { "OFFLINE".to_string() });
        self.quiet.set(false);
    }
}

/// Build the host strip (`.strip.bus.host`): label, 0..=100 fader with marks 0/25/50/75/100, "<n>%" readout, M toggle.
pub fn host_strip(h: &Host, on_edit: Rc<dyn Fn(HostEdit)>) -> HostStrip {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 8);
    root.add_css_class("strip");
    root.add_css_class("bus");
    root.add_css_class("host");
    root.add_css_class(&format!("host-{}", h.id));
    root.append(&dash::screw_row());
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    bar.add_css_class("colorbar");
    bar.set_size_request(-1, 6);
    root.append(&bar);
    root.append(&ui::label(&h.label, "name"));
    let scale = gtk::Scale::with_range(gtk::Orientation::Vertical, 0.0, 1.0, 0.001);
    scale.set_inverted(true);
    scale.set_draw_value(false);
    scale.set_vexpand(true);
    scale.set_size_request(-1, 220);
    scale.add_css_class("console");
    for m in PCT_MARKS {
        scale.add_mark(fader_pos(m), gtk::PositionType::Left, Some(&m.to_string()));
    }
    let readout = ui::label("--", "db");
    readout.set_width_chars(8);
    let mute = gtk::ToggleButton::with_label("M");
    mute.add_css_class("mute");
    mute.set_halign(gtk::Align::Center);
    mute.set_tooltip_text(Some("Mute"));
    root.append(&scale);
    root.append(&readout);
    root.append(&mute);
    let quiet = Rc::new(Cell::new(false));
    let dragging = Rc::new(Cell::new(false));
    let send: Rc<dyn Fn(f64)> = {
        let e = on_edit.clone();
        Rc::new(move |p: f64| e(HostEdit::Volume((p * 100.0).round().clamp(0.0, 100.0) as u8)))
    };
    wire_fader(&scale, quiet.clone(), dragging.clone(), send);
    {
        let (q, e) = (quiet.clone(), on_edit.clone());
        mute.connect_toggled(move |b| {
            if !q.get() {
                e(HostEdit::Mute(b.is_active()));
            }
        });
    }
    let hs = HostStrip { root, id: h.id.clone(), scale, readout, mute, quiet, dragging };
    hs.set(h);
    hs
}

/// Fader marks, percent.
pub const PCT_MARKS: [u8; 5] = [0, 25, 50, 75, 100];

/// Fader position of a percent value: 0 -> 0.0, 100 -> 1.0, linear.
pub fn fader_pos(p: u8) -> f64 {
    p.min(100) as f64 / 100.0
}

/// Echo guard between the Mac's volume and the engine (#37). Our own CoreAudio writes come back as watch wakes;
/// those must not be reported as user changes.
pub struct Echo {
    last_written: Option<Volume>,
}

impl Echo {
    pub fn new() -> Echo {
        Echo { last_written: None }
    }
    /// An engine delta wants `want` on this Mac. Some = write it; None = CoreAudio already reads it (no write).
    pub fn on_delta(&mut self, current: Option<Volume>, want: Volume) -> Option<Volume> {
        if current == Some(want) {
            return None;
        }
        self.last_written = Some(want);
        Some(want)
    }
    /// A CoreAudio watch wake read `current`. Some = report it to the engine; None = it was our own write.
    pub fn on_watch(&mut self, current: Volume) -> Option<Volume> {
        if self.last_written == Some(current) {
            // our own write may wake us more than once (volume, then mute): keep swallowing that value
            return None;
        }
        self.last_written = None;
        Some(current)
    }
}

/// Fader position of a dB value on the console taper (`dash::db_to_pos`), exposed for the strips.
fn db_text(db: f64) -> String {
    if db <= DB_FLOOR { "-∞ dB".to_string() } else { format!("{db:+.1} dB") }
}

/// Wire a fader: sends its position at most every `SEND_GAP` while moving, and always on release.
/// Programmatic moves (`quiet`) are never sent; a fader being dragged (`dragging`) is not refreshed from state.
fn wire_fader(scale: &gtk::Scale, quiet: Rc<Cell<bool>>, dragging: Rc<Cell<bool>>, send: Rc<dyn Fn(f64)>) {
    let pending: Rc<Cell<Option<f64>>> = Rc::new(Cell::new(None));
    let last: Rc<Cell<Option<Instant>>> = Rc::new(Cell::new(None));
    let armed: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let flush: Rc<dyn Fn()> = {
        let (pending, last, send) = (pending.clone(), last.clone(), send.clone());
        Rc::new(move || {
            if let Some(p) = pending.take() {
                last.set(Some(Instant::now()));
                send(p);
            }
        })
    };
    {
        let (quiet, pending, last, armed, flush) = (quiet.clone(), pending.clone(), last.clone(), armed.clone(), flush.clone());
        scale.connect_value_changed(move |s| {
            if quiet.get() {
                return;
            }
            pending.set(Some(s.value()));
            let due = last.get().map_or(true, |t| t.elapsed() >= SEND_GAP);
            if due {
                flush();
            } else if !armed.get() {
                armed.set(true);
                let (armed, flush) = (armed.clone(), flush.clone());
                glib::timeout_add_local_once(SEND_GAP, move || {
                    armed.set(false);
                    flush();
                });
            }
        });
    }
    let drag = gtk::GestureDrag::new();
    drag.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let d = dragging.clone();
        drag.connect_drag_begin(move |_, _, _| d.set(true));
    }
    {
        let (d, f) = (dragging.clone(), flush.clone());
        drag.connect_drag_end(move |_, _, _| {
            d.set(false);
            f();
        });
    }
    scale.add_controller(drag);
}

/// A dB fader with the same taper and marks as the main window.
fn db_scale(height: i32) -> gtk::Scale {
    let scale = gtk::Scale::with_range(gtk::Orientation::Vertical, 0.0, 1.0, 0.001);
    scale.set_inverted(true);
    scale.set_draw_value(false);
    scale.set_vexpand(true);
    scale.set_size_request(-1, height);
    scale.add_css_class("console");
    for (d, t) in [(12.0, "+12"), (6.0, "+6"), (0.0, "0"), (-5.0, "5"), (-10.0, "10"), (-20.0, "20"), (-30.0, "30"), (-40.0, "40"), (-60.0, "∞")] {
        scale.add_mark(db_to_pos(d), gtk::PositionType::Left, Some(t));
    }
    scale
}

/// Per-strip widgets of one input.
struct InputUi {
    id: String,
    root: gtk::Box,
    scale: gtk::Scale,
    readout: gtk::Label,
    mute: gtk::ToggleButton,
    routes: Vec<(String, gtk::ToggleButton)>,
    vu: Rc<Vu>,
    quiet: Rc<Cell<bool>>,
    dragging: Rc<Cell<bool>>,
}

/// Per-strip widgets of one bus.
struct BusUi {
    id: String,
    scale: gtk::Scale,
    readout: gtk::Label,
    mute: gtk::ToggleButton,
    vu: Rc<Vu>,
    quiet: Rc<Cell<bool>>,
    dragging: Rc<Cell<bool>>,
}

/// The console window's state. Lives on the GTK thread only.
struct Console {
    win: adw::ApplicationWindow,
    pill: gtk::Label,
    body: gtk::Box,
    dynamic: gtk::CssProvider,
    conn: RefCell<Option<Conn>>,
    conn_text: RefCell<String>,
    state: RefCell<State>,
    meters: RefCell<Meters>,
    inputs: RefCell<Vec<InputUi>>,
    buses: RefCell<Vec<BusUi>>,
    hosts: RefCell<Vec<HostStrip>>,
    echo: RefCell<Echo>,
    /// Our host id when this computer registered as a host (Mac only).
    my_id: Option<String>,
    /// Keeps the CoreAudio listeners alive.
    watch: RefCell<Option<Watch>>,
    /// CoreAudio watch wakes, drained on the GTK thread.
    wake: RefCell<Option<mpsc::Receiver<()>>>,
    next_id: Cell<u64>,
    /// The default width is set once, from the strips present at the first state.
    sized: Cell<bool>,
}

impl Console {
    fn send(&self, cmd: Cmd) {
        let id = self.next_id.get() + 1;
        self.next_id.set(id);
        if let Some(c) = self.conn.borrow().as_ref() {
            c.send(ClientMsg::Command { id, cmd });
        }
    }

    fn send_host_state(&self, v: Volume) {
        if let Some(c) = self.conn.borrow().as_ref() {
            c.send(ClientMsg::HostState { volume: v.volume, mute: v.mute });
        }
    }

    fn set_conn_text(&self, text: String) {
        self.pill.set_text(&text);
        *self.conn_text.borrow_mut() = text;
    }

    /// Show a line in the header for 3 s (an Ack note or an error), then the connection state again.
    fn flash(self: &Rc<Self>, text: &str) {
        self.pill.set_text(text);
        let c = self.clone();
        glib::timeout_add_local_once(Duration::from_secs(3), move || {
            let t = c.conn_text.borrow().clone();
            c.pill.set_text(&t);
        });
    }

    /// Rebuild every strip from the state (structure changed: a strip was added, removed or relabelled).
    fn rebuild(self: &Rc<Self>) {
        while let Some(c) = self.body.first_child() {
            self.body.remove(&c);
        }
        self.inputs.borrow_mut().clear();
        self.buses.borrow_mut().clear();
        self.hosts.borrow_mut().clear();
        let st = self.state.borrow().clone();
        let mut css = String::new();

        let ins = gtk::Box::new(gtk::Orientation::Vertical, 6);
        ins.append(&section("INPUTS"));
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.set_vexpand(true);
        for inp in &st.inputs {
            let u = self.input_strip(inp, &st.buses, &mut css);
            row.append(&u.root);
            self.inputs.borrow_mut().push(u);
        }
        ins.append(&row);

        let outs = gtk::Box::new(gtk::Orientation::Vertical, 6);
        outs.append(&section("OUTPUTS"));
        let orow = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        orow.set_vexpand(true);
        for bus in &st.buses {
            let u = self.bus_strip(bus, &mut css);
            orow.append(&u.0);
            self.buses.borrow_mut().push(u.1);
        }
        for h in &st.hosts {
            let cls = format!("host-{}", h.id);
            css.push_str(&colour_css(&cls, HOST_COLOUR));
            let me = self.clone();
            let id = h.id.clone();
            let on_edit: Rc<dyn Fn(HostEdit)> = Rc::new(move |e| {
                me.send(match e {
                    HostEdit::Volume(v) => Cmd::SetHostVolume { host: id.clone(), volume: v },
                    HostEdit::Mute(m) => Cmd::SetHostMute { host: id.clone(), mute: m },
                });
            });
            let s = host_strip(h, on_edit);
            orow.append(&s.root);
            self.hosts.borrow_mut().push(s);
        }
        outs.append(&orow);

        self.body.append(&ins);
        self.body.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        self.body.append(&outs);
        self.dynamic.load_from_string(&css);
        self.refresh();
        if !self.sized.get() && st.inputs.len() + st.buses.len() + st.hosts.len() > 0 {
            self.sized.set(true);
            // Every strip is in the body, so its natural width is what the window needs.
            let natural = self.body.measure(gtk::Orientation::Horizontal, -1).1;
            let w = default_width(natural, monitor_width(&self.win));
            self.win.set_default_size(w, self.win.default_height());
        }
    }

    fn input_strip(self: &Rc<Self>, inp: &wavelink_proto::Input, buses: &[wavelink_proto::Bus], css: &mut String) -> InputUi {
        let strip = gtk::Box::new(gtk::Orientation::Vertical, 8);
        strip.add_css_class("strip");
        let cls = format!("in-{}", inp.id);
        strip.add_css_class(&cls);
        css.push_str(&colour_css(&cls, &inp.color));
        strip.append(&dash::screw_row());
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        bar.add_css_class("colorbar");
        bar.set_size_request(-1, 6);
        strip.append(&bar);
        strip.append(&ui::label(&inp.name, "name"));
        let vu = Vu::new(64);
        strip.append(&vu.area);
        let scale = db_scale(220);
        strip.append(&scale);
        let readout = ui::label("--", "db");
        readout.set_width_chars(8);
        strip.append(&readout);
        let mute = gtk::ToggleButton::with_label("M");
        mute.add_css_class("mute");
        mute.set_halign(gtk::Align::Center);
        strip.append(&mute);
        let quiet = Rc::new(Cell::new(false));
        let dragging = Rc::new(Cell::new(false));
        let send: Rc<dyn Fn(f64)> = {
            let me = self.clone();
            let id = inp.id.clone();
            Rc::new(move |p: f64| me.send(Cmd::SetInputGain { input: id.clone(), db: pos_to_db(p) }))
        };
        wire_fader(&scale, quiet.clone(), dragging.clone(), send);
        {
            let (q, me, id) = (quiet.clone(), self.clone(), inp.id.clone());
            mute.connect_toggled(move |b| {
                if !q.get() {
                    me.send(Cmd::SetInputMute { input: id.clone(), mute: b.is_active() });
                }
            });
        }
        let mut routes = Vec::new();
        let rbox = gtk::Box::new(gtk::Orientation::Vertical, 4);
        for bus in buses {
            let t = gtk::ToggleButton::with_label(&bus.name);
            t.add_css_class("route");
            let (q, me, iid, bid) = (quiet.clone(), self.clone(), inp.id.clone(), bus.id.clone());
            t.connect_toggled(move |b| {
                if !q.get() {
                    me.send(Cmd::SetRoute { input: iid.clone(), bus: bid.clone(), on: Some(b.is_active()), level_db: None });
                }
            });
            rbox.append(&t);
            routes.push((bus.id.clone(), t));
        }
        strip.append(&rbox);
        InputUi { id: inp.id.clone(), root: strip, scale, readout, mute, routes, vu, quiet, dragging }
    }

    /// Returns (root widget, per-strip handles).
    fn bus_strip(self: &Rc<Self>, bus: &wavelink_proto::Bus, css: &mut String) -> (gtk::Box, BusUi) {
        let strip = gtk::Box::new(gtk::Orientation::Vertical, 8);
        strip.add_css_class("strip");
        strip.add_css_class("bus");
        strip.append(&dash::screw_row());
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        bar.add_css_class("colorbar");
        bar.set_size_request(-1, 6);
        strip.append(&bar);
        strip.append(&ui::label(&bus.name, "name"));
        if let Some(m) = bus.max_db {
            strip.append(&ui::label(&format!("CAP {m:+.1}"), "small"));
        }
        if let Some(c) = bus.ceiling_db {
            strip.append(&ui::label(&format!("CEIL {c:+.1}"), "small"));
        }
        let cls = format!("bus-{}", bus.id);
        strip.add_css_class(&cls);
        css.push_str(&colour_css(&cls, &bus.color));
        let vu = Vu::new(64);
        strip.append(&vu.area);
        let scale = db_scale(220);
        strip.append(&scale);
        let readout = ui::label("--", "db");
        readout.set_width_chars(8);
        strip.append(&readout);
        let mute = gtk::ToggleButton::with_label("M");
        mute.add_css_class("mute");
        mute.set_halign(gtk::Align::Center);
        strip.append(&mute);
        let quiet = Rc::new(Cell::new(false));
        let dragging = Rc::new(Cell::new(false));
        let send: Rc<dyn Fn(f64)> = {
            let me = self.clone();
            let id = bus.id.clone();
            Rc::new(move |p: f64| me.send(Cmd::SetBusGain { bus: id.clone(), db: pos_to_db(p), unlock: false }))
        };
        wire_fader(&scale, quiet.clone(), dragging.clone(), send);
        {
            let (q, me, id) = (quiet.clone(), self.clone(), bus.id.clone());
            mute.connect_toggled(move |b| {
                if !q.get() {
                    me.send(Cmd::SetBusMute { bus: id.clone(), mute: b.is_active() });
                }
            });
        }
        let u = BusUi { id: bus.id.clone(), scale, readout, mute, vu, quiet, dragging };
        (strip, u)
    }

    /// Show the current state on every control, except a fader the user is dragging.
    fn refresh(&self) {
        let st = self.state.borrow();
        for u in self.inputs.borrow().iter() {
            if let Some(inp) = st.inputs.iter().find(|i| i.id == u.id) {
                u.quiet.set(true);
                if !u.dragging.get() {
                    u.scale.set_value(db_to_pos(inp.gain_db));
                }
                u.readout.set_text(&db_text(inp.gain_db));
                u.mute.set_active(inp.mute);
                for (bid, t) in &u.routes {
                    t.set_active(inp.routes.get(bid).map_or(false, |r| r.on));
                }
                u.quiet.set(false);
            }
        }
        for u in self.buses.borrow().iter() {
            if let Some(bus) = st.buses.iter().find(|b| b.id == u.id) {
                u.quiet.set(true);
                if !u.dragging.get() {
                    u.scale.set_value(db_to_pos(bus.gain_db));
                }
                u.readout.set_text(&db_text(bus.gain_db));
                u.mute.set_active(bus.mute);
                u.quiet.set(false);
            }
        }
        for h in self.hosts.borrow().iter() {
            if let Some(host) = st.hosts.iter().find(|x| x.id == h.id) {
                h.set(host);
            }
        }
    }

    /// Apply the engine's Delta to our own host and write it to CoreAudio when it is not already there.
    fn echo_host(&self, d: &Delta) {
        let Some(me) = self.my_id.as_deref() else { return };
        if host_edit_for(d, me).is_none() {
            return;
        }
        let want = match self.state.borrow().host(me) {
            Some(h) => Volume { volume: h.volume, mute: h.mute },
            None => return,
        };
        if let Some(w) = self.echo.borrow_mut().on_delta(macvol::read(), want) {
            if !macvol::write(w) {
                eprintln!("remote: CoreAudio refused volume {} mute {}", w.volume, w.mute);
            }
        }
    }

    /// One GTK-thread pass: connection events, CoreAudio wakes, meters.
    fn tick(self: &Rc<Self>) {
        let evs: Vec<Event> = match self.conn.borrow().as_ref() {
            Some(c) => c.rx.try_iter().collect(),
            None => Vec::new(),
        };
        for ev in evs {
            match ev {
                Event::Connecting(addr) => self.set_conn_text(format!("Connecting… {addr}")),
                Event::Connected { server } => {
                    self.win.set_title(Some(&format!("Mixer — {server}")));
                    self.set_conn_text(server);
                }
                Event::Lost(why) => self.set_conn_text(format!("Lost: {why}")),
                Event::Fatal(msg) => self.set_conn_text(format!("Lost: {msg}")),
                Event::Msg(ServerMsg::State { state }) => {
                    let same = self.state.borrow().same_structure(&state);
                    *self.state.borrow_mut() = state;
                    if same {
                        self.refresh();
                    } else {
                        self.rebuild();
                    }
                }
                Event::Msg(ServerMsg::Delta { changes, .. }) => {
                    for d in &changes {
                        let _ = self.state.borrow_mut().apply(d);
                        self.echo_host(d);
                    }
                    self.refresh();
                }
                Event::Msg(ServerMsg::Meters { meters }) => *self.meters.borrow_mut() = meters,
                Event::Msg(ServerMsg::Ack { ok, error, note, .. }) => {
                    if !ok {
                        self.flash(&format!("Refused: {}", error.unwrap_or_default()));
                    } else if let Some(n) = note {
                        self.flash(&n);
                    }
                }
                Event::Msg(ServerMsg::Error { message, .. }) => self.flash(&format!("Error: {message}")),
                Event::Msg(_) => {}
            }
        }
        let woke = self.wake.borrow().as_ref().map_or(false, |r| r.try_iter().count() > 0);
        if woke {
            if let Some(cur) = macvol::read() {
                let report = self.echo.borrow_mut().on_watch(cur);
                if let Some(v) = report {
                    self.send_host_state(v);
                }
            }
        }
        let m = self.meters.borrow();
        for u in self.inputs.borrow().iter() {
            if let Some(l) = m.inputs.iter().find(|l| l.id == u.id) {
                u.vu.step(l.peak_db as f64, TICK.as_secs_f64());
            }
        }
        for u in self.buses.borrow().iter() {
            if let Some(l) = m.buses.iter().find(|l| l.id == u.id) {
                u.vu.step(l.peak_db as f64, TICK.as_secs_f64());
            }
        }
    }
}

/// Default window width: the strips' natural width, never under the old 1100, never over the monitor (1800 when GTK can't tell).
fn default_width(natural: i32, monitor: Option<i32>) -> i32 {
    natural.max(1100).min(monitor.unwrap_or(1800))
}

/// Width of the monitor the window is on, in logical pixels.
fn monitor_width(win: &adw::ApplicationWindow) -> Option<i32> {
    let surface = win.surface()?;
    let mon = gdk::Display::default()?.monitor_at_surface(&surface)?;
    Some(mon.geometry().width())
}

fn section(text: &str) -> gtk::Label {
    let l = ui::label(text, "section");
    l.set_xalign(0.0);
    l
}

fn colour_css(cls: &str, colour: &str) -> String {
    format!(
        ".{cls} .colorbar {{ background: {colour}; }}\n\
         .{cls} button.route:checked {{ background-color: {colour}; background-image: none; color: #111; box-shadow: none; }}\n"
    )
}

fn activate(app: &adw::Application, o: Opts) {
    let Some(display) = gdk::Display::default() else {
        eprintln!("remote: no display");
        return;
    };
    let base = gtk::CssProvider::new();
    base.load_from_string(CSS);
    gtk::style_context_add_provider_for_display(&display, &base, gtk::STYLE_PROVIDER_PRIORITY_USER);
    let dynamic = gtk::CssProvider::new();
    gtk::style_context_add_provider_for_display(&display, &dynamic, gtk::STYLE_PROVIDER_PRIORITY_USER + 1);

    // This Mac as a host: only when CoreAudio has a default output with a volume control.
    let mut host_src: Option<HostSource> = None;
    let mut my_id = None;
    let mut watch = None;
    let mut wake = None;
    if o.host {
        match macvol::read() {
            Some(_) => {
                let id = host_id();
                let (idc, lc) = (id.clone(), o.label.clone());
                host_src = Some(Box::new(move || {
                    let v = macvol::read().unwrap_or(Volume { volume: 0, mute: false });
                    HostHello { id: idc.clone(), label: lc.clone(), volume: v.volume, mute: v.mute }
                }));
                let (wtx, wrx) = mpsc::channel::<()>();
                watch = macvol::watch(move || {
                    let _ = wtx.send(());
                });
                if watch.is_none() {
                    eprintln!("remote: could not listen for system volume changes");
                }
                wake = Some(wrx);
                my_id = Some(id);
            }
            None => eprintln!("remote: no default output volume here; not registering a host"),
        }
    }

    let win = adw::ApplicationWindow::builder().application(app).title("Mixer — remote").default_width(1100).default_height(760).build();
    let pill = gtk::Label::new(Some("Connecting…"));
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&pill));
    let body = gtk::Box::new(gtk::Orientation::Horizontal, 16);
    body.set_margin_start(16);
    body.set_margin_end(16);
    body.set_margin_top(12);
    body.set_margin_bottom(12);
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_child(Some(&body));
    scroll.set_vexpand(true);
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    outer.append(&header);
    outer.append(&scroll);
    win.set_content(Some(&outer));

    let conn = link::connect(o.addr.clone(), o.token.clone(), true, host_src, || {});
    let console = Rc::new(Console {
        win: win.clone(),
        pill,
        body,
        dynamic,
        conn: RefCell::new(Some(conn)),
        conn_text: RefCell::new("Connecting…".to_string()),
        state: RefCell::new(State::default()),
        meters: RefCell::new(Meters::default()),
        inputs: RefCell::new(Vec::new()),
        buses: RefCell::new(Vec::new()),
        hosts: RefCell::new(Vec::new()),
        echo: RefCell::new(Echo::new()),
        my_id,
        watch: RefCell::new(watch),
        wake: RefCell::new(wake),
        next_id: Cell::new(0),
        sized: Cell::new(false),
    });
    let c = console.clone();
    glib::timeout_add_local(TICK, move || {
        c.tick();
        glib::ControlFlow::Continue
    });
    win.present();

    if let Ok(path) = std::env::var("MIXER_SCREENSHOT") {
        let w = win.clone();
        glib::timeout_add_local_once(Duration::from_millis(std::env::var("MIXER_SCREENSHOT_DELAY").ok().and_then(|v| v.parse().ok()).unwrap_or(3000)), move || {
            let p = gtk::WidgetPaintable::new(Some(&w));
            let (wd, ht) = (w.width(), w.height());
            let snap = gtk::Snapshot::new();
            p.snapshot(&snap, wd as f64, ht as f64);
            let node = snap.to_node();
            if node.is_none() {
                eprintln!("screenshot: window had nothing rendered ({wd}x{ht})");
            }
            if let (Some(node), Some(r)) = (node, w.renderer()) {
                let tex = r.render_texture(&node, Some(&gtk::graphene::Rect::new(0.0, 0.0, wd as f32, ht as f32)));
                let _ = tex.save_to_png(&path);
            }
            if let Some(app) = w.application() {
                app.quit();
            }
        });
    }
    let _ = console;
}

#[cfg(test)]
mod tests {
    use super::*;
    use wavelink_proto::Delta;

    #[test]
    fn save_client_file_tightens_an_existing_0644_file_to_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("wavelink-clientfile-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("remote-client.json");
        std::fs::write(&p, "{}").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        let f: ClientFile = serde_json::from_str(r#"{"addr":"127.0.0.1:8791","token":"t","label":"m"}"#).unwrap();
        save_client_file_to(&p, &f).unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn sanitize_host_id_lowercases_and_strips() {
        assert_eq!(sanitize_host_id("Lionel's MacBook Pro"), "lionel-s-macbook-pro");
        assert_eq!(sanitize_host_id("  "), "");
    }

    #[test]
    fn echo_swallows_our_own_write_once_and_reports_external_changes() {
        let v = |volume: u8| Volume { volume, mute: false };
        let mut e = Echo::new();
        // the engine wants 30 while the Mac reads 50: write it
        assert_eq!(e.on_delta(Some(v(50)), v(30)), Some(v(30)));
        // the wake our own write caused is swallowed once
        assert_eq!(e.on_watch(v(30)), None);
        // a volume key press on the Mac is reported
        assert_eq!(e.on_watch(v(80)), Some(v(80)));
        // the engine wants what CoreAudio already reads: no write (keeps two Macs from looping)
        assert_eq!(e.on_delta(Some(v(80)), v(80)), None);
    }

    #[test]
    fn delta_for_other_host_is_ignored() {
        let d = Delta::HostVolume { host: "studio".into(), volume: 40 };
        assert_eq!(host_edit_for(&d, "macbook"), None);
        let mine = Delta::HostVolume { host: "macbook".into(), volume: 40 };
        assert_eq!(host_edit_for(&mine, "macbook"), Some(HostEdit::Volume(40)));
        let mute = Delta::HostMute { host: "macbook".into(), mute: true };
        assert_eq!(host_edit_for(&mute, "macbook"), Some(HostEdit::Mute(true)));
    }

    #[test]
    fn pct_marks_and_fader_pos() {
        assert_eq!(PCT_MARKS, [0, 25, 50, 75, 100]);
        assert_eq!(fader_pos(0), 0.0);
        assert_eq!(fader_pos(25), 0.25);
        assert_eq!(fader_pos(50), 0.5);
        assert_eq!(fader_pos(100), 1.0);
    }

    #[test]
    fn default_width_fits_the_strips_and_respects_the_caps() {
        // Few strips keep the old 1100 default.
        assert_eq!(default_width(900, Some(2560)), 1100);
        // A natural width wider than 1100 is used so the last strip is not clipped.
        assert_eq!(default_width(1300, Some(2560)), 1300);
        // Capped at the monitor width when GTK can tell it.
        assert_eq!(default_width(2400, Some(1728)), 1728);
        // Without a monitor, the sane max applies.
        assert_eq!(default_width(2400, None), 1800);
    }
}
