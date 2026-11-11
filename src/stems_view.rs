//! Stems panel: source, engine and plays pickers, three spectrum rows with piano rolls, Save MIDI.
//! Compiled as a child of `ui` (see the #[path] mod line in ui.rs), so it can use App's private items.
#![allow(dead_code, unused_imports, unused_variables)]
use super::App;
use crate::config::{glib_home, names};
use crate::dash::{hex_rgb, Spectrum};
use crate::notes::{save_once, save_status, COLORS, STEMS};
use crate::stemcfg::{self, Plays, SepEngine, StemsCfg};
use crate::stems::{Opts, Session, Shared};
use gtk::prelude::*;
use gtk::{glib, Orientation};
use std::cell::{Cell, RefCell};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

const ENGINES: [SepEngine; 2] = [SepEngine::Live, SepEngine::Quality];
const PLAYS: [Plays; 3] = [Plays::Midi, Plays::Stems, Plays::Nothing];

pub(super) struct State {
    pub(super) shared: Shared,
    pub(super) replaced: RefCell<Option<String>>,
    session: RefCell<Option<Session>>,
    view: RefCell<Option<Rc<View>>>,
    srcs: RefCell<Vec<(String, String)>>,
}

impl State {
    pub(super) fn new() -> State {
        State {
            shared: Shared::new(),
            replaced: RefCell::new(None),
            session: RefCell::new(None),
            view: RefCell::new(None),
            srcs: RefCell::new(Vec::new()),
        }
    }
}

pub(super) struct View {
    pub(super) root: gtk::Box,
    source: gtk::DropDown,
    engine: gtk::DropDown,
    plays: gtk::DropDown,
    replace: gtk::CheckButton,
    on: gtk::Switch,
    save: gtk::Button,
    status: gtk::Label,
    spectra: Vec<Rc<Spectrum>>,
    rolls: Vec<gtk::DrawingArea>,
    /// Set while the code changes widgets itself, so their handlers stay silent.
    quiet: Cell<bool>,
    /// Last shared.status text shown, so the label follows it only when it changes.
    seen: RefCell<String>,
}

impl View {
    pub(super) fn new() -> Rc<View> {
        let source = gtk::DropDown::from_strings(&[]);
        let engine = gtk::DropDown::from_strings(&["Live (~30 ms)", "Quality (~0.3–1.4 s)"]);
        let plays = gtk::DropDown::from_strings(&["MIDI", "Stems (no vocals)", "Nothing"]);
        let replace = gtk::CheckButton::with_label("Mute source while on");
        let on = gtk::Switch::new();
        let save = gtk::Button::with_label("Save MIDI");
        let status = gtk::Label::new(None);
        status.set_hexpand(true);
        status.set_xalign(0.0);

        let head = gtk::Box::new(Orientation::Horizontal, 8);
        head.append(&gtk::Label::new(Some("STEMS")));
        head.append(&source);
        head.append(&engine);
        head.append(&gtk::Label::new(Some("Channel plays")));
        head.append(&plays);
        head.append(&replace);
        head.append(&on);
        head.append(&save);
        head.append(&status);

        let root = gtk::Box::new(Orientation::Vertical, 2);
        root.append(&head);
        let mut spectra = Vec::new();
        let mut rolls = Vec::new();
        for i in 0..3 {
            let row = gtk::Box::new(Orientation::Horizontal, 6);
            let name = gtk::Label::new(None);
            name.set_markup(&format!("<span foreground=\"{}\">{}</span>", COLORS[i], STEMS[i]));
            name.set_size_request(64, -1);
            name.set_xalign(0.0);
            let sp = Spectrum::new(STEMS[i]);
            sp.set_compact(true);
            sp.area.set_content_height(56);
            sp.area.set_hexpand(true);
            let roll = gtk::DrawingArea::new();
            roll.set_content_width(260);
            roll.set_content_height(56);
            row.append(&name);
            row.append(&sp.area);
            row.append(&roll);
            root.append(&row);
            spectra.push(sp);
            rolls.push(roll);
        }
        Rc::new(View {
            root,
            source,
            engine,
            plays,
            replace,
            on,
            save,
            status,
            spectra,
            rolls,
            quiet: Cell::new(false),
            seen: RefCell::new(String::new()),
        })
    }
}

