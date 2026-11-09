//! Piano section under the Spectrum analyser (#23): engine (Piano DSP / Neural stems), source,
//! on switch, live MIDI port, split lanes, Save MIDI, CPU and latency readout, and an 88-key piano
//! roll with the Bass / Mid / High lanes coloured. "Neural stems" shows the #16 Stems panel.
//! Compiled as a child of `ui` (see the #[path] mod line in ui.rs), so it can use App's private items.
use super::App;
use crate::config::glib_home;
use crate::dash::hex_rgb;
use crate::piano::{lane, HIGH_KEY, LOW_KEY};
use crate::piano_live::{Opts, Port, Session, Shared};
use crate::stemcfg;
use gtk::prelude::*;
use gtk::{glib, Orientation};
use std::cell::{Cell, RefCell};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Bass / Mid / High lane colours on the roll.
pub(super) const LANE_COLORS: [&str; 3] = ["#54a0ff", "#1dd1a1", "#feca57"];
/// Seconds of history on the roll.
const ROLL_S: f64 = 6.0;

/// Piano settings, ~/.config/wavelink-mixer/piano.json (stems.json is left as it is).
#[derive(serde::Serialize, serde::Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub(super) struct PianoCfg {
    pub source: String,
    /// "dsp" or "neural"
    pub engine: String,
    pub port: bool,
    pub split: bool,
}

impl Default for PianoCfg {
    fn default() -> Self {
        PianoCfg { source: "spotify".into(), engine: "dsp".into(), port: false, split: true }
    }
}

pub(super) fn cfg_path() -> PathBuf {
    glib_home().join(".config/wavelink-mixer/piano.json")
}

