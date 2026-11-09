//! Wavelink Remote: view and control a wavelink-mixer engine from another computer.
//!
//!   wavelink-remote                         open the window
//!   wavelink-remote --probe HOST[:PORT] ['<command json>'...]
//!       headless check: print the engine's state, send each command (e.g. '{"op":"set_bus_gain","bus":"a1","db":-20}'),
//!       print the acks/deltas and a few meter frames, then exit. The token comes from $WAVELINK_REMOTE_TOKEN,
//!       else the first line of stdin (never argv: `ps` would show it).
#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]
mod discover;
mod net;
mod settings;

use eframe::egui::{self, Color32, RichText, Vec2};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use wavelink_proto::{self as p, ClientMsg, Command, ServerMsg};

const FADER_H: f32 = 220.0;
const STRIP_W: f32 = 118.0;
/// Fader drags send at most this often; the last position is always sent on release.
const SEND_EVERY: Duration = Duration::from_millis(40);

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(|a| a == "--probe").unwrap_or(false) {
        let token = match std::env::var("WAVELINK_REMOTE_TOKEN") {
            Ok(t) if !t.trim().is_empty() => t,
            _ => {
                let mut line = String::new();
                let _ = std::io::stdin().read_line(&mut line);
                line
            }
        };
        std::process::exit(probe(args.get(2).cloned().unwrap_or_default(), token.trim().to_string(), args.get(3..).unwrap_or_default()));
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title("Wavelink Remote").with_inner_size([1180.0, 660.0]).with_min_inner_size([480.0, 360.0]),
        ..Default::default()
    };
    eframe::run_native("Wavelink Remote", options, Box::new(|cc| Ok(Box::new(Remote::new(&cc.egui_ctx)))))
}

/// Headless check: connect, print the snapshot and meter frames, exit 0 on success.
fn probe(addr: String, token: String, cmds: &[String]) -> i32 {
    let mut parsed = vec![];
    for (k, j) in cmds.iter().enumerate() {
        match serde_json::from_str::<Command>(j) {
            Ok(c) => parsed.push(ClientMsg::Command { id: k as u64 + 1, cmd: c }),
            Err(e) => {
                eprintln!("bad command {j}: {e}");
                return 2;
            }
        }
    }
    let c = net::connect(addr, token, true, || {});
    let t0 = Instant::now();
    let mut frames = 0;
    let mut acks = 0;
    while t0.elapsed() < Duration::from_secs(8) {
        match c.rx.recv_timeout(Duration::from_millis(200)) {
            Ok(net::Event::Connected { server }) => println!("connected to {server}"),
            Ok(net::Event::Msg(ServerMsg::State { state })) => {
                println!("rev {}  {} inputs, {} buses", state.rev, state.inputs.len(), state.buses.len());
                for i in &state.inputs {
                    let on: Vec<&str> = i.routes.iter().filter(|r| r.1.on).map(|r| r.0.as_str()).collect();
                    println!("  in  {:<10} {:>6.1} dB{} -> {}", i.id, i.gain_db, if i.mute { " MUTED" } else { "" }, on.join(","));
                }
                for b in &state.buses {
                    println!("  bus {:<10} {:>6.1} dB{}{}", b.id, b.gain_db, if b.mute { " MUTED" } else { "" }, b.max_db.map(|c| format!("  (cap {c} dB)")).unwrap_or_default());
                }
                for m in parsed.drain(..) {
                    c.send(m);
                }
            }
            Ok(net::Event::Msg(ServerMsg::Ack { id, ok, error, note })) => {
                acks += 1;
                println!("ack {id}: {}{}{}", if ok { "ok" } else { "REFUSED" }, error.map(|e| format!(" {e}")).unwrap_or_default(), note.map(|n| format!(" ({n})")).unwrap_or_default());
            }
            Ok(net::Event::Msg(ServerMsg::Delta { rev, changes })) => println!("delta rev {rev}: {changes:?}"),
            Ok(net::Event::Msg(ServerMsg::Meters { meters })) => {
                frames += 1;
                if frames <= 3 {
                    let s: Vec<String> = meters.inputs.iter().chain(&meters.buses).map(|l| format!("{}={:.0}", l.id, l.peak_db)).collect();
                    println!("meters: {}", s.join(" "));
                }
                if frames >= 15 && acks >= cmds.len() {
                    println!("{frames} meter frames in {:.1} s", t0.elapsed().as_secs_f32());
                    drop(c);
                    std::thread::sleep(Duration::from_millis(100)); // let the thread close the socket cleanly
                    return 0;
                }
            }
            Ok(net::Event::Fatal(m)) => {
                eprintln!("{m}");
                return 2;
            }
            Ok(net::Event::Lost(m)) => eprintln!("lost: {m}"),
            Ok(_) => {}
            Err(_) => {}
        }
    }
    eprintln!("timed out ({frames} meter frames)");
    1
}

