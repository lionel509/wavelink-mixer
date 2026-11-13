//! Player page: a Spotify client inside the mixer, laid out like Spotify's desktop app.
//! Browsing/control via the Web API, audio from librespot (Connect device "Mixer")
//! straight into the mixer's Spotify channel.
use crate::spotify::{self, Api};
use gtk::prelude::*;
use gtk::{gdk, glib};
use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
mod api;
mod images;
mod view;

#[derive(Clone, Debug)]
struct Item {
    title: String,
    artists: String,
    album: String,
    uri: String,
    dur_ms: u64,
    img: String,
}

#[derive(Clone, Debug)]
struct Playlist {
    name: String,
    id: String,
    uri: String,
    img: String,
    owner: String,
    count: u64,
}

/// What the main area is showing.
#[derive(Clone)]
struct View {
    kind: &'static str, // "PLAYLIST", "LIKED", "SEARCH", "QUEUE"
    title: String,
    img: String,
    ctx: Option<String>,
}

enum Msg {
    Connected(Arc<Api>, String),
    Status(String),
    State(Value),
    Device(Option<String>),
    Playlists(Vec<Playlist>),
    List(View, Vec<Item>),
    Image(String, Vec<u8>),
}

fn fmt_ms(ms: f64) -> String {
    let s = (ms / 1000.0).max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

fn lbl(t: &str, class: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(t));
    l.add_css_class(class);
    l.set_xalign(0.0);
    l.set_ellipsize(gtk::pango::EllipsizeMode::End);
    l
}

fn icon_btn(icon: &str, tip: &str) -> gtk::Button {
    let b = gtk::Button::from_icon_name(icon);
    b.set_tooltip_text(Some(tip));
    b.add_css_class("sp-icon");
    b
}

/// Smallest image at least `min` px wide (Spotify lists biggest first).
fn pick_img(v: &Value, min: u64) -> String {
    let imgs = v.as_array().cloned().unwrap_or_default();
    imgs.iter()
        .rev()
        .find(|i| i["width"].as_u64().unwrap_or(640) >= min)
        .or(imgs.first())
        .and_then(|i| i["url"].as_str())
        .unwrap_or("")
        .to_string()
}

fn track_item(t: &Value) -> Option<Item> {
    let uri = t["uri"].as_str()?.to_string();
    let artists: Vec<&str> = t["artists"].as_array().map(|a| a.iter().filter_map(|x| x["name"].as_str()).collect()).unwrap_or_default();
    Some(Item {
        title: t["name"].as_str().unwrap_or("?").to_string(),
        artists: artists.join(", "),
        album: t["album"]["name"].as_str().unwrap_or("").to_string(),
        uri,
        dur_ms: t["duration_ms"].as_u64().unwrap_or(0),
        img: pick_img(&t["album"]["images"], 64),
    })
}

/// A fixed-size square cover image (never grows to the texture's natural size).
fn cover(px: i32, round: bool) -> (gtk::Box, gtk::Picture) {
    let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
    frame.set_size_request(px, px);
    frame.set_hexpand(false);
    frame.set_vexpand(false);
    frame.set_halign(gtk::Align::Start);
    frame.set_valign(gtk::Align::Center);
    frame.set_overflow(gtk::Overflow::Hidden);
    frame.add_css_class(if round { "sp-cover-round" } else { "sp-cover" });
    let pic = gtk::Picture::new();
    pic.set_can_shrink(true);
    pic.set_content_fit(gtk::ContentFit::Cover);
    pic.set_size_request(px, px);
    pic.set_vexpand(true);
    frame.append(&pic);
    (frame, pic)
}