pub(super) fn load_from(p: &Path) -> PianoCfg {
    std::fs::read_to_string(p).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

/// Writes to a temp file, then renames it into place.
pub(super) fn save_to(c: &PianoCfg, p: &Path) {
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = p.with_extension("json.tmp");
    if let Ok(s) = serde_json::to_string_pretty(c) {
        if std::fs::write(&tmp, s).is_ok() {
            let _ = std::fs::rename(&tmp, p);
        }
    }
}

pub(super) struct State {
    pub(super) shared: Shared,
    session: RefCell<Option<Session>>,
    /// The "Wavelink Piano" client: opened when Live MIDI port is ticked, kept for the app's life
    /// across sessions and source changes, emptied (all-notes-off) when unticked or at shutdown.
    port: Port,
    view: RefCell<Option<Rc<View>>>,
    srcs: RefCell<Vec<(String, String)>>,
    /// The source id the running session taps.
    running: RefCell<Option<String>>,
    /// A one-off message (Save, errors) is shown until then instead of the session status.
    msg_until: Cell<Option<Instant>>,
    /// The roll needs one more paint after a stop.
    repaint: Cell<bool>,
}

impl State {
    pub(super) fn new() -> State {
        State {
            shared: Shared::new(),
            session: RefCell::new(None),
            port: Arc::new(Mutex::new(None)),
            view: RefCell::new(None),
            srcs: RefCell::new(Vec::new()),
            running: RefCell::new(None),
            msg_until: Cell::new(None),
            repaint: Cell::new(true),
        }
    }
}

pub(super) struct View {
    pub(super) root: gtk::Box,
    engine: gtk::DropDown,
    stack: gtk::Stack,
    source: gtk::DropDown,
    on: gtk::Switch,
    port: gtk::ToggleButton,
    split: gtk::ToggleButton,
    save: gtk::Button,
    clear: gtk::Button,
    status: gtk::Label,
    readout: gtk::Label,
    roll: gtk::DrawingArea,
    quiet: Cell<bool>,
}

fn name_it(w: &impl IsA<gtk::Accessible>, label: &str) {
    w.update_property(&[gtk::accessible::Property::Label(label)]);
}

impl View {
    /// `stems` is the #16 Stems panel, shown when the engine is "Neural stems".
    pub(super) fn new(stems: &gtk::Box) -> Rc<View> {
        let engine = gtk::DropDown::from_strings(&["Piano DSP", "Neural stems"]);
        engine.set_tooltip_text(Some("Piano DSP: our own CPU algorithm, song → piano MIDI. Neural stems: the separator from #16."));
        name_it(&engine, "Piano engine");
        let source = gtk::DropDown::from_strings(&[]);
        source.set_tooltip_text(Some("Channel to listen to (a passive tap: nothing is added to the mix)"));
        name_it(&source, "Piano source");
        let on = gtk::Switch::new();
        on.set_tooltip_text(Some("Listen and turn what plays into piano MIDI"));
        name_it(&on, "Piano on");
        // toggle buttons, not check buttons: they expose a press action over AT-SPI
        let port = gtk::ToggleButton::with_label("Live MIDI port");
        port.set_tooltip_text(Some("ALSA sequencer port \"Wavelink Piano\": lanes on MIDI channels 1 (Bass) / 2 (Mid) / 3 (High)"));
        let split = gtk::ToggleButton::with_label("Split lanes");
        split.set_tooltip_text(Some("Save MIDI: one track per lane (Bass < C3, Mid C3–C5, High > C5) instead of one merged track"));
        let save = gtk::Button::with_label("Save MIDI");
        let clear = gtk::Button::with_label("New take");
        clear.set_tooltip_text(Some("Start a new take: Save MIDI forgets everything played so far"));
        port.add_css_class("pianotoggle");
        split.add_css_class("pianotoggle");
        let status = gtk::Label::new(None);
        status.set_xalign(0.0);
        status.set_hexpand(true);
        status.set_ellipsize(gtk::pango::EllipsizeMode::End);
        name_it(&status, "Piano status");
        let readout = gtk::Label::new(None);
        readout.set_tooltip_text(Some(
            "Live CPU per 0.5 s of audio (read while the core idles between 10 ms chunks; the powersave governor makes it read about 1.5x the bench figure) · attack → NoteOn on the analysis stream: typically about 200 ms, up to about 380 ms on dense chords",
        ));
        name_it(&readout, "Piano CPU and latency");

        let head = gtk::Box::new(Orientation::Horizontal, 8);
        let title = gtk::Label::new(Some("PIANO"));
        title.add_css_class("section");
        head.append(&title);
        let engine_lbl = gtk::Label::new(Some("Engine"));
        engine_lbl.add_css_class("small");
        engine.update_relation(&[gtk::accessible::Relation::LabelledBy(&[engine_lbl.upcast_ref()])]);
        head.append(&engine_lbl);
        head.append(&engine);
        let stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).vhomogeneous(false).build();

        let dsp = gtk::Box::new(Orientation::Vertical, 4);
        let row = gtk::Box::new(Orientation::Horizontal, 8);
        let source_lbl = gtk::Label::new(Some("Source"));
        source_lbl.add_css_class("small");
        source.update_relation(&[gtk::accessible::Relation::LabelledBy(&[source_lbl.upcast_ref()])]);
        row.append(&source_lbl);
        row.append(&source);
        row.append(&on);
        row.append(&port);
        row.append(&split);
        row.append(&save);
        row.append(&clear);
        row.append(&status);
        row.append(&readout);
        dsp.append(&row);
        let roll: gtk::DrawingArea = glib::Object::builder().property("accessible-role", gtk::AccessibleRole::Img).build();
        name_it(&roll, "Piano roll");
        roll.set_content_height(110);
        roll.set_hexpand(true);
        roll.set_vexpand(true);
        dsp.append(&roll);
        let lanes = gtk::Box::new(Orientation::Horizontal, 12);
        for (i, l) in ["Bass &lt; C3 · ch 1", "Mid C3–C5 · ch 2", "High &gt; C5 · ch 3"].iter().enumerate() {
            let lb = gtk::Label::new(None);
            lb.set_markup(&format!("<span foreground=\"{}\">■</span> {l}", LANE_COLORS[i]));
            lb.add_css_class("small");
            lanes.append(&lb);
        }
        dsp.append(&lanes);
        stack.add_named(&dsp, Some("dsp"));
        stack.add_named(stems, Some("neural"));

        let root = gtk::Box::new(Orientation::Vertical, 4);
        root.set_margin_top(6);
        root.append(&head);
        root.append(&stack);
        Rc::new(View { root, engine, stack, source, on, port, split, save, clear, status, readout, roll, quiet: Cell::new(false) })
    }
}