fn hex(c: &str) -> Color32 {
    let h = c.trim_start_matches('#');
    let v = |i: usize| h.get(i..i + 2).and_then(|s| u8::from_str_radix(s, 16).ok()).unwrap_or(128);
    if h.len() >= 6 {
        Color32::from_rgb(v(0), v(2), v(4))
    } else {
        Color32::GRAY
    }
}

/// Fader taper: position 0..1 -> dB. The top 75 % covers -30..+12 dB evenly, the bottom quarter -60..-30.
pub fn pos_to_db(pos: f64) -> f64 {
    let p = pos.clamp(0.0, 1.0);
    let db = if p >= 0.25 { -30.0 + (p - 0.25) / 0.75 * 42.0 } else { -60.0 + p / 0.25 * 30.0 };
    if db.abs() < 0.3 {
        0.0
    } else {
        (db * 10.0).round() / 10.0
    }
}
pub fn db_to_pos(db: f64) -> f64 {
    let d = db.clamp(p::MIN_DB, p::MAX_DB);
    if d >= -30.0 {
        0.25 + (d + 30.0) / 42.0 * 0.75
    } else {
        (d + 60.0) / 30.0 * 0.25
    }
}

fn fmt_db(db: f64) -> String {
    if db <= p::MIN_DB {
        "-inf".into()
    } else {
        format!("{db:+.1} dB")
    }
}

/// A fader the user is moving: its local value wins over the server's until released and acknowledged.
struct Held {
    db: f64,
    sent_at: Instant,
    unsent: bool,
    released: Option<Instant>,
}

/// Meter ballistics: instant rise, ~20 dB/s fall, peak hold 1 s.
#[derive(Clone, Copy)]
struct Ballistic {
    rms: f32,
    peak: f32,
    hold: f32,
    hold_t: f32,
}
impl Default for Ballistic {
    fn default() -> Self {
        Ballistic { rms: -90.0, peak: -90.0, hold: -90.0, hold_t: 0.0 }
    }
}

struct Remote {
    settings: settings::Settings,
    disc: discover::Discovery,
    conn: Option<net::Conn>,
    status: String,
    fatal: Option<String>,
    server: Option<String>,
    state: Option<p::State>,
    meters: HashMap<String, Ballistic>,
    meter_raw: HashMap<String, (f32, f32)>,
    held: HashMap<String, Held>,
    /// buses this connection holds a server-side unlock for, until when (the server says, and expires it)
    unlock: HashMap<String, Instant>,
    note: Option<(String, Instant)>,
    next_id: u64,
    addr_edit: String,
    token_edit: String,
    show_token: bool,
    last_frame: Instant,
}

impl Remote {
    fn new(ctx: &egui::Context) -> Remote {
        let settings = settings::Settings::load();
        let c2 = ctx.clone();
        let disc = discover::start(move || c2.request_repaint());
        let addr_edit = settings.last.clone().unwrap_or_default();
        let token_edit = settings.tokens.get(&addr_edit).cloned().unwrap_or_default();
        let mut r = Remote {
            settings,
            disc,
            conn: None,
            status: "Not connected".into(),
            fatal: None,
            server: None,
            state: None,
            meters: HashMap::new(),
            meter_raw: HashMap::new(),
            held: HashMap::new(),
            unlock: HashMap::new(),
            note: None,
            next_id: 1,
            addr_edit,
            token_edit,
            show_token: false,
            last_frame: Instant::now(),
        };
        if !r.addr_edit.is_empty() && !r.token_edit.is_empty() {
            r.connect(ctx);
        }
        r
    }

    fn meters_on(&self) -> bool {
        self.settings.meters.unwrap_or(true)
    }