pub struct Player {
    pub root: gtk::Box,
    api: RefCell<Option<Arc<Api>>>,
    user: RefCell<String>,
    tx: Sender<Msg>,
    stack: gtk::Stack,
    status: gtk::Label,
    // images: url -> texture, plus pictures waiting for a url
    textures: RefCell<HashMap<String, gdk::Texture>>,
    waiting: RefCell<HashMap<String, Vec<glib::WeakRef<gtk::Picture>>>>,
    img_q: Arc<Mutex<Vec<String>>>,
    // library
    lib: gtk::ListBox,
    playlists: RefCell<Vec<Playlist>>,
    // main area
    main: gtk::Stack,
    home_grid: gtk::FlowBox,
    head_pic: gtk::Picture,
    head_kind: gtk::Label,
    head_title: gtk::Label,
    head_meta: gtk::Label,
    head_box: gtk::Box,
    list: gtk::ListBox,
    items: RefCell<(View, Vec<Item>)>,
    rows: RefCell<Vec<(String, gtk::Label, gtk::Label)>>, // uri, number label, title label
    // now playing
    now_pic: gtk::Picture,
    now_url: RefCell<String>,
    now_title: gtk::Label,
    now_sub: gtk::Label,
    now_uri: RefCell<String>,
    play_btn: gtk::Button,
    shuffle: gtk::ToggleButton,
    repeat: gtk::Button,
    repeat_state: RefCell<String>,
    progress: gtk::Scale,
    t_pos: gtk::Label,
    t_len: gtk::Label,
    volume: gtk::Scale,
    device_btn: gtk::Button,
    device_lbl: gtk::Label,
    mixer_id: RefCell<Option<String>>,
    updating: Cell<bool>,
    playing: Cell<bool>,
    pos_ms: Cell<f64>,
    dur_ms: Cell<f64>,
    hold_until: Cell<i64>,
}

