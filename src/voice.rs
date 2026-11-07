//! "Discord Call" panel: one strip per person in your current Discord voice call,
//! with a speaking light, a volume slider (sets their volume inside Discord) and mute.
use crate::discord::{self, Cmd, Event, Person};
use gtk::glib;
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc::{channel, Sender};
use std::time::Duration;

pub struct Voice {
    pub root: gtk::Box,
    tx: Sender<Cmd>,
    stack: gtk::Stack,
    status: gtk::Label,
    title: gtk::Label,
    strips: gtk::Box,
    people: RefCell<Vec<Person>>,
    lights: RefCell<HashMap<String, gtk::Label>>,
    sliders: RefCell<HashMap<String, (gtk::Scale, gtk::Label, gtk::ToggleButton)>>,
    touched: Cell<i64>,
}

fn lbl(t: &str, class: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(t));
    l.add_css_class(class);
    l
}

impl Voice {
    pub fn new() -> Rc<Voice> {
        let (ctx, crx) = channel::<Cmd>();
        let (etx, erx) = channel::<Event>();
        discord::spawn(etx, crx);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
        root.set_size_request(-1, 200);
        let stack = gtk::Stack::new();
        stack.set_vexpand(true);

        // setup form
        let setup = gtk::Box::new(gtk::Orientation::Horizontal, 16);
        setup.set_margin_start(12);
        setup.set_margin_top(8);
        let help = lbl(
            "Per-person volume for your Discord calls — one-time setup:\n\
             1. discord.com/developers/applications → New Application (name it Mixer)\n\
             2. OAuth2 → add Redirect  http://127.0.0.1  → Save\n\
             3. Copy Client ID, Reset Secret → copy Client Secret, paste both here → Connect\n\
             4. Click Authorize in the Discord popup",
            "sub",
        );
        help.set_xalign(0.0);
        setup.append(&help);
        let form = gtk::Box::new(gtk::Orientation::Vertical, 6);
        form.set_size_request(320, -1);
        let cid = gtk::Entry::builder().placeholder_text("Client ID").build();
        let secret = gtk::PasswordEntry::builder().placeholder_text("Client Secret").show_peek_icon(true).build();
        let go = gtk::Button::with_label("Connect Discord");
        go.add_css_class("suggested-action");
        form.append(&cid);
        form.append(&secret);
        form.append(&go);
        setup.append(&form);
        stack.add_named(&setup, Some("setup"));

        // call view
        let call = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let title = lbl("Not in a voice call", "section");
        title.set_xalign(0.0);
        title.set_margin_start(12);
        call.append(&title);
        let strips = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        strips.set_margin_start(12);
        strips.set_margin_end(12);
        call.append(&gtk::ScrolledWindow::builder().child(&strips).vscrollbar_policy(gtk::PolicyType::Never).vexpand(true).build());
        stack.add_named(&call, Some("call"));
        root.append(&stack);
        let status = lbl("", "sub");
        status.set_xalign(0.0);
        status.set_margin_start(12);
        status.set_wrap(true);
        root.append(&status);

        let v = Rc::new(Voice {
            root,
            tx: ctx,
            stack,
            status,
            title,
            strips,
            people: RefCell::new(vec![]),
            lights: RefCell::new(HashMap::new()),
            sliders: RefCell::new(HashMap::new()),
            touched: Cell::new(0),
        });
        v.stack.set_visible_child_name("call");
        {
            let (tx, cid, secret, st) = (v.tx.clone(), cid.clone(), secret.clone(), v.status.clone());
            go.connect_clicked(move |_| {
                let (a, b) = (cid.text().trim().to_string(), secret.text().trim().to_string());
                if a.len() < 15 || b.len() < 20 {
                    st.set_label("Paste both the Client ID and the Client Secret.");
                    return;
                }
                st.set_label("Connecting to Discord…");
                let _ = tx.send(Cmd::Setup(a, b));
            });
        }
        let w = Rc::downgrade(&v);
        glib::timeout_add_local(Duration::from_millis(100), move || {
            let Some(v) = w.upgrade() else { return glib::ControlFlow::Break };
            while let Ok(e) = erx.try_recv() {
                v.handle(e);
            }
            glib::ControlFlow::Continue
        });
        v
    }