fn engine_name(i: u32) -> &'static str {
    if i == 1 {
        "neural"
    } else {
        "dsp"
    }
}

/// Connects the handlers and fills the pickers from piano.json and the environment
/// (MIXER_PAGE=stems or MIXER_PIANO_ENGINE pick the engine without saving; MIXER_PIANO=1 switches it on).
pub(super) fn wire(app: &Rc<App>, v: Rc<View>) {
    *app.piano.view.borrow_mut() = Some(v.clone());
    {
        let sh = app.piano.shared.clone();
        v.roll.set_draw_func(move |_, cr, w, h| draw_roll(cr, w as f64, h as f64, &sh));
    }
    let saved = load_from(&cfg_path());
    v.quiet.set(true);
    fill_sources(app, &v, stemcfg::sources(&app.cfg.borrow()), &saved.source);
    let mut eng = if saved.engine == "neural" { 1 } else { 0 };
    if std::env::var("MIXER_PAGE").as_deref() == Ok("stems") {
        eng = 1;
    }
    match std::env::var("MIXER_PIANO_ENGINE").as_deref() {
        Ok("neural") => eng = 1,
        Ok("dsp") => eng = 0,
        _ => {}
    }
    v.engine.set_selected(eng);
    v.stack.set_visible_child_name(engine_name(eng));
    v.port.set_active(saved.port);
    v.split.set_active(saved.split);
    v.quiet.set(false);
    set_port(app, &v, saved.port);

    let (a, vv) = (app.clone(), v.clone());
    v.engine.connect_selected_notify(move |d| {
        vv.stack.set_visible_child_name(engine_name(d.selected()));
        if vv.quiet.get() {
            return;
        }
        if d.selected() == 1 {
            if vv.on.is_active() {
                vv.on.set_active(false);
            }
        } else {
            // the Stems panel is about to be hidden: never leave its session (and a live mute) running
            super::stems_view::turn_off(&a);
        }
        edit(|c| c.engine = engine_name(d.selected()).into());
    });
    let (a, vv) = (app.clone(), v.clone());
    v.source.connect_selected_notify(move |_| {
        if vv.quiet.get() {
            return;
        }
        if let Some(id) = source_id(&a, &vv) {
            edit(|c| c.source = id);
        }
        restart_if_on(&a);
    });
    let (a, vv) = (app.clone(), v.clone());
    v.port.connect_toggled(move |b| {
        if vv.quiet.get() {
            return;
        }
        let on = b.is_active();
        edit(|c| c.port = on);
        set_port(&a, &vv, on);
    });
    let vv = v.clone();
    v.split.connect_toggled(move |b| {
        if vv.quiet.get() {
            return;
        }
        let on = b.is_active();
        edit(|c| c.split = on);
    });
    let a = app.clone();
    v.on.connect_active_notify(move |sw| {
        if sw.is_active() {
            start(&a);
        } else {
            stop(&a);
        }
    });
    let (a, vv) = (app.clone(), v.clone());
    v.save.connect_clicked(move |_| save_midi(&a, &vv));
    let (a, vv) = (app.clone(), v.clone());
    v.clear.connect_clicked(move |_| {
        a.piano.shared.get().new_take();
        a.piano.repaint.set(true);
        say(&a, &vv, "new take");
    });
    if std::env::var("MIXER_PIANO").as_deref() == Ok("1") && eng == 0 {
        v.on.set_active(true);
    }
}