    fn connect(&mut self, ctx: &egui::Context) {
        let addr = self.addr_edit.trim().to_string();
        let token = self.token_edit.trim().to_string();
        if addr.is_empty() || token.is_empty() {
            self.fatal = Some("Enter the engine address and its token.".into());
            return;
        }
        self.settings.last = Some(addr.clone());
        self.settings.tokens.insert(addr.clone(), token.clone());
        self.settings.save();
        self.fatal = None;
        self.state = None;
        self.held.clear();
        self.unlock.clear();
        let c = ctx.clone();
        self.conn = Some(net::connect(addr, token, self.meters_on(), move || c.request_repaint()));
    }

    fn disconnect(&mut self) {
        self.conn = None;
        self.state = None;
        self.server = None;
        self.unlock.clear();
        self.status = "Not connected".into();
    }

    fn send(&mut self, cmd: Command) {
        if let Some(c) = &self.conn {
            let id = self.next_id;
            self.next_id += 1;
            c.send(ClientMsg::Command { id, cmd });
        }
    }

    fn pump(&mut self) {
        let Some(c) = &self.conn else { return };
        let evs: Vec<net::Event> = c.rx.try_iter().collect();
        for ev in evs {
            match ev {
                net::Event::Connecting(a) => self.status = format!("Connecting to {a}..."),
                net::Event::Connected { server } => {
                    self.status = format!("Connected to {server}");
                    self.server = Some(server);
                }
                net::Event::Lost(m) => {
                    self.status = format!("Disconnected ({m}), retrying...");
                    self.state = None;
                    self.unlock.clear();
                }
                net::Event::Fatal(m) => {
                    self.fatal = Some(m);
                    self.conn = None;
                    self.state = None;
                    self.status = "Not connected".into();
                    return;
                }
                net::Event::Msg(m) => self.on_msg(m),
            }
        }
    }

    fn on_msg(&mut self, m: ServerMsg) {
        match m {
            ServerMsg::State { state } => self.state = Some(state),
            ServerMsg::Delta { rev, changes } => {
                if let Some(s) = &mut self.state {
                    for d in &changes {
                        s.apply(d);
                    }
                    s.rev = rev;
                }
            }
            ServerMsg::Meters { meters } => {
                for l in meters.inputs {
                    self.meter_raw.insert(format!("in:{}", l.id), (l.peak_db, l.rms_db));
                }
                for l in meters.buses {
                    self.meter_raw.insert(format!("bus:{}", l.id), (l.peak_db, l.rms_db));
                }
            }
            ServerMsg::Ack { ok: false, error: Some(e), .. } => self.note = Some((e, Instant::now())),
            ServerMsg::Ack { note: Some(n), .. } => self.note = Some((n, Instant::now())),
            ServerMsg::Error { message, .. } => self.note = Some((message, Instant::now())),
            ServerMsg::Unlock { bus, seconds } => {
                if seconds > 0 {
                    self.unlock.insert(bus, Instant::now() + Duration::from_secs(seconds as u64));
                } else {
                    self.unlock.remove(&bus);
                }
            }
            _ => {}
        }
    }

    fn step_meters(&mut self, dt: f32) {
        for (k, (pk, rms)) in self.meter_raw.iter() {
            let b = self.meters.entry(k.clone()).or_default();
            let fall = 24.0 * dt;
            b.rms = if *rms > b.rms { *rms } else { (b.rms - fall).max(*rms) };
            b.peak = if *pk > b.peak { *pk } else { (b.peak - fall).max(*pk) };
            if *pk >= b.hold {
                b.hold = *pk;
                b.hold_t = 1.0;
            } else {
                b.hold_t -= dt;
                if b.hold_t <= 0.0 {
                    b.hold = (b.hold - fall).max(b.peak);
                }
            }
        }
    }