    fn handle(self: &Rc<Self>, e: Event) {
        match e {
            Event::NeedSetup => self.stack.set_visible_child_name("setup"),
            Event::Status(s) => self.status.set_label(&s),
            Event::Speaking(id, on) => {
                if let Some(l) = self.lights.borrow().get(&id) {
                    if on { l.add_css_class("talking") } else { l.remove_css_class("talking") }
                }
            }
            Event::Call(name, people) => {
                self.stack.set_visible_child_name("call");
                match &name {
                    Some(n) => self.title.set_label(&format!("DISCORD CALL · {} · {} {}", n.to_uppercase(), people.len(), if people.len() == 1 { "PERSON" } else { "PEOPLE" })),
                    None => self.title.set_label("NOT IN A DISCORD VOICE CALL"),
                }
                let same_people = {
                    let cur = self.people.borrow();
                    cur.len() == people.len() && cur.iter().zip(&people).all(|(a, b)| a.id == b.id)
                };
                if same_people {
                    // just refresh values (unless you're dragging a slider right now)
                    if glib::monotonic_time() > self.touched.get() {
                        for p in &people {
                            if let Some((s, l, m)) = self.sliders.borrow().get(&p.id) {
                                s.set_value(p.volume);
                                l.set_label(&format!("{:.0}%", p.volume));
                                m.set_active(p.muted);
                            }
                        }
                    }
                } else {
                    self.build(&people);
                }
                *self.people.borrow_mut() = people;
            }
        }
    }

    fn build(self: &Rc<Self>, people: &[Person]) {
        while let Some(c) = self.strips.first_child() {
            self.strips.remove(&c);
        }
        self.lights.borrow_mut().clear();
        self.sliders.borrow_mut().clear();
        if people.is_empty() {
            let l = lbl("Join a voice channel in Discord and everyone in it shows up here.", "sub");
            self.strips.append(&l);
            return;
        }
        for p in people {
            let col = gtk::Box::new(gtk::Orientation::Vertical, 4);
            col.add_css_class("person");
            col.set_size_request(92, -1);
            let light = lbl("●", "speaklight");
            let name = lbl(&p.name, "name");
            name.set_ellipsize(gtk::pango::EllipsizeMode::End);
            name.set_max_width_chars(10);
            let head = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            head.set_halign(gtk::Align::Center);
            head.append(&light);
            head.append(&name);
            col.append(&head);
            let s = gtk::Scale::with_range(gtk::Orientation::Vertical, 0.0, 200.0, 1.0);
            s.set_inverted(true);
            s.set_draw_value(false);
            s.set_vexpand(true);
            s.set_size_request(-1, 90);
            s.add_mark(100.0, gtk::PositionType::Left, Some("100"));
            s.add_css_class("console");
            s.set_value(p.volume);
            let val = lbl(&format!("{:.0}%", p.volume), "db");
            let mute = gtk::ToggleButton::with_label("M");
            mute.add_css_class("mute");
            mute.set_halign(gtk::Align::Center);
            mute.set_active(p.muted);
            mute.set_tooltip_text(Some(&format!("Mute {} for you only", p.name)));
            {
                let (tx, id, val, w) = (self.tx.clone(), p.id.clone(), val.clone(), Rc::downgrade(self));
                s.connect_change_value(move |_, _, v| {
                    let v = v.clamp(0.0, 200.0).round();
                    val.set_label(&format!("{v:.0}%"));
                    if let Some(me) = w.upgrade() {
                        me.touched.set(glib::monotonic_time() + 1_500_000);
                    }
                    let _ = tx.send(Cmd::Volume(id.clone(), v));
                    glib::Propagation::Proceed
                });
            }
            {
                // double-click = back to 100%
                let (s2, tx, id, val) = (s.clone(), self.tx.clone(), p.id.clone(), val.clone());
                let g = gtk::GestureClick::new();
                g.set_propagation_phase(gtk::PropagationPhase::Capture);
                g.connect_pressed(move |g, n, _, _| {
                    if n == 2 {
                        s2.set_value(100.0);
                        val.set_label("100%");
                        let _ = tx.send(Cmd::Volume(id.clone(), 100.0));
                        g.set_state(gtk::EventSequenceState::Claimed);
                    }
                });
                s.add_controller(g);
            }
            {
                let (tx, id) = (self.tx.clone(), p.id.clone());
                mute.connect_toggled(move |m| {
                    let _ = tx.send(Cmd::Mute(id.clone(), m.is_active()));
                });
            }
            col.append(&s);
            col.append(&val);
            col.append(&mute);
            self.strips.append(&col);
            self.lights.borrow_mut().insert(p.id.clone(), light);
            self.sliders.borrow_mut().insert(p.id.clone(), (s, val, mute));
        }
    }
}