/// Fills or empties the long-lived port slot. A running session follows it without a restart.
/// Never opened under MIXER_NO_AUDIO.
fn set_port(app: &App, v: &View, on: bool) {
    let mut slot = app.piano.port.lock().unwrap_or_else(|e| e.into_inner());
    if on && slot.is_none() && !crate::dash::no_audio() {
        match crate::piano_live::AlsaSink::open() {
            Ok(s) => *slot = Some(crate::piano_live::MidiOut::new(s)),
            Err(e) => {
                drop(slot);
                say(app, v, &format!("MIDI port: {e}"));
                return;
            }
        }
    } else if !on {
        // dropping the MidiOut sends the note-offs and CC 123 before the client closes
        *slot = None;
    }
}

/// Shows a one-off message for 4 s, over the session status.
fn say(app: &App, v: &View, msg: &str) {
    v.status.set_text(msg);
    app.piano.msg_until.set(Some(Instant::now() + std::time::Duration::from_secs(4)));
}

/// Reads piano.json, applies one change, writes it back (env overrides are never saved).
fn edit(f: impl FnOnce(&mut PianoCfg)) {
    let p = cfg_path();
    let mut c = load_from(&p);
    f(&mut c);
    save_to(&c, &p);
}

fn source_id(app: &App, v: &View) -> Option<String> {
    app.piano.srcs.borrow().get(v.source.selected() as usize).map(|(id, _)| id.clone())
}

fn fill_sources(app: &App, v: &View, list: Vec<(String, String)>, want: &str) {
    let names: Vec<&str> = list.iter().map(|(_, n)| n.as_str()).collect();
    let was = v.quiet.replace(true);
    v.source.set_model(Some(&gtk::StringList::new(&names)));
    let idx = list.iter().position(|(id, _)| id == want).unwrap_or(0);
    v.source.set_selected(idx as u32);
    v.quiet.set(was);
    *app.piano.srcs.borrow_mut() = list;
}

/// Source change: a new session on the new tap. The take (roll and unsaved notes) and the MIDI port
/// carry over; the old session sends its note-offs before the new one starts.
fn restart_if_on(app: &Rc<App>) {
    if app.piano.session.borrow().is_some() {
        *app.piano.session.borrow_mut() = None;
        start(app);
    }
}

fn start(app: &Rc<App>) {
    let Some(v) = app.piano.view.borrow().clone() else {
        return;
    };
    let id = source_id(app, &v);
    let src = id.clone().and_then(|id| stemcfg::resolve(&app.cfg.borrow(), &id));
    let input = src.and_then(|id| app.cfg.borrow().inputs.iter().find(|i| i.id == id).cloned());
    let Some(input) = input else {
        say(app, &v, "no channel to read");
        v.on.set_active(false);
        return;
    };
    // MIXER_PIANO_WAV replays a file instead of tapping (UI drives); allowed in test mode, it plays nothing
    let wav = std::env::var_os("MIXER_PIANO_WAV").map(PathBuf::from);
    if crate::dash::no_audio() && wav.is_none() {
        // test runs: no tap, no ALSA port
        v.status.set_text(&format!("test mode: not listening to {}", input.name));
        return;
    }
    let (tap, tap_sink) = stemcfg::tap(&input);
    *app.piano.running.borrow_mut() = id;
    let port = Some(app.piano.port.clone());
    *app.piano.session.borrow_mut() = Some(Session::start_with(Opts { tap, tap_sink, port: false, wav }, app.piano.shared.clone(), port));
}

