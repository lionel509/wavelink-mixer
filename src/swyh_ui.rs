//! Network bus picker (#15): a DLNA renderer dropdown plus a "Scan network" button.
use crate::config::Renderer;
use crate::swyh;
use gtk::prelude::*;
use gtk::glib;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

const NONE_LABEL: &str = "URL only (no renderer)";

pub struct NetPick {
    pub widget: gtk::Box,
    dd: gtk::DropDown,
    list: Rc<RefCell<Vec<Renderer>>>,
}

/// Fills the dropdown with "URL only" plus the renderer names, selecting `udn` when it is in the list.
fn fill(dd: &gtk::DropDown, list: &[Renderer], udn: Option<&str>) {
    let mut labels = vec![NONE_LABEL.to_string()];
    labels.extend(list.iter().map(|r| r.name.clone()));
    let refs: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
    dd.set_model(Some(&gtk::StringList::new(&refs)));
    let pos = udn.and_then(|u| list.iter().position(|r| r.udn == u)).map(|p| p + 1).unwrap_or(0);
    dd.set_selected(pos as u32);
}

/// The udn of the renderer currently selected in the dropdown, if any.
fn selected_udn(dd: &gtk::DropDown, list: &[Renderer]) -> Option<String> {
    let i = dd.selected() as usize;
    if i == 0 {
        None
    } else {
        list.get(i - 1).map(|r| r.udn.clone())
    }
}

impl NetPick {
    pub fn new(current: Option<Renderer>) -> NetPick {
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let mut start = Vec::<Renderer>::new();
        if let Some(r) = &current {
            start.push(r.clone());
        }
        let list = Rc::new(RefCell::new(start));
        let dd = gtk::DropDown::from_strings(&[NONE_LABEL]);
        fill(&dd, &list.borrow(), current.as_ref().map(|r| r.udn.as_str()));
        let scan = gtk::Button::with_label("Scan network");
        widget.append(&dd);
        widget.append(&scan);
        // never scan on construction: bus menus are rebuilt on every structural change
        let (dd2, list2, scan2) = (dd.clone(), list.clone(), scan.clone());
        scan.connect_clicked(move |b| {
            b.set_sensitive(false);
            b.set_label("Scanning…");
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(swyh::discover(Duration::from_secs(3)));
            });
            let (dd3, list3, scan3) = (dd2.clone(), list2.clone(), scan2.clone());
            glib::timeout_add_local(Duration::from_millis(200), move || match rx.try_recv() {
                Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                res => {
                    let found = res.unwrap_or_default();
                    let keep = selected_udn(&dd3, &list3.borrow());
                    {
                        let mut l = list3.borrow_mut();
                        for f in found {
                            if !l.iter().any(|r| r.udn == f.udn) {
                                l.push(Renderer { udn: f.udn, name: f.name });
                            }
                        }
                    }
                    fill(&dd3, &list3.borrow(), keep.as_deref());
                    let empty = list3.borrow().is_empty();
                    scan3.set_sensitive(true);
                    scan3.set_label(if empty { "None found – scan again" } else { "Scan network" });
                    glib::ControlFlow::Break
                }
            });
        });
        NetPick { widget, dd, list }
    }

    /// The renderer picked, or None for "URL only".
    pub fn selected(&self) -> Option<Renderer> {
        let i = self.dd.selected() as usize;
        if i == 0 {
            None
        } else {
            self.list.borrow().get(i - 1).cloned()
        }
    }
}