    /// Fader value to show and, when the user moved it, what to send. `key` is unique per fader.
    fn fader(&mut self, ui: &mut egui::Ui, key: &str, server_db: f64, color: Color32, meter: Option<String>, max_db: Option<f64>) -> Option<f64> {
        let now = Instant::now();
        // a released fader shows its own value until the server confirms it (or 1.5 s pass)
        if let Some(h) = self.held.get(key) {
            if let Some(t) = h.released {
                if !h.unsent && ((h.db - server_db).abs() < 0.05 || now.duration_since(t) > Duration::from_millis(1500)) {
                    self.held.remove(key);
                }
            }
        }
        let shown = self.held.get(key).map(|h| h.db).unwrap_or(server_db);
        let mut pos = db_to_pos(shown);
        let mut out = None;
        ui.horizontal(|ui| {
            ui.spacing_mut().slider_width = FADER_H;
            ui.spacing_mut().item_spacing.x = 4.0;
            let resp = ui.add(egui::Slider::new(&mut pos, 0.0..=1.0).vertical().show_value(false).trailing_fill(true));
            if let Some(cap) = max_db {
                // cap tick on the fader
                let r = resp.rect;
                let y = r.bottom() - (db_to_pos(cap) as f32) * r.height();
                ui.painter().line_segment([egui::pos2(r.left(), y), egui::pos2(r.right(), y)], egui::Stroke::new(2.0, Color32::from_rgb(255, 160, 40)));
            }
            if let Some(mk) = meter {
                self.draw_meter(ui, &mk, color, resp.rect.height());
            }
            if resp.changed() {
                let db = pos_to_db(pos);
                let h = self.held.entry(key.to_string()).or_insert(Held { db, sent_at: now - SEND_EVERY, unsent: false, released: None });
                h.db = db;
                h.unsent = true;
                h.released = None;
            }
            if resp.double_clicked() {
                self.held.insert(key.to_string(), Held { db: 0.0, sent_at: now - SEND_EVERY, unsent: true, released: Some(now) });
            }
            let dragging = resp.dragged();
            if let Some(h) = self.held.get_mut(key) {
                if !dragging && h.released.is_none() {
                    h.released = Some(now);
                }
                if h.unsent && (now.duration_since(h.sent_at) >= SEND_EVERY || h.released.is_some()) {
                    h.unsent = false;
                    h.sent_at = now;
                    out = Some(h.db);
                }
                if h.unsent {
                    ui.ctx().request_repaint_after(SEND_EVERY);
                }
            }
        });
        ui.label(RichText::new(fmt_db(shown)).monospace().size(12.0));
        out
    }

