//! Player page: view updates (split out of player.rs, #64).
use super::*;

impl Player {
    // ---------- view updates ----------

    pub(super) fn show_header(&self, view: &View, count: Option<(usize, u64)>) {
        self.head_box.remove_css_class("sp-head-liked");
        self.head_kind.set_label(match view.kind {
            "LIKED" => "PLAYLIST",
            "SEARCH" => "SEARCH",
            "QUEUE" => "UP NEXT",
            _ => "PLAYLIST",
        });
        self.head_title.set_label(if view.kind == "SEARCH" { &view.title } else { &view.title });
        if view.kind == "LIKED" {
            self.head_box.add_css_class("sp-head-liked");
        }
        self.set_image(&self.head_pic, &view.img);
        self.head_meta.set_label(&match count {
            Some((n, ms)) => {
                let m = ms / 60000;
                let len = if m >= 60 { format!("{} hr {} min", m / 60, m % 60) } else { format!("{m} min") };
                let user = self.user.borrow().clone();
                if user.is_empty() { format!("{n} songs, {len}") } else { format!("{user} · {n} songs, {len}") }
            }
            None => "Loading…".into(),
        });
    }

    pub(super) fn fill_list(&self, view: View, items: Vec<Item>) {
        while let Some(c) = self.list.first_child() {
            self.list.remove(&c);
        }
        let total: u64 = items.iter().map(|i| i.dur_ms).sum();
        self.show_header(&view, Some((items.len(), total)));
        let mut rows = vec![];
        let now = self.now_uri.borrow().clone();
        for (n, it) in items.iter().enumerate() {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
            row.add_css_class("sp-row");
            let num = lbl(&format!("{}", n + 1), "sp-num");
            num.set_width_chars(3);
            num.set_xalign(1.0);
            row.append(&num);
            let (f, pic) = cover(40, false);
            self.set_image(&pic, &it.img);
            row.append(&f);
            let tv = gtk::Box::new(gtk::Orientation::Vertical, 2);
            tv.set_hexpand(true);
            tv.set_valign(gtk::Align::Center);
            let t = lbl(&it.title, "sp-track");
            tv.append(&t);
            tv.append(&lbl(&it.artists, "sp-sub"));
            row.append(&tv);
            let al = lbl(&it.album, "sp-sub");
            al.set_width_chars(34);
            al.set_max_width_chars(34);
            row.append(&al);
            let d = lbl(&fmt_ms(it.dur_ms as f64), "sp-sub");
            d.set_width_chars(5);
            d.set_xalign(1.0);
            row.append(&d);
            if it.uri == now {
                t.add_css_class("sp-playing");
                num.add_css_class("sp-playing");
            }
            rows.push((it.uri.clone(), num, t));
            self.list.append(&row);
        }
        *self.rows.borrow_mut() = rows;
        *self.items.borrow_mut() = (view, items);
    }