/// Connects every handler, fills the pickers from stems.json and the environment, and starts
/// the crash guard. Called once, right after the App exists.
pub(super) fn wire(app: &Rc<App>, v: Rc<View>) {
    *app.stems.view.borrow_mut() = Some(v.clone());
    for (i, area) in v.rolls.iter().enumerate() {
        let sh = app.stems.shared.clone();
        area.set_draw_func(move |_, cr, w, h| draw_roll(cr, w as f64, h as f64, &sh, i));
    }

    // initial values from stems.json (nothing is written back here)
    let saved = stemcfg::load();
    let want = {
        let c = app.cfg.borrow();
        stemcfg::resolve(&c, &saved.source).unwrap_or_else(|| saved.source.clone())
    };
    let list = stemcfg::sources(&app.cfg.borrow());
    fill_sources(app, &v, list, &want);
    v.quiet.set(true);
    v.engine.set_selected(ENGINES.iter().position(|e| *e == saved.engine).unwrap_or(0) as u32);
    v.plays.set_selected(PLAYS.iter().position(|p| *p == saved.plays).unwrap_or(0) as u32);
    // environment overrides: set the pickers, never save them
    if let Ok(want) = std::env::var("MIXER_STEMS_SOURCE") {
        let pos = app.stems.srcs.borrow().iter().position(|(id, _)| *id == want);
        if let Some(p) = pos {
            v.source.set_selected(p as u32);
        }
    }
    match std::env::var("MIXER_STEMS_ENGINE").as_deref() {
        Ok("quality") => v.engine.set_selected(1),
        Ok("live") => v.engine.set_selected(0),
        _ => {}
    }
    match std::env::var("MIXER_STEMS_PLAYS").as_deref() {
        Ok("midi") => v.plays.set_selected(0),
        Ok("stems") => v.plays.set_selected(1),
        Ok("nothing") => v.plays.set_selected(2),
        _ => {}
    }
    if std::env::var("MIXER_STEMS_MUTE_SOURCE").as_deref() == Ok("1") {
        v.replace.set_active(true);
    }
    update_label(app, &v);
    v.quiet.set(false);

    // crash guard: a marker left by a session that died keeps the source muted; undo it once
    // the source node is back, or give up after 40 tries (20 s)
    if let Some(id) = saved.replaced.clone() {
        let a = app.clone();
        let mut tries = 0u32;
        glib::timeout_add_local(Duration::from_millis(500), move || {
            tries += 1;
            let gone = a.node_id(&names::strip_in(&id)).is_some();
            if gone || tries >= 40 {
                if a.stems.replaced.borrow().is_none() {
                    a.apply_solo();
                    set_marker(None);
                }
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
    }

    // pickers: any change saves stems.json and restarts a running session
    let a = app.clone();
    let vv = v.clone();
    v.source.connect_selected_notify(move |_| {
        if vv.quiet.get() {
            return;
        }
        update_label(&a, &vv);
        if let Some(id) = source_id(&a, &vv) {
            changed(&a, &vv, Edit::Source(id));
        }
    });
    let a = app.clone();
    let vv = v.clone();
    v.engine.connect_selected_notify(move |_| changed(&a, &vv, Edit::Engine(ENGINES[(vv.engine.selected() as usize).min(1)])));
    let a = app.clone();
    let vv = v.clone();
    v.plays.connect_selected_notify(move |_| changed(&a, &vv, Edit::Plays(PLAYS[(vv.plays.selected() as usize).min(2)])));

    // "Mute <source> while on": live only, and only while a session runs
    let a = app.clone();
    let vv = v.clone();
    v.replace.connect_toggled(move |cb| {
        if vv.quiet.get() || a.stems.session.borrow().is_none() {
            return;
        }
        let id = if cb.is_active() { source_for_session(&a, &vv) } else { None };
        set_marker(id.clone());
        *a.stems.replaced.borrow_mut() = id;
        a.apply_solo();
    });

    // the on switch starts and stops the session
    let a = app.clone();
    let vv = v.clone();
    v.on.connect_active_notify(move |sw| {
        if sw.is_active() {
            start(&a, vv.replace.is_active());
        } else {
            stop(&a);
            vv.replace.set_active(false);
        }
    });

    // Save MIDI: writes the notes collected so far, nothing else
    let a = app.clone();
    let vv = v.clone();
    v.save.connect_clicked(move |_| save_midi(&a, &vv));

    if std::env::var("MIXER_STEMS").as_deref() == Ok("1") {
        v.on.set_active(true);
    }
}

/// Visuals only. Called every frame from the window tick, which stops while the window is unseen
/// (display asleep, minimised), so nothing that must keep running may live here: see `housekeep`.
pub(super) fn step(app: &Rc<App>, dt: f64) {
    let Some(v) = app.stems.view.borrow().clone() else {
        return;
    };
    for i in 0..3 {
        v.spectra[i].step(dt, Some(&app.stems.shared.bands[i]));
        v.rolls[i].queue_draw();
    }
    let s = app.stems.shared.status.lock().unwrap().clone();
    if *v.seen.borrow() != s {
        *v.seen.borrow_mut() = s.clone();
        v.status.set_text(&s);
    }
}

/// Correctness work that must run with no frames. Called from the 100 ms `timeout_add_local` loop in ui.rs
/// (never from the frame tick): `remute` right after a new graph arrives, `refill` every 10th pass (1 s).
pub(super) fn housekeep(app: &Rc<App>, remute: bool, refill: bool) {
    let Some(v) = app.stems.view.borrow().clone() else {
        return;
    };
    if refill {
        // re-list the channels; rebuild the picker only when the list changed
        let list = stemcfg::sources(&app.cfg.borrow());
        if list != *app.stems.srcs.borrow() {
            let keep = source_id(app, &v).unwrap_or_default();
            fill_sources(app, &v, list, &keep);
        }
    }
    if remute {
        // the engine's level re-apply, a bus re-spawn or the strip's own mute control can un-mute the
        // replaced source (same node id): check its actual mute state and mute it again
        let rep = app.stems.replaced.borrow().clone();
        let node = rep.as_ref().and_then(|id| {
            app.graph.borrow().node(&names::strip_in(id)).map(|n| (n.id, n.mute))
        });
        if let Some(id) = stemcfg::remute_target(rep.is_some(), node, None) {
            crate::pw::set_mute(id, true);
        }
    }
}

/// Stops the session and undoes the live mute. Called on window close.
pub(super) fn shutdown(app: &App) {
    stop(app);
}

/// home/Music/wavelink/stems-<source>-<stamp>.mid
pub(super) fn midi_path(home: &Path, source: &str, stamp: &str) -> PathBuf {
    home.join("Music").join("wavelink").join(format!("stems-{source}-{stamp}.mid"))
}

/// Creates the first free name from `midi_path`, then `-2`, `-3`... on a clash, so a save never replaces
/// an earlier file (two saves in the same second share a stamp). Never overwrites: `create_new`.
pub(super) fn create_midi(home: &Path, source: &str, stamp: &str) -> std::io::Result<(PathBuf, std::fs::File)> {
    let first = midi_path(home, source, stamp);
    if let Some(dir) = first.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut path = first.clone();
    let mut n = 1u32;
    loop {
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(f) => return Ok((path, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                n += 1;
                path = first.with_file_name(format!("stems-{source}-{stamp}-{n}.mid"));
            }
            Err(e) => return Err(e),
        }
    }
}

// ---------- helpers ----------

/// The pickers as they stand now, with the replace marker from memory.
fn cur_cfg(app: &App, v: &View) -> StemsCfg {
    StemsCfg {
        source: source_id(app, v).unwrap_or_else(|| stemcfg::load().source),
        engine: ENGINES[(v.engine.selected() as usize).min(1)],
        plays: PLAYS[(v.plays.selected() as usize).min(2)],
        replaced: app.stems.replaced.borrow().clone(),
    }
}

/// Writes the crash marker, leaving the other saved fields alone.
fn set_marker(id: Option<String>) {
    let mut c = stemcfg::load();
    c.replaced = id;
    c.save();
}

fn source_id(app: &App, v: &View) -> Option<String> {
    app.stems.srcs.borrow().get(v.source.selected() as usize).map(|(id, _)| id.clone())
}

/// The channel the running session reads: the picked source, resolved the way start resolves it.
fn source_for_session(app: &App, v: &View) -> Option<String> {
    let wanted = source_id(app, v)?;
    let c = app.cfg.borrow();
    stemcfg::resolve(&c, &wanted)
}

fn say(app: &App, msg: &str) {
    if let Some(v) = app.stems.view.borrow().as_ref() {
        v.status.set_text(msg);
    }
}

fn update_label(app: &App, v: &View) {
    let name = app
        .stems
        .srcs
        .borrow()
        .get(v.source.selected() as usize)
        .map(|(_, n)| n.clone())
        .unwrap_or_else(|| "source".into());
    v.replace.set_label(Some(&format!("Mute {name} while on")));
}

/// Replaces the source picker's list, keeping `want` selected when it is still there.
fn fill_sources(app: &App, v: &View, list: Vec<(String, String)>, want: &str) {
    let names: Vec<&str> = list.iter().map(|(_, n)| n.as_str()).collect();
    v.quiet.set(true);
    v.source.set_model(Some(&gtk::StringList::new(&names)));
    let idx = list.iter().position(|(id, _)| id == want).unwrap_or(0);
    v.source.set_selected(idx as u32);
    v.quiet.set(false);
    *app.stems.srcs.borrow_mut() = list;
    update_label(app, v);
}

/// One picker change. Only this field is written, on top of what stems.json holds, so a value that
/// came from an environment override (set on the picker, never saved) is never saved by accident.
enum Edit {
    Source(String),
    Engine(SepEngine),
    Plays(Plays),
}

fn apply_edit(c: &mut StemsCfg, e: Edit) {
    match e {
        Edit::Source(id) => c.source = id,
        Edit::Engine(x) => c.engine = x,
        Edit::Plays(x) => c.plays = x,
    }
}

/// A picker changed: save that one field, and restart the session if one runs.
fn changed(app: &Rc<App>, v: &Rc<View>, e: Edit) {
    if v.quiet.get() {
        return;
    }
    let mut saved = stemcfg::load();
    apply_edit(&mut saved, e);
    saved.save();
    if app.stems.session.borrow().is_some() {
        stop(app);
        start(app, v.replace.is_active());
    }
}

/// Resolves the source, adds the Stems channel if needed, sets the live mute, and starts the
/// session. On failure the on switch goes back to off.
fn start(app: &Rc<App>, replace: bool) {
    let Some(v) = app.stems.view.borrow().clone() else {
        return;
    };
    let want = cur_cfg(app, &v);
    let src = {
        let c = app.cfg.borrow();
        stemcfg::resolve(&c, &want.source)
    };
    let Some(src) = src else {
        say(app, "no channel to read");
        v.on.set_active(false);
        return;
    };
    if want.plays != Plays::Nothing {
        let added = stemcfg::ensure_channel(&mut app.cfg.borrow_mut(), &src);
        if added {
            app.structural();
        }
    }
    let input = {
        let c = app.cfg.borrow();
        c.inputs.iter().find(|i| i.id == src).cloned()
    };
    let Some(input) = input else {
        say(app, "no channel to read");
        v.on.set_active(false);
        return;
    };
    let (tap, sink) = stemcfg::tap(&input);
    if replace {
        *app.stems.replaced.borrow_mut() = Some(src.clone());
        set_marker(Some(src.clone()));
        app.apply_solo();
    }
    if !crate::dash::no_audio() {
        let opts = Opts {
            tap,
            tap_sink: sink,
            engine: want.engine,
            plays: want.plays,
            out: "ch_stems".into(),
            buf_ms: std::env::var("MIXER_STEMS_BUF_MS").ok().and_then(|s| s.parse().ok()).unwrap_or(0),
            tier: std::env::var("MIXER_STEMS_TIER").ok().and_then(|s| s.parse().ok()),
        };
        *app.stems.session.borrow_mut() = Some(Session::start(opts, app.stems.shared.clone(), None));
    }
}

/// Drops the session, and if the source was muted live, unmutes it. Touches no widgets.
fn stop(app: &App) {
    *app.stems.session.borrow_mut() = None;
    let was = app.stems.replaced.borrow_mut().take();
    if was.is_some() {
        set_marker(None);
        app.apply_solo();
    }
}

fn save_midi(app: &Rc<App>, v: &View) {
    let running = app.stems.session.borrow().is_some();
    let src = source_id(app, v).unwrap_or_else(|| "spotify".into());
    let res = save_once(&app.stems.shared.timeline, running, || {
        let stamp = glib::DateTime::now_local()
            .and_then(|d| d.format("%Y%m%d-%H%M%S"))
            .map(|s| s.to_string())
            .unwrap_or_else(|_| "unknown-time".into());
        create_midi(&glib_home(), &src, &stamp)
    });
    match res {
        Ok(s) if s.path.as_os_str().is_empty() => v.status.set_text(&save_status("", 0, s.held)),
        Ok(s) => v.status.set_text(&save_status(&s.path.display().to_string(), s.per_stem.iter().sum(), s.held)),
        Err(e) => v.status.set_text(&format!("save failed: {e}")),
    }
}

/// One piano-roll row: the last 4 s, a 1 px white playhead at the right edge.
fn draw_roll(cr: &gtk::cairo::Context, w: f64, h: f64, sh: &Shared, i: usize) {
    let now = sh.now();
    let from = now - 4.0;
    let (r, g, b) = hex_rgb(COLORS[i]);
    let x = |t: f64| (t - from) / 4.0 * (w - 1.0);
    let notes = sh.timeline.lock().unwrap().window(i, from, now);
    for n in notes {
        cr.set_source_rgba(r, g, b, 0.8);
        if n.pitch == 0 {
            cr.rectangle(x(n.t), 0.0, 2.0, h);
        } else {
            let bw = (n.dur / 4.0 * (w - 1.0)).max(2.0);
            let p = ((96.0 - n.pitch as f64) / 72.0).clamp(0.0, 1.0);
            cr.rectangle(x(n.t), p * (h - 3.0), bw, 3.0);
        }
        let _ = cr.fill();
    }
    cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
    cr.rectangle(w - 1.0, 0.0, 1.0, h);
    let _ = cr.fill();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn midi_path_format() {
        assert_eq!(
            midi_path(Path::new("/h"), "spotify", "20261108-120000"),
            PathBuf::from("/h/Music/wavelink/stems-spotify-20261108-120000.mid")
        );
    }

    /// Two saves in the same second (same stamp) keep both files and both notes lists.
    #[test]
    fn save_in_same_second_keeps_both_files() {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let home = std::env::temp_dir().join(format!("wl-save-test-{}-{nanos}", std::process::id()));
        let (p1, mut f1) = create_midi(&home, "spotify", "20261108-120000").unwrap();
        f1.write_all(b"first").unwrap();
        drop(f1);
        let (p2, mut f2) = create_midi(&home, "spotify", "20261108-120000").unwrap();
        f2.write_all(b"second").unwrap();
        drop(f2);
        assert_ne!(p1, p2, "the second save replaced the first file");
        assert_eq!(std::fs::read(&p1).unwrap(), b"first", "the first save was overwritten");
        assert_eq!(std::fs::read(&p2).unwrap(), b"second");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Picking one value saves that field only; the other saved fields stay as they were on disk.
    #[test]
    fn picker_edit_saves_only_its_field() {
        let mut saved = StemsCfg::default();
        apply_edit(&mut saved, Edit::Plays(Plays::Stems));
        assert_eq!(saved.plays, Plays::Stems);
        assert_eq!(saved.engine, SepEngine::Live, "an engine set only by MIXER_STEMS_ENGINE must not be written");
        assert_eq!(saved.source, "spotify");
        apply_edit(&mut saved, Edit::Source("ch_x".into()));
        assert_eq!(saved.source, "ch_x");
        assert_eq!(saved.plays, Plays::Stems);
    }
}