    fn draw_meter(&self, ui: &mut egui::Ui, key: &str, color: Color32, h: f32) {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(8.0, h), egui::Sense::hover());
        let p = ui.painter();
        p.rect_filled(rect, 2.0, Color32::from_gray(18));
        let Some(b) = self.meters.get(key) else { return };
        let frac = |db: f32| ((db + 60.0) / 60.0).clamp(0.0, 1.0);
        let top = |f: f32| rect.bottom() - f * rect.height();
        let rms = egui::Rect::from_min_max(egui::pos2(rect.left(), top(frac(b.rms))), rect.right_bottom());
        p.rect_filled(rms, 2.0, color.gamma_multiply(0.85));
        let pk = egui::Rect::from_min_max(egui::pos2(rect.left() + 2.0, top(frac(b.peak))), egui::pos2(rect.right() - 2.0, rect.bottom()));
        p.rect_filled(pk, 1.0, color.gamma_multiply(0.35));
        let hc = if b.hold > -1.0 { Color32::from_rgb(255, 60, 60) } else { Color32::from_gray(230) };
        let y = top(frac(b.hold));
        p.line_segment([egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)], egui::Stroke::new(1.5, hc));
    }

    fn input_strip(&mut self, ui: &mut egui::Ui, inp: &p::Input, buses: &[p::Bus]) {
        let color = hex(&inp.color);
        strip_frame(ui, color, |ui| {
            ui.label(RichText::new(&inp.name).strong().size(15.0));
            ui.label(RichText::new(match &inp.source { p::InputSource::Device { .. } => "device", _ => "apps" }).small().weak());
            if let Some(db) = self.fader(ui, &format!("in:{}", inp.id), inp.gain_db, color, Some(format!("in:{}", inp.id)), None) {
                self.send(Command::SetInputGain { input: inp.id.clone(), db });
            }
            ui.horizontal(|ui| {
                if ui.add(egui::Button::new(RichText::new("M").strong()).fill(if inp.mute { Color32::from_rgb(220, 50, 50) } else { Color32::from_gray(45) })).on_hover_text("Mute").clicked() {
                    self.send(Command::SetInputMute { input: inp.id.clone(), mute: !inp.mute });
                }
                if ui
                    .add(egui::Button::new(RichText::new("N").strong()).fill(if inp.normalize { Color32::from_rgb(60, 140, 230) } else { Color32::from_gray(45) }))
                    .on_hover_text("Auto-normalize (runs in the mixer window on the PC)")
                    .clicked()
                {
                    self.send(Command::SetNormalize { input: inp.id.clone(), on: Some(!inp.normalize), target_db: None });
                }
            });
            if inp.normalize {
                let mut t = inp.norm_target_db.unwrap_or(-20.0);
                let r = ui.add(egui::DragValue::new(&mut t).range(p::NORM_TARGET_MIN_DB..=p::NORM_TARGET_MAX_DB).speed(0.2).suffix(" dB").max_decimals(1)).on_hover_text("Normalize target loudness (dBFS)");
                if r.drag_stopped() || (r.changed() && !r.dragged()) {
                    self.send(Command::SetNormalize { input: inp.id.clone(), on: None, target_db: Some(t) });
                }
            }
            ui.add_space(4.0);
            egui::Grid::new(format!("routes-{}", inp.id)).num_columns(2).spacing([4.0, 3.0]).show(ui, |ui| {
                for b in buses {
                    let r = inp.route(&b.id);
                    let bc = hex(&b.color);
                    let txt = RichText::new(b.id.to_uppercase()).strong().color(if r.on { Color32::BLACK } else { bc });
                    if ui.add(egui::Button::new(txt).fill(if r.on { bc } else { Color32::from_gray(35) }).min_size(Vec2::new(36.0, 0.0))).on_hover_text(format!("Send {} to {}", inp.name, b.name)).clicked() {
                        self.send(Command::SetRoute { input: inp.id.clone(), bus: b.id.clone(), on: Some(!r.on), level_db: None });
                    }
                    let key = format!("route:{}:{}", inp.id, b.id);
                    let mut lv = self.held.get(&key).map(|h| h.db).unwrap_or(r.level_db);
                    let resp = ui.add_enabled(r.on, egui::DragValue::new(&mut lv).range(p::MIN_DB..=p::MAX_DB).speed(0.25).max_decimals(1));
                    if resp.changed() {
                        self.held.insert(key.clone(), Held { db: lv, sent_at: Instant::now(), unsent: false, released: None });
                        self.send(Command::SetRoute { input: inp.id.clone(), bus: b.id.clone(), on: None, level_db: Some(lv) });
                    }
                    if !resp.dragged() && !resp.has_focus() {
                        if let Some(h) = self.held.get(&key) {
                            if (h.db - r.level_db).abs() < 0.05 || h.sent_at.elapsed() > Duration::from_millis(1500) {
                                self.held.remove(&key);
                            }
                        }
                    }
                    ui.end_row();
                }
            });
        });
    }

    fn bus_strip(&mut self, ui: &mut egui::Ui, b: &p::Bus) {
        let color = hex(&b.color);
        strip_frame(ui, color, |ui| {
            ui.label(RichText::new(&b.name).strong().size(15.0));
            ui.label(RichText::new(format!("{} · {}", b.id.to_uppercase(), if b.physical() { "output" } else { "virtual" })).small().weak());
            let left = self.unlock.get(&b.id).map(|t| t.saturating_duration_since(Instant::now()).as_secs()).filter(|s| *s > 0);
            let unlocked = left.is_some();
            // what the server will allow: the soft cap unless unlocked, never above the hard ceiling
            let cap = match (b.max_db.filter(|_| !unlocked), b.ceiling_db) {
                (Some(a), Some(c)) => Some(a.min(c)),
                (a, c) => a.or(c),
            };
            if let Some(mut db) = self.fader(ui, &format!("bus:{}", b.id), b.gain_db, color, Some(format!("bus:{}", b.id)), cap) {
                if let Some(c) = cap {
                    db = db.min(c); // the server enforces this too
                }
                self.send(Command::SetBusGain { bus: b.id.clone(), db, unlock: false });
            }
            if ui.add(egui::Button::new(RichText::new("M").strong()).fill(if b.mute { Color32::from_rgb(220, 50, 50) } else { Color32::from_gray(45) })).clicked() {
                self.send(Command::SetBusMute { bus: b.id.clone(), mute: !b.mute });
            }
            if let Some(c) = b.ceiling_db {
                ui.label(RichText::new(format!("max {c:.0} dB")).small().color(Color32::from_rgb(255, 100, 60))).on_hover_text("Hard ceiling set on the PC: never louder");
            }
            if let Some(c) = b.max_db.filter(|c| b.ceiling_db.map_or(true, |x| *c < x)) {
                ui.label(RichText::new(format!("cap {c:.0} dB")).small().color(Color32::from_rgb(255, 160, 40)));
                let mut u = unlocked;
                let label = match left {
                    Some(s) => format!("unlocked {s}s"),
                    None => "unlock".into(),
                };
                if ui.checkbox(&mut u, label).on_hover_text("Allow this output above its safety cap for 60 s (this connection only; the ceiling and the raise limit still apply)").changed() {
                    if let Some(c) = &self.conn {
                        c.send(ClientMsg::Unlock { bus: b.id.clone(), on: u });
                    }
                }
                if unlocked {
                    ui.ctx().request_repaint_after(Duration::from_millis(500));
                }
            } else if b.physical() {
                ui.label(RichText::new("raise ≤ 12 dB/s").small().weak());
            }
        });
    }

    fn connect_view(&mut self, ui: &mut egui::Ui) {
        ui.add_space(12.0);
        ui.heading("Connect to a Wavelink mixer");
        ui.add_space(8.0);
        let found = self.disc.found.lock().map(|v| v.clone()).unwrap_or_default();
        if let Some(e) = &self.disc.error {
            ui.label(RichText::new(e).weak());
        } else if found.is_empty() {
            ui.label(RichText::new("Looking for engines on this network...").weak());
        }
        for f in &found {
            let label = format!("{}   ({}{})", f.name, f.addr, if f.ips.is_empty() { String::new() } else { format!(", {}", f.ips.join(", ")) });
            if ui.selectable_label(self.addr_edit == f.addr, label).clicked() {
                self.addr_edit = f.addr.clone();
                if let Some(t) = self.settings.tokens.get(&f.addr) {
                    self.token_edit = t.clone();
                }
            }
        }
        ui.add_space(10.0);
        egui::Grid::new("connect").num_columns(2).spacing([8.0, 8.0]).show(ui, |ui| {
            ui.label("Address");
            ui.add(egui::TextEdit::singleline(&mut self.addr_edit).hint_text("bazzite.local:8790 or 192.168.1.20").desired_width(320.0));
            ui.end_row();
            ui.label("Token");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.token_edit).password(!self.show_token).hint_text("paste from `wavelink-mixer remote-token`").desired_width(320.0));
                ui.checkbox(&mut self.show_token, "show");
            });
            ui.end_row();
        });
        ui.add_space(8.0);
        if ui.button(RichText::new("Connect").strong()).clicked() {
            let ctx = ui.ctx().clone();
            self.connect(&ctx);
        }
        if let Some(f) = &self.fatal {
            ui.add_space(8.0);
            ui.colored_label(Color32::from_rgb(255, 110, 90), f);
        }
        ui.add_space(16.0);
        ui.label(RichText::new("On the PC, run `wavelink-mixer remote-token` to see the address and token. The token is saved on this computer after the first connection.").weak());
    }
}

