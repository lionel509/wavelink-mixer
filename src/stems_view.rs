//! Stems panel: source, engine and plays pickers, three spectrum rows with piano rolls, Save MIDI.
//! Compiled as a child of `ui` (see the #[path] mod line in ui.rs), so it can use App's private items.
//! Unit 1 skeleton: bodies are `todo!()` until Unit 6.
#![allow(dead_code, unused_imports, unused_variables)]
use super::App;
use crate::stems::{Session, Shared};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

pub(super) struct State {
    pub(super) shared: Shared,
    pub(super) replaced: RefCell<Option<String>>,
    session: RefCell<Option<Session>>,
    view: RefCell<Option<Rc<View>>>,
    replaced_node: Cell<Option<u64>>,
    acc: Cell<f64>,
    srcs: RefCell<Vec<(String, String)>>,
}

impl State {
    pub(super) fn new() -> State {
        todo!()
    }
}

pub(super) struct View {
    pub(super) root: gtk::Box,
}

impl View {
    pub(super) fn new() -> Rc<View> {
        todo!()
    }
}

pub(super) fn wire(app: &Rc<App>, v: Rc<View>) {
    todo!()
}

pub(super) fn step(app: &Rc<App>, dt: f64) {
    todo!()
}

pub(super) fn shutdown(app: &App) {
    todo!()
}

/// home/Music/wavelink/stems-<source>-<stamp>.mid
pub(super) fn midi_path(home: &Path, source: &str, stamp: &str) -> PathBuf {
    todo!()
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
}