    pub(super) fn fill_library(&self, pls: Vec<Playlist>) {
        while let Some(c) = self.lib.first_child() {
            self.lib.remove(&c);
        }
        let entry = |title: &str, sub: &str, special: Option<&str>| -> (gtk::Box, gtk::Picture) {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
            row.add_css_class("sp-librow");
            let (f, pic) = cover(48, false);
            if let Some(cls) = special {
                f.add_css_class(cls);
                let ic = gtk::Image::from_icon_name(if cls == "sp-liked-tile" { "emblem-favorite-symbolic" } else { "view-list-symbolic" });
                ic.set_vexpand(true);
                ic.set_valign(gtk::Align::Center);
                f.remove(&pic);
                f.append(&ic);
            }
            row.append(&f);
            let v = gtk::Box::new(gtk::Orientation::Vertical, 2);
            v.set_valign(gtk::Align::Center);
            v.append(&lbl(title, "sp-libtitle"));
            v.append(&lbl(sub, "sp-sub"));
            row.append(&v);
            (row, pic)
        };
        let (liked, _) = entry("Liked Songs", "Playlist", Some("sp-liked-tile"));
        self.lib.append(&liked);
        let (queue, _) = entry("Queue", "Up next", Some("sp-queue-tile"));
        self.lib.append(&queue);
        while let Some(c) = self.home_grid.first_child() {
            self.home_grid.remove(&c);
        }
        for pl in &pls {
            let (row, pic) = entry(&pl.name, &format!("Playlist · {}", pl.owner), None);
            self.set_image(&pic, &pl.img);
            self.lib.append(&row);
            // home card
            let card = gtk::Box::new(gtk::Orientation::Vertical, 8);
            card.add_css_class("sp-tile");
            let (f, p2) = cover(160, false);
            self.set_image(&p2, &pl.img);
            card.append(&f);
            let t = lbl(&pl.name, "sp-libtitle");
            t.set_max_width_chars(18);
            card.append(&t);
            card.append(&lbl(&format!("{} songs", pl.count), "sp-sub"));
            self.home_grid.insert(&card, -1);
        }
        *self.playlists.borrow_mut() = pls;
    }

    pub(super) fn set_playing(&self, on: bool) {
        self.playing.set(on);
        self.play_btn.set_icon_name(if on { "media-playback-pause-symbolic" } else { "media-playback-start-symbolic" });
    }

    pub(super) fn set_repeat(&self, s: &str) {
        *self.repeat_state.borrow_mut() = s.to_string();
        self.repeat.set_icon_name(if s == "track" { "media-playlist-repeat-song-symbolic" } else { "media-playlist-repeat-symbolic" });
        self.repeat.set_tooltip_text(Some(&format!("Repeat: {s}")));
        if s == "off" { self.repeat.remove_css_class("sp-on") } else { self.repeat.add_css_class("sp-on") }
    }

    pub(super) fn show_pos(&self) {
        self.updating.set(true);
        self.progress.set_range(0.0, self.dur_ms.get().max(1.0));
        self.progress.set_value(self.pos_ms.get());
        self.updating.set(false);
        self.t_pos.set_label(&fmt_ms(self.pos_ms.get()));
        self.t_len.set_label(&fmt_ms(self.dur_ms.get()));
    }

    pub(super) fn pump(self: &Rc<Self>, rx: &Receiver<Msg>) {
        while let Ok(m) = rx.try_recv() {
            match m {
                Msg::Connected(api, user) => {
                    *self.api.borrow_mut() = Some(api.clone());
                    *self.user.borrow_mut() = user;
                    self.stack.set_visible_child_name("main");
                    self.main.set_visible_child_name("home");
                    let tx = self.tx.clone();
                    let a2 = api.clone();
                    std::thread::spawn(move || {
                        let mut n = 0u32;
                        loop {
                            match a2.get("/me/player") {
                                Ok(v) => {
                                    if tx.send(Msg::State(v)).is_err() {
                                        break;
                                    }
                                }
                                Err(e) => {
                                    let _ = tx.send(Msg::Status(e));
                                }
                            }
                            if n % 5 == 0 {
                                let _ = tx.send(Msg::Device(a2.mixer_device()));
                            }
                            n += 1;
                            std::thread::sleep(Duration::from_secs(1));
                        }
                    });
                    let tx = self.tx.clone();
                    std::thread::spawn(move || {
                        let mut out = vec![];
                        let mut url = "/me/playlists?limit=50".to_string();
                        while out.len() < 300 {
                            let Ok(v) = api.get(&url) else { break };
                            for pl in v["items"].as_array().cloned().unwrap_or_default() {
                                if let (Some(n), Some(id), Some(uri)) = (pl["name"].as_str(), pl["id"].as_str(), pl["uri"].as_str()) {
                                    out.push(Playlist {
                                        name: n.to_string(),
                                        id: id.to_string(),
                                        uri: uri.to_string(),
                                        img: pick_img(&pl["images"], 120),
                                        owner: pl["owner"]["display_name"].as_str().unwrap_or("").to_string(),
                                        count: pl["tracks"]["total"].as_u64().unwrap_or(0),
                                    });
                                }
                            }
                            match v["next"].as_str() {
                                Some(n) => url = n.to_string(),
                                None => break,
                            }
                        }
                        let _ = tx.send(Msg::Playlists(out));
                    });
                }
                Msg::Status(s) => {
                    self.status.set_label(&s);
                    self.device_btn.set_tooltip_text(Some(&s));
                }
                Msg::Device(id) => *self.mixer_id.borrow_mut() = id,
                Msg::Playlists(pls) => self.fill_library(pls),
                Msg::List(view, items) => self.fill_list(view, items),
                Msg::State(v) => self.apply_state(&v),
                Msg::Image(url, bytes) => {
                    if let Ok(t) = gdk::Texture::from_bytes(&glib::Bytes::from(&bytes)) {
                        if let Some(ws) = self.waiting.borrow_mut().remove(&url) {
                            for w in ws {
                                if let Some(pic) = w.upgrade() {
                                    pic.set_paintable(Some(&t));
                                }
                            }
                        }
                        self.textures.borrow_mut().insert(url, t);
                    }
                }
            }
        }
    }

