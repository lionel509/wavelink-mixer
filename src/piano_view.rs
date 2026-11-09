//! Piano section under the Spectrum analyser (#23): engine (Piano DSP / Neural stems), source,
//! on switch, live MIDI port, split lanes, Save MIDI, CPU and latency readout, and an 88-key piano
//! roll with the Bass / Mid / High lanes coloured. "Neural stems" shows the #16 Stems panel.
//! Compiled as a child of `ui` (see the #[path] mod line in ui.rs), so it can use App's private items.
#![allow(dead_code)]
use super::App;
use crate::config::glib_home;
use crate::dash::hex_rgb;
use crate::piano::{lane, HIGH_KEY, LOW_KEY};
use crate::piano_live::{Opts, Session, Shared};
use crate::stemcfg;
use gtk::prelude::*;
use gtk::{glib, Orientation};
use std::cell::{Cell, RefCell};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;

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
    view: RefCell<Option<Rc<View>>>,
    srcs: RefCell<Vec<(String, String)>>,
}

impl State {
    pub(super) fn new() -> State {
        State { shared: Shared::new(), session: RefCell::new(None), view: RefCell::new(None), srcs: RefCell::new(Vec::new()) }
    }
}

pub(super) struct View {
    pub(super) root: gtk::Box,
    engine: gtk::DropDown,
    stack: gtk::Stack,
    source: gtk::DropDown,
    on: gtk::Switch,
    port: gtk::CheckButton,
    split: gtk::CheckButton,
    save: gtk::Button,
    status: gtk::Label,
    readout: gtk::Label,
    roll: gtk::DrawingArea,
    quiet: Cell<bool>,
}

impl View {
    /// `stems` is the #16 Stems panel, shown when the engine is "Neural stems".
    pub(super) fn new(stems: &gtk::Box) -> Rc<View> {
        let engine = gtk::DropDown::from_strings(&["Piano DSP", "Neural stems"]);
        engine.set_tooltip_text(Some("Piano DSP: our own CPU algorithm, song → piano MIDI. Neural stems: the separator from #16."));
        let source = gtk::DropDown::from_strings(&[]);
        source.set_tooltip_text(Some("Channel to listen to (a passive tap: nothing is added to the mix)"));
        let on = gtk::Switch::new();
        on.set_tooltip_text(Some("Listen and turn what plays into piano MIDI"));
        let port = gtk::CheckButton::with_label("Live MIDI port");
        port.set_tooltip_text(Some("ALSA sequencer port \"Wavelink Piano\": lanes on MIDI channels 1 (Bass) / 2 (Mid) / 3 (High)"));
        let split = gtk::CheckButton::with_label("Split lanes");
        split.set_tooltip_text(Some("Save MIDI: one track per lane (Bass < C3, Mid C3–C5, High > C5) instead of one merged track"));
        let save = gtk::Button::with_label("Save MIDI");
        let status = gtk::Label::new(None);
        status.set_xalign(0.0);
        status.set_hexpand(true);
        status.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let readout = gtk::Label::new(None);
        readout.add_css_class("small");
        readout.set_tooltip_text(Some("CPU per 0.5 s of audio · attack → NoteOn on the analysis stream"));

        let head = gtk::Box::new(Orientation::Horizontal, 8);
        head.append(&gtk::Label::new(Some("PIANO")));
        head.append(&engine);
        let stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).vhomogeneous(false).build();

        let dsp = gtk::Box::new(Orientation::Vertical, 4);
        let row = gtk::Box::new(Orientation::Horizontal, 8);
        row.append(&source);
        row.append(&on);
        row.append(&port);
        row.append(&split);
        row.append(&save);
        row.append(&status);
        row.append(&readout);
        dsp.append(&row);
        let roll = gtk::DrawingArea::new();
        roll.set_content_height(150);
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
        Rc::new(View { root, engine, stack, source, on, port, split, save, status, readout, roll, quiet: Cell::new(false) })
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