/// Drops the session (joined: its note-offs are out when this returns). The take stays for Save MIDI.
fn stop(app: &App) {
    *app.piano.session.borrow_mut() = None;
    *app.piano.running.borrow_mut() = None;
    app.piano.repaint.set(true);
    if let Some(v) = app.piano.view.borrow().as_ref() {
        if app.piano.msg_until.get().is_none_or(|t| Instant::now() >= t) {
            v.status.set_text("stopped");
        }
        v.readout.set_text("");
    }
}

/// Visuals: the roll and the readouts. Called every frame from the window tick.
pub(super) fn step(app: &Rc<App>, _dt: f64) {
    let Some(v) = app.piano.view.borrow().clone() else {
        return;
    };
    if !v.root.is_mapped() || v.stack.visible_child_name().as_deref() != Some("dsp") {
        return;
    }
    let running = app.piano.session.borrow().is_some();
    if running || app.piano.repaint.replace(false) {
        v.roll.queue_draw();
    }
    if !running {
        return;
    }
    let (status, cpu, lat) = {
        let g = app.piano.shared.get();
        (g.status.clone(), g.cpu_ms, g.latency_ms)
    };
    let st = match crate::piano_live::port_address(&app.piano.port) {
        Some(p) => format!("{status} · port {p}"),
        None => status,
    };
    let msg = app.piano.msg_until.get().is_some_and(|t| Instant::now() < t);
    if !msg && v.status.text() != st {
        v.status.set_text(&st);
    }
    let r = format!("live CPU {cpu:.1} ms/0.5 s · latency {lat:.0} ms");
    if v.readout.text() != r {
        v.readout.set_text(&r);
    }
}

/// Re-lists the channels every second (called from the 100 ms housekeeping loop). If the source a
/// session taps disappears, the session stops with a message instead of tapping a stale node.
pub(super) fn housekeep(app: &Rc<App>, refill: bool) {
    let Some(v) = app.piano.view.borrow().clone() else {
        return;
    };
    if refill {
        let list = stemcfg::sources(&app.cfg.borrow());
        if list != *app.piano.srcs.borrow() {
            let keep = source_id(app, &v).unwrap_or_default();
            let gone = !list.iter().any(|(id, _)| *id == keep);
            fill_sources(app, &v, list, &keep);
            let running = app.piano.running.borrow().clone();
            if gone && running.as_deref() == Some(keep.as_str()) {
                v.on.set_active(false);
                say(app, &v, &format!("{keep} is gone: stopped"));
            }
        }
    }
}