    pub(super) fn apply_state(&self, v: &Value) {
        let dev = v["device"]["name"].as_str().unwrap_or("").to_string();
        let on_mixer = dev == spotify::DEVICE;
        self.device_lbl.set_label(if dev.is_empty() { "Play on Mixer" } else if on_mixer { "Mixer" } else { &dev });
        if on_mixer { self.device_btn.add_css_class("sp-on") } else { self.device_btn.remove_css_class("sp-on") }
        self.device_btn.set_tooltip_text(Some(if on_mixer { "Playing on the Mixer (through the Spotify channel)" } else { "Click to move playback to the Mixer" }));
        if v.is_null() {
            self.now_title.set_label("Nothing playing");
            self.now_sub.set_label("Double-click a song to play it on the Mixer");
            self.set_playing(false);
            return;
        }
        let item = &v["item"];
        let artists: Vec<&str> = item["artists"].as_array().map(|a| a.iter().filter_map(|x| x["name"].as_str()).collect()).unwrap_or_default();
        self.now_title.set_label(item["name"].as_str().unwrap_or("—"));
        self.now_sub.set_label(&artists.join(", "));
        let held = glib::monotonic_time() < self.hold_until.get();
        if !held {
            self.set_playing(v["is_playing"].as_bool().unwrap_or(false));
            self.pos_ms.set(v["progress_ms"].as_f64().unwrap_or(0.0));
            self.updating.set(true);
            if let Some(vol) = v["device"]["volume_percent"].as_f64() {
                self.volume.set_value(vol);
            }
            self.shuffle.set_active(v["shuffle_state"].as_bool().unwrap_or(false));
            self.updating.set(false);
            self.set_repeat(v["repeat_state"].as_str().unwrap_or("off"));
        }
        self.dur_ms.set(item["duration_ms"].as_f64().unwrap_or(0.0));
        // highlight the playing row
        let uri = item["uri"].as_str().unwrap_or("").to_string();
        if *self.now_uri.borrow() != uri {
            for (u, num, t) in self.rows.borrow().iter() {
                if *u == uri {
                    num.add_css_class("sp-playing");
                    t.add_css_class("sp-playing");
                } else {
                    num.remove_css_class("sp-playing");
                    t.remove_css_class("sp-playing");
                }
            }
            *self.now_uri.borrow_mut() = uri;
        }
        let url = pick_img(&item["album"]["images"], 64);
        if *self.now_url.borrow() != url {
            *self.now_url.borrow_mut() = url.clone();
            self.set_image(&self.now_pic, &url);
        }
    }
}