    let (a, vv) = (app.clone(), v.clone());
    v.engine.connect_selected_notify(move |d| {
        vv.stack.set_visible_child_name(engine_name(d.selected()));
        if vv.quiet.get() {
            return;
        }
        if d.selected() == 1 && vv.on.is_active() {
            vv.on.set_active(false);
        }
        edit(|c| c.engine = engine_name(d.selected()).into());
        let _ = &a;
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
        restart_if_on(&a);
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
    if std::env::var("MIXER_PIANO").as_deref() == Ok("1") && eng == 0 {
        v.on.set_active(true);
    }
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

fn restart_if_on(app: &Rc<App>) {
    if app.piano.session.borrow().is_some() {
        stop(app);
        start(app);
    }
}

fn start(app: &Rc<App>) {
    let Some(v) = app.piano.view.borrow().clone() else {
        return;
    };
    let src = source_id(app, &v).and_then(|id| stemcfg::resolve(&app.cfg.borrow(), &id));
    let input = src.and_then(|id| app.cfg.borrow().inputs.iter().find(|i| i.id == id).cloned());
    let Some(input) = input else {
        v.status.set_text("no channel to read");
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
    let port = v.port.is_active() && !crate::dash::no_audio();
    *app.piano.session.borrow_mut() = Some(Session::start(Opts { tap, tap_sink, port, wav }, app.piano.shared.clone()));
}

/// Drops the session (kills its tap; the worker sends all-notes-off and closes the port).
fn stop(app: &App) {
    *app.piano.session.borrow_mut() = None;
    if let Some(v) = app.piano.view.borrow().as_ref() {
        if crate::dash::no_audio() {
            v.status.set_text("");
        }
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
    v.roll.queue_draw();
    if app.piano.session.borrow().is_none() {
        return;
    }
    let g = app.piano.shared.get();
    let st = match &g.port {
        Some(p) => format!("{} · port {p}", g.status),
        None => g.status.clone(),
    };
    if v.status.text() != st {
        v.status.set_text(&st);
    }
    let r = format!("{:.1} ms · {:.0} ms", g.cpu_ms, g.latency_ms);
    if v.readout.text() != r {
        v.readout.set_text(&r);
    }
}

/// Re-lists the channels every second (called from the 100 ms housekeeping loop).
pub(super) fn housekeep(app: &Rc<App>, refill: bool) {
    let Some(v) = app.piano.view.borrow().clone() else {
        return;
    };
    if refill {
        let list = stemcfg::sources(&app.cfg.borrow());
        if list != *app.piano.srcs.borrow() {
            let keep = source_id(app, &v).unwrap_or_default();
            fill_sources(app, &v, list, &keep);
        }
    }
}

pub(super) fn shutdown(app: &App) {
    stop(app);
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
    let notes = app.piano.shared.get().take_notes();
    if notes.is_empty() {
        v.status.set_text("no notes yet");
        return;
    }
    let src = source_id(app, v).unwrap_or_else(|| "source".into());
    let stamp = glib::DateTime::now_local().and_then(|d| d.format("%Y%m%d-%H%M%S")).map(|s| s.to_string()).unwrap_or_else(|_| "unknown-time".into());
    let bytes = crate::notes::piano_midi(&notes, v.split.is_active(), None);
    match create_midi(&glib_home(), &src, &stamp).and_then(|(p, mut f)| f.write_all(&bytes).map(|_| p)) {
        Ok(p) => v.status.set_text(&format!("saved {} ({} notes)", p.display(), notes.len())),
        Err(e) => {
            // put the notes back so a retry can still save them
            app.piano.shared.get().all.extend(notes);
            v.status.set_text(&format!("save failed: {e}"));
        }
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
    let g = sh.get();
    let now = g.now;
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
    for n in g.roll.iter() {
        draw(n.t, n.dur, n.pitch, n.vel);
    }
    for &(t, p, vel) in g.open.iter() {
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