/// Window close: stop the session and close the port (all-notes-off first).
pub(super) fn shutdown(app: &App) {
    stop(app);
    *app.piano.port.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// home/Music/wavelink/piano-<source>-<stamp>.mid, then -2, -3... on a clash; never overwrites.
pub(super) fn create_midi(home: &Path, source: &str, stamp: &str) -> std::io::Result<(PathBuf, std::fs::File)> {
    let dir = home.join("Music").join("wavelink");
    std::fs::create_dir_all(&dir)?;
    let mut n = 1u32;
    loop {
        let path = if n == 1 { dir.join(format!("piano-{source}-{stamp}.mid")) } else { dir.join(format!("piano-{source}-{stamp}-{n}.mid")) };
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(f) => return Ok((path, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
            Err(e) => return Err(e),
        }
    }
}

fn save_midi(app: &Rc<App>, v: &View) {
    // a copy: Save never drains the take, and sounding notes are cut at now in the copy only
    let notes = app.piano.shared.get().snapshot();
    if notes.is_empty() {
        say(app, v, "no notes yet");
        return;
    }
    let src = source_id(app, v).unwrap_or_else(|| "source".into());
    let stamp = glib::DateTime::now_local().and_then(|d| d.format("%Y%m%d-%H%M%S")).map(|s| s.to_string()).unwrap_or_else(|_| "unknown-time".into());
    let bytes = crate::notes::piano_midi(&notes, v.split.is_active(), None);
    match create_midi(&glib_home(), &src, &stamp).and_then(|(p, mut f)| f.write_all(&bytes).map(|_| p)) {
        Ok(p) => say(app, v, &format!("saved {} ({} notes)", p.display(), notes.len())),
        Err(e) => say(app, v, &format!("save failed: {e}")),
    }
}

/// The roll: last ROLL_S seconds, 88 keys bottom (A0) to top (C8), faint lines on every C, notes
/// coloured by lane, sounding notes drawn up to now, a playhead at the right edge.
fn draw_roll(cr: &gtk::cairo::Context, w: f64, h: f64, sh: &Shared) {
    cr.set_source_rgb(0.07, 0.07, 0.08);
    let _ = cr.paint();
    let keys = (HIGH_KEY - LOW_KEY + 1) as f64;
    let kh = h / keys;
    let y = |p: u8| h - (p - LOW_KEY + 1) as f64 * kh;
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.07);
    for p in (LOW_KEY..=HIGH_KEY).filter(|p| p % 12 == 0) {
        cr.rectangle(0.0, y(p) + kh - 0.5, w, 1.0);
    }
    for split in [48u8, 73] {
        cr.rectangle(0.0, y(split) + kh - 0.5, w, 1.0);
    }
    let _ = cr.fill();
    // copy out, so the worker never waits on a paint
    let (roll, open, now) = {
        let g = sh.get();
        let from = g.now - ROLL_S;
        (g.roll.iter().filter(|n| n.t + n.dur >= from).cloned().collect::<Vec<_>>(), g.open.clone(), g.now)
    };
    let from = now - ROLL_S;
    let x = |t: f64| (t - from) / ROLL_S * (w - 1.0);
    let draw = |t: f64, dur: f64, p: u8, vel: u8| {
        if t + dur < from || !(LOW_KEY..=HIGH_KEY).contains(&p) {
            return;
        }
        let (r, gg, b) = hex_rgb(LANE_COLORS[lane(p).index()]);
        cr.set_source_rgba(r, gg, b, 0.45 + 0.55 * vel as f64 / 127.0);
        cr.rectangle(x(t), y(p), (dur / ROLL_S * (w - 1.0)).max(2.0), kh.max(1.5));
        let _ = cr.fill();
    };
    for n in roll.iter() {
        draw(n.t, n.dur, n.pitch, n.vel);
    }
    for &(t, p, vel, _) in open.iter() {
        draw(t, now - t, p, vel);
    }
    cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
    cr.rectangle(w - 1.0, 0.0, 1.0, h);
    let _ = cr.fill();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn piano_cfg_defaults_roundtrip_and_bad_file() {
        let d = PianoCfg::default();
        assert_eq!((d.source.as_str(), d.engine.as_str(), d.port, d.split), ("spotify", "dsp", false, true));
        let dir = std::env::temp_dir().join(format!("wl-piano-cfg-{}", std::process::id()));
        let p = dir.join("piano.json");
        let c = PianoCfg { source: "game".into(), engine: "neural".into(), port: true, split: false };
        save_to(&c, &p);
        assert_eq!(load_from(&p), c);
        std::fs::write(&p, r#"{"port":true}"#).unwrap();
        assert_eq!(load_from(&p), PianoCfg { port: true, ..PianoCfg::default() });
        std::fs::write(&p, "not json {").unwrap();
        assert_eq!(load_from(&p), PianoCfg::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_in_same_second_keeps_both_files() {
        let home = std::env::temp_dir().join(format!("wl-piano-midi-{}", std::process::id()));
        let (a, _) = create_midi(&home, "spotify", "20261108-120000").unwrap();
        let (b, _) = create_midi(&home, "spotify", "20261108-120000").unwrap();
        assert_ne!(a, b);
        assert!(a.ends_with("Music/wavelink/piano-spotify-20261108-120000.mid"), "{a:?}");
        assert!(b.ends_with("Music/wavelink/piano-spotify-20261108-120000-2.mid"), "{b:?}");
        let _ = std::fs::remove_dir_all(&home);
    }
}