fn strip_frame(ui: &mut egui::Ui, color: Color32, body: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style()).fill(Color32::from_gray(26)).inner_margin(egui::Margin::same(6)).show(ui, |ui| {
        ui.set_width(STRIP_W);
        let (r, _) = ui.allocate_exact_size(Vec2::new(STRIP_W, 4.0), egui::Sense::hover());
        ui.painter().rect_filled(r, 2.0, color);
        ui.vertical_centered(body);
    });
}

impl eframe::App for Remote {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.pump();
        let dt = self.last_frame.elapsed().as_secs_f32().min(0.2);
        self.last_frame = Instant::now();
        self.step_meters(dt);
        if self.state.is_some() && self.meters_on() {
            ctx.request_repaint_after(Duration::from_millis(33));
        }
        egui::Panel::top("top").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Wavelink Remote").strong());
                ui.separator();
                ui.label(&self.status);
                if let Some((n, t)) = &self.note {
                    if t.elapsed() < Duration::from_secs(4) {
                        ui.separator();
                        ui.colored_label(Color32::from_rgb(255, 170, 60), n);
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.conn.is_some() {
                        if ui.button("Disconnect").clicked() {
                            self.disconnect();
                        }
                        let mut m = self.meters_on();
                        if ui.checkbox(&mut m, "meters").changed() {
                            self.settings.meters = Some(m);
                            self.settings.save();
                            if let Some(c) = &self.conn {
                                c.send(ClientMsg::Subscribe { meters: m });
                            }
                        }
                    }
                });
            });
        });
        egui::CentralPanel::default_margins().show(ui, |ui| {
            let Some(state) = self.state.clone() else {
                if self.conn.is_some() && self.fatal.is_none() {
                    ui.add_space(20.0);
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(&self.status);
                    });
                    ui.add_space(10.0);
                    if ui.button("Change engine or token").clicked() {
                        self.disconnect();
                    }
                } else {
                    self.connect_view(ui);
                }
                return;
            };
            egui::ScrollArea::horizontal().show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    for inp in &state.inputs {
                        self.input_strip(ui, inp, &state.buses);
                    }
                    ui.separator();
                    for b in &state.buses {
                        self.bus_strip(ui, b);
                    }
                });
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taper_round_trips() {
        for db in [-60.0, -45.0, -30.0, -20.0, -6.0, -1.5, 0.0, 3.0, 12.0] {
            assert!((pos_to_db(db_to_pos(db)) - db).abs() <= 0.1, "{db}");
        }
        assert_eq!(pos_to_db(0.0), -60.0);
        assert_eq!(pos_to_db(1.0), 12.0);
        assert!((db_to_pos(0.0) - 0.25 - 30.0 / 42.0 * 0.75).abs() < 1e-9);
    }

    #[test]
    fn colours_parse() {
        assert_eq!(hex("#1ed760"), Color32::from_rgb(0x1e, 0xd7, 0x60));
        assert_eq!(hex("bad"), Color32::GRAY);
    }

    #[test]
    fn deltas_keep_the_client_state_current() {
        let mut r = Remote {
            settings: Default::default(),
            disc: discover::Discovery { found: Default::default(), error: None, _daemon: None },
            conn: None,
            status: String::new(),
            fatal: None,
            server: None,
            state: None,
            meters: HashMap::new(),
            meter_raw: HashMap::new(),
            held: HashMap::new(),
            unlock: HashMap::new(),
            note: None,
            next_id: 1,
            addr_edit: String::new(),
            token_edit: String::new(),
            show_token: false,
            last_frame: Instant::now(),
        };
        let st = p::State {
            rev: 1,
            server: "x".into(),
            inputs: vec![],
            buses: vec![p::Bus { id: "a1".into(), name: "H".into(), color: "#fff".into(), kind: p::BusKind::Physical, gain_db: -10.0, mute: false, outputs: vec![], max_db: None, ceiling_db: None }],
            zones: vec![],
        };
        r.on_msg(ServerMsg::State { state: st });
        r.on_msg(ServerMsg::Delta { rev: 2, changes: vec![p::Delta::BusGain { bus: "a1".into(), db: -4.0 }] });
        assert_eq!(r.state.as_ref().unwrap().buses[0].gain_db, -4.0);
        assert_eq!(r.state.as_ref().unwrap().rev, 2);
        r.on_msg(ServerMsg::Ack { id: 1, ok: true, error: None, note: Some("capped".into()) });
        assert_eq!(r.note.as_ref().unwrap().0, "capped");
        r.on_msg(ServerMsg::Meters { meters: p::Meters { inputs: vec![], buses: vec![p::Level { id: "a1".into(), peak_db: -3.0, rms_db: -12.0 }] } });
        r.step_meters(0.016);
        assert_eq!(r.meters["bus:a1"].peak, -3.0);
        // unlock state comes from the server, and it can take it back
        r.on_msg(ServerMsg::Unlock { bus: "a1".into(), seconds: 60 });
        assert!(r.unlock["a1"] > Instant::now() + Duration::from_secs(50));
        r.on_msg(ServerMsg::Unlock { bus: "a1".into(), seconds: 0 });
        assert!(!r.unlock.contains_key("a1"));
    }
}