impl Player {
    pub fn new() -> Rc<Player> {
        let (tx, rx) = channel::<Msg>();
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("sp");
        let stack = gtk::Stack::new();
        stack.set_vexpand(true);

        // ---------- connect screen ----------
        let connect = gtk::Box::new(gtk::Orientation::Vertical, 12);
        connect.set_halign(gtk::Align::Center);
        connect.set_valign(gtk::Align::Center);
        connect.set_size_request(520, -1);
        let h = lbl("Connect Spotify", "sp-h1");
        connect.append(&h);
        connect.append(&lbl("One-time setup (Spotify Premium):", "sp-sub"));
        let steps = lbl("1. developer.spotify.com/dashboard → Create app\n2. Redirect URI: http://127.0.0.1:8898/callback · tick Web API\n3. Paste the app's Client ID below → Connect", "sp-sub");
        steps.set_ellipsize(gtk::pango::EllipsizeMode::None);
        connect.append(&steps);
        let cid = gtk::Entry::builder().placeholder_text("Client ID").text(spotify::saved().0).build();
        connect.append(&cid);
        let go = gtk::Button::with_label("Connect Spotify");
        go.add_css_class("sp-green");
        connect.append(&go);
        let status = lbl("", "sp-sub");
        status.set_ellipsize(gtk::pango::EllipsizeMode::None);
        status.set_wrap(true);
        connect.append(&status);
        stack.add_named(&connect, Some("connect"));

        // ---------- sidebar ----------
        let body = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        body.set_margin_top(8);
        body.set_margin_start(8);
        body.set_margin_end(8);
        let side = gtk::Box::new(gtk::Orientation::Vertical, 8);
        side.set_size_request(320, -1);
        let nav = gtk::Box::new(gtk::Orientation::Vertical, 4);
        nav.add_css_class("sp-card");
        let home_btn = gtk::Button::new();
        home_btn.set_child(Some(&{
            let b = gtk::Box::new(gtk::Orientation::Horizontal, 14);
            b.append(&gtk::Image::from_icon_name("go-home-symbolic"));
            b.append(&lbl("Home", "sp-nav"));
            b
        }));
        home_btn.add_css_class("sp-navbtn");
        nav.append(&home_btn);
        let search = gtk::SearchEntry::builder().placeholder_text("What do you want to play?").build();
        search.add_css_class("sp-search");
        nav.append(&search);
        side.append(&nav);
        let library = gtk::Box::new(gtk::Orientation::Vertical, 6);
        library.add_css_class("sp-card");
        library.set_vexpand(true);
        let lib_head = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        lib_head.append(&gtk::Image::from_icon_name("view-list-symbolic"));
        lib_head.append(&lbl("Your Library", "sp-nav"));
        library.append(&lib_head);
        let lib = gtk::ListBox::new();
        lib.add_css_class("sp-lib");
        lib.set_activate_on_single_click(true);
        library.append(&gtk::ScrolledWindow::builder().child(&lib).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build());
        side.append(&library);
        body.append(&side);

        // ---------- main area ----------
        let main_card = gtk::Box::new(gtk::Orientation::Vertical, 0);
        main_card.add_css_class("sp-card");
        main_card.add_css_class("sp-main");
        main_card.set_hexpand(true);
        let main = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).build();
        // home: playlist grid
        let home = gtk::Box::new(gtk::Orientation::Vertical, 12);
        home.set_margin_top(20);
        home.set_margin_start(20);
        home.set_margin_end(20);
        home.append(&lbl("Your playlists", "sp-h2"));
        let home_grid = gtk::FlowBox::builder().selection_mode(gtk::SelectionMode::None).min_children_per_line(3).max_children_per_line(10).column_spacing(12).row_spacing(12).homogeneous(true).build();
        home_grid.set_activate_on_single_click(true);
        home.append(&home_grid);
        main.add_named(&gtk::ScrolledWindow::builder().child(&home).vexpand(true).build(), Some("home"));
        // list view: header + actions + table
        let lv = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let head_box = gtk::Box::new(gtk::Orientation::Horizontal, 24);
        head_box.add_css_class("sp-head");
        let (head_frame, head_pic) = cover(200, false);
        head_box.append(&head_frame);
        let hv = gtk::Box::new(gtk::Orientation::Vertical, 6);
        hv.set_valign(gtk::Align::End);
        let head_kind = lbl("PLAYLIST", "sp-kind");
        let head_title = lbl("", "sp-h0");
        let head_meta = lbl("", "sp-sub");
        hv.append(&head_kind);
        hv.append(&head_title);
        hv.append(&head_meta);
        head_box.append(&hv);
        lv.append(&head_box);
        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 16);
        actions.set_margin_start(24);
        actions.set_margin_top(16);
        actions.set_margin_bottom(8);
        let big_play = gtk::Button::from_icon_name("media-playback-start-symbolic");
        big_play.add_css_class("sp-bigplay");
        big_play.set_tooltip_text(Some("Play"));
        actions.append(&big_play);
        lv.append(&actions);
        let cols = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        cols.add_css_class("sp-cols");
        let c_num = lbl("#", "sp-colh");
        c_num.set_width_chars(3);
        c_num.set_xalign(1.0);
        let c_title = lbl("Title", "sp-colh");
        c_title.set_hexpand(true);
        let c_album = lbl("Album", "sp-colh");
        c_album.set_width_chars(34);
        let c_time = gtk::Image::from_icon_name("alarm-symbolic");
        c_time.set_margin_end(18);
        cols.append(&c_num);
        cols.append(&c_title);
        cols.append(&c_album);
        cols.append(&c_time);
        lv.append(&cols);
        let list = gtk::ListBox::new();
        list.add_css_class("sp-tracks");
        list.set_activate_on_single_click(false);
        lv.append(&list);
        main.add_named(&gtk::ScrolledWindow::builder().child(&lv).vexpand(true).build(), Some("list"));
        main_card.append(&main);
        body.append(&main_card);
        stack.add_named(&body, Some("main"));
        root.append(&stack);

        // ---------- now playing bar ----------
        let bar = gtk::CenterBox::new();
        bar.add_css_class("sp-bar");
        let left = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        left.set_size_request(360, -1);
        let (now_frame, now_pic) = cover(56, false);
        left.append(&now_frame);
        let meta = gtk::Box::new(gtk::Orientation::Vertical, 2);
        meta.set_valign(gtk::Align::Center);
        let now_title = lbl("Nothing playing", "sp-now");
        let now_sub = lbl("", "sp-sub");
        meta.append(&now_title);
        meta.append(&now_sub);
        left.append(&meta);
        bar.set_start_widget(Some(&left));
        let mid = gtk::Box::new(gtk::Orientation::Vertical, 4);
        mid.set_size_request(620, -1);
        let ctl = gtk::Box::new(gtk::Orientation::Horizontal, 14);
        ctl.set_halign(gtk::Align::Center);
        let shuffle = gtk::ToggleButton::new();
        shuffle.set_icon_name("media-playlist-shuffle-symbolic");
        shuffle.set_tooltip_text(Some("Shuffle"));
        shuffle.add_css_class("sp-icon");
        let prev = icon_btn("media-skip-backward-symbolic", "Previous");
        let play_btn = gtk::Button::from_icon_name("media-playback-start-symbolic");
        play_btn.add_css_class("sp-play");
        let next = icon_btn("media-skip-forward-symbolic", "Next");
        let repeat = icon_btn("media-playlist-repeat-symbolic", "Repeat: off");
        ctl.append(&shuffle);
        ctl.append(&prev);
        ctl.append(&play_btn);
        ctl.append(&next);
        ctl.append(&repeat);
        mid.append(&ctl);
        let prow = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let t_pos = lbl("0:00", "sp-time");
        t_pos.set_xalign(1.0);
        t_pos.set_width_chars(5);
        let t_len = lbl("0:00", "sp-time");
        t_len.set_width_chars(5);
        let progress = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 1000.0);
        progress.set_hexpand(true);
        progress.set_draw_value(false);
        progress.add_css_class("sp-scale");
        prow.append(&t_pos);
        prow.append(&progress);
        prow.append(&t_len);
        mid.append(&prow);
        bar.set_center_widget(Some(&mid));
        let right = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        right.set_halign(gtk::Align::End);
        right.set_valign(gtk::Align::Center);
        let device_btn = gtk::Button::new();
        let device_lbl = lbl("", "sp-device");
        device_btn.set_child(Some(&{
            let b = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            b.append(&gtk::Image::from_icon_name("audio-speakers-symbolic"));
            b.append(&device_lbl);
            b
        }));
        device_btn.add_css_class("sp-devbtn");
        right.append(&device_btn);
        right.append(&gtk::Image::from_icon_name("audio-volume-high-symbolic"));
        let volume = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 100.0, 1.0);
        volume.set_size_request(120, -1);
        volume.set_draw_value(false);
        volume.add_css_class("sp-scale");
        right.append(&volume);
        bar.set_end_widget(Some(&right));
        root.append(&bar);

        let p = Rc::new(Player {
            root,
            api: RefCell::new(None),
            user: RefCell::new(String::new()),
            tx,
            stack,
            status,
            textures: RefCell::new(HashMap::new()),
            waiting: RefCell::new(HashMap::new()),
            img_q: Arc::new(Mutex::new(vec![])),
            lib,
            playlists: RefCell::new(vec![]),
            main,
            home_grid,
            head_pic,
            head_kind,
            head_title,
            head_meta,
            head_box,
            list,
            items: RefCell::new((View { kind: "PLAYLIST", title: String::new(), img: String::new(), ctx: None }, vec![])),
            rows: RefCell::new(vec![]),
            now_pic,
            now_url: RefCell::new(String::new()),
            now_title,
            now_sub,
            now_uri: RefCell::new(String::new()),
            play_btn,
            shuffle,
            repeat,
            repeat_state: RefCell::new("off".into()),
            progress,
            t_pos,
            t_len,
            volume,
            device_btn,
            device_lbl,
            mixer_id: RefCell::new(None),
            updating: Cell::new(false),
            playing: Cell::new(false),
            pos_ms: Cell::new(0.0),
            dur_ms: Cell::new(0.0),
            hold_until: Cell::new(0),
        });
        p.start_image_workers();

        // ---------- wiring ----------
        {
            let (p2, cid2) = (Rc::downgrade(&p), cid.clone());
            go.connect_clicked(move |_| {
                let Some(p) = p2.upgrade() else { return };
                let id = cid2.text().trim().to_string();
                if id.len() < 16 {
                    p.status.set_label("Paste the Client ID from your Spotify developer app first.");
                    return;
                }
                p.status.set_label("Opening Spotify login in your browser…");
                let tx = p.tx.clone();
                std::thread::spawn(move || match Api::login(&id) {
                    Ok(api) => {
                        let user = api.get("/me").ok().and_then(|v| v["id"].as_str().map(String::from)).unwrap_or_default();
                        let _ = tx.send(Msg::Connected(Arc::new(api), user));
                    }
                    Err(e) => {
                        let _ = tx.send(Msg::Status(format!("Login failed: {e}")));
                    }
                });
            });
        }
        {
            let p2 = Rc::downgrade(&p);
            home_btn.connect_clicked(move |_| {
                if let Some(p) = p2.upgrade() {
                    p.main.set_visible_child_name("home");
                }
            });
        }
        p.act(&prev, |a| a.post("/me/player/previous").map(|_| ()));
        p.act(&next, |a| a.post("/me/player/next").map(|_| ()));
        {
            let p2 = Rc::downgrade(&p);
            p.play_btn.connect_clicked(move |_| {
                let Some(p) = p2.upgrade() else { return };
                let playing = p.playing.get();
                p.set_playing(!playing);
                p.hold_until.set(glib::monotonic_time() + 1_500_000);
                p.call(move |a| if playing { a.put("/me/player/pause", None).map(|_| ()) } else { a.put("/me/player/play", None).map(|_| ()) });
            });
        }
        {
            let p2 = Rc::downgrade(&p);
            big_play.connect_clicked(move |_| {
                if let Some(p) = p2.upgrade() {
                    p.play_index(0);
                }
            });
        }
        {
            let p2 = Rc::downgrade(&p);
            p.shuffle.connect_toggled(move |t| {
                let Some(p) = p2.upgrade() else { return };
                if p.updating.get() {
                    return;
                }
                let on = t.is_active();
                p.hold_until.set(glib::monotonic_time() + 1_500_000);
                p.call(move |a| a.put(&format!("/me/player/shuffle?state={on}"), None).map(|_| ()));
            });
        }
        {
            let p2 = Rc::downgrade(&p);
            p.repeat.connect_clicked(move |_| {
                let Some(p) = p2.upgrade() else { return };
                let nxt = match p.repeat_state.borrow().as_str() { "off" => "context", "context" => "track", _ => "off" }.to_string();
                p.set_repeat(&nxt);
                p.hold_until.set(glib::monotonic_time() + 1_500_000);
                p.call(move |a| a.put(&format!("/me/player/repeat?state={nxt}"), None).map(|_| ()));
            });
        }
        {
            let p2 = Rc::downgrade(&p);
            p.progress.connect_change_value(move |_, _, v| {
                if let Some(p) = p2.upgrade() {
                    let ms = v.max(0.0) as u64;
                    p.pos_ms.set(ms as f64);
                    p.hold_until.set(glib::monotonic_time() + 1_500_000);
                    p.call(move |a| a.put(&format!("/me/player/seek?position_ms={ms}"), None).map(|_| ()));
                }
                glib::Propagation::Proceed
            });
        }
        {
            let p2 = Rc::downgrade(&p);
            p.volume.connect_change_value(move |_, _, v| {
                if let Some(p) = p2.upgrade() {
                    let vol = v.clamp(0.0, 100.0) as u32;
                    p.hold_until.set(glib::monotonic_time() + 1_500_000);
                    p.call(move |a| a.put(&format!("/me/player/volume?volume_percent={vol}"), None).map(|_| ()));
                }
                glib::Propagation::Proceed
            });
        }
        {
            let p2 = Rc::downgrade(&p);
            p.device_btn.connect_clicked(move |_| {
                let Some(p) = p2.upgrade() else { return };
                p.call(|a| {
                    let id = a.mixer_device().ok_or("The Mixer device isn't online (is wavelink-spotify running?)")?;
                    a.put("/me/player", Some(json!({ "device_ids": [id], "play": true }))).map(|_| ())
                });
            });
        }
        {
            let p2 = Rc::downgrade(&p);
            search.connect_activate(move |e| {
                let Some(p) = p2.upgrade() else { return };
                let term = e.text().to_string();
                if term.trim().is_empty() {
                    return;
                }
                let view = View { kind: "SEARCH", title: term.clone(), img: String::new(), ctx: None };
                p.load(view, move |a| {
                    let v = a.get(&format!("/search?type=track&limit=50&q={}", spotify::q(&term)))?;
                    Ok(v["tracks"]["items"].as_array().cloned().unwrap_or_default().iter().filter_map(track_item).collect())
                });
            });
        }
        {
            let p2 = Rc::downgrade(&p);
            p.lib.connect_row_activated(move |_, row| {
                let Some(p) = p2.upgrade() else { return };
                match row.index() {
                    0 => p.open_liked(),
                    1 => p.open_queue(),
                    k => {
                        let pl = p.playlists.borrow().get(k as usize - 2).cloned();
                        if let Some(pl) = pl {
                            p.open_playlist(pl);
                        }
                    }
                }
            });
        }
        {
            let p2 = Rc::downgrade(&p);
            p.home_grid.connect_child_activated(move |_, child| {
                let Some(p) = p2.upgrade() else { return };
                let pl = p.playlists.borrow().get(child.index() as usize).cloned();
                if let Some(pl) = pl {
                    p.open_playlist(pl);
                }
            });
        }
        {
            let p2 = Rc::downgrade(&p);
            p.list.connect_row_activated(move |_, row| {
                if let Some(p) = p2.upgrade() {
                    p.play_index(row.index() as usize);
                }
            });
        }

        // ---------- message pump + local progress ----------
        let rx = Rc::new(rx);
        {
            let (p2, rx) = (Rc::downgrade(&p), rx.clone());
            glib::timeout_add_local(Duration::from_millis(100), move || {
                let Some(p) = p2.upgrade() else { return glib::ControlFlow::Break };
                p.pump(&rx);
                if p.playing.get() && glib::monotonic_time() > p.hold_until.get() {
                    p.pos_ms.set((p.pos_ms.get() + 100.0).min(p.dur_ms.get()));
                }
                p.show_pos();
                glib::ControlFlow::Continue
            });
        }
        {
            let tx = p.tx.clone();
            std::thread::spawn(move || {
                if let Some(api) = Api::resume() {
                    let user = api.get("/me").ok().and_then(|v| v["id"].as_str().map(String::from)).unwrap_or_default();
                    let _ = tx.send(Msg::Connected(Arc::new(api), user));
                }
            });
        }
        p.stack.set_visible_child_name("connect");
        // test-only: fake library for layout screenshots (no network, no login)
        if let Ok(which) = std::env::var("MIXER_SP_DEMO") {
            p.stack.set_visible_child_name("main");
            let pls: Vec<Playlist> = ["Truly Cpop V2", "Truly Studying V2", "Truly Sleep", "Truly EDM", "Truly Da lounge", "Truly Kpop", "Truly Hiphop", "Group Blend"]
                .iter().enumerate().map(|(k, n)| Playlist { name: n.to_string(), id: format!("{k}"), uri: String::new(), img: String::new(), owner: "demo".into(), count: 40 + k as u64 * 7 }).collect();
            p.fill_library(pls);
            if which == "list" {
                let items: Vec<Item> = (0..14).map(|k| Item { title: format!("Song number {}", k + 1), artists: "ROSÉ, Bruno Mars".into(), album: "rosie".into(), uri: format!("u{k}"), dur_ms: 170_000 + k * 9_000, img: String::new() }).collect();
                *p.now_uri.borrow_mut() = "u2".into();
                p.fill_list(View { kind: "PLAYLIST", title: "Truly Kpop".into(), img: String::new(), ctx: None }, items);
                p.main.set_visible_child_name("list");
            }
            p.now_title.set_label("Gone");
            p.now_sub.set_label("ROSÉ");
            p.device_lbl.set_label("Mixer");
            p.device_btn.add_css_class("sp-on");
            p.dur_ms.set(207_000.0);
            p.pos_ms.set(62_000.0);
            p.set_playing(true);
        }
        p
    }
}
