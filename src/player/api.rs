//! Player page: API helpers (split out of player.rs, #64).
use super::*;

impl Player {
    // ---------- API helpers ----------

    pub(super) fn call(&self, f: impl FnOnce(&Api) -> Result<(), String> + Send + 'static) {
        let Some(api) = self.api.borrow().clone() else { return };
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            if let Err(e) = f(&api) {
                let _ = tx.send(Msg::Status(e));
            }
        });
    }

    pub(super) fn act(self: &Rc<Self>, b: &gtk::Button, f: fn(&Api) -> Result<(), String>) {
        let p2 = Rc::downgrade(self);
        b.connect_clicked(move |_| {
            if let Some(p) = p2.upgrade() {
                p.call(f);
            }
        });
    }

    pub(super) fn load(&self, view: View, f: impl FnOnce(&Api) -> Result<Vec<Item>, String> + Send + 'static) {
        let Some(api) = self.api.borrow().clone() else { return };
        self.show_header(&view, None);
        while let Some(c) = self.list.first_child() {
            self.list.remove(&c);
        }
        self.main.set_visible_child_name("list");
        let tx = self.tx.clone();
        std::thread::spawn(move || match f(&api) {
            Ok(items) => {
                let _ = tx.send(Msg::List(view, items));
            }
            Err(e) => {
                let _ = tx.send(Msg::Status(e));
            }
        });
    }

    pub(super) fn open_playlist(&self, pl: Playlist) {
        let (id, uri) = (pl.id.clone(), pl.uri.clone());
        let view = View { kind: "PLAYLIST", title: pl.name.clone(), img: pl.img.clone(), ctx: Some(uri) };
        self.load(view, move |a| {
            let mut out = vec![];
            let mut url = format!("/playlists/{id}/tracks?limit=100");
            while out.len() < 500 {
                let v = a.get(&url)?;
                out.extend(v["items"].as_array().cloned().unwrap_or_default().iter().filter_map(|i| track_item(&i["track"])));
                match v["next"].as_str() {
                    Some(n) => url = n.to_string(),
                    None => break,
                }
            }
            Ok(out)
        });
    }

    pub(super) fn open_liked(&self) {
        let user = self.user.borrow().clone();
        let view = View { kind: "LIKED", title: "Liked Songs".into(), img: String::new(), ctx: (!user.is_empty()).then(|| format!("spotify:user:{user}:collection")) };
        self.load(view, |a| {
            let mut out = vec![];
            let mut url = "/me/tracks?limit=50".to_string();
            while out.len() < 500 {
                let v = a.get(&url)?;
                out.extend(v["items"].as_array().cloned().unwrap_or_default().iter().filter_map(|i| track_item(&i["track"])));
                match v["next"].as_str() {
                    Some(n) => url = n.to_string(),
                    None => break,
                }
            }
            Ok(out)
        });
    }

    pub(super) fn open_queue(&self) {
        let view = View { kind: "QUEUE", title: "Queue".into(), img: String::new(), ctx: None };
        self.load(view, |a| {
            let v = a.get("/me/player/queue")?;
            let mut out: Vec<Item> = v["currently_playing"].is_object().then(|| track_item(&v["currently_playing"])).flatten().into_iter().collect();
            out.extend(v["queue"].as_array().cloned().unwrap_or_default().iter().filter_map(track_item));
            Ok(out)
        });
    }

    /// Play a track from the current list on the Mixer device, keeping its context.
    pub(super) fn play_index(&self, k: usize) {
        let (view, items) = self.items.borrow().clone();
        let Some(item) = items.get(k).cloned() else { return };
        let uris: Vec<String> = items.iter().skip(k).take(100).map(|i| i.uri.clone()).collect();
        let dev = self.mixer_id.borrow().clone();
        let tx = self.tx.clone();
        self.call(move |a| {
            let dev = dev.or_else(|| a.mixer_device());
            let path = match &dev {
                Some(id) => format!("/me/player/play?device_id={id}"),
                None => {
                    let _ = tx.send(Msg::Status("Mixer device offline — playing on your active Spotify device".into()));
                    "/me/player/play".to_string()
                }
            };
            let body = match view.ctx {
                Some(c) => json!({ "context_uri": c, "offset": { "uri": item.uri } }),
                None => json!({ "uris": uris }),
            };
            a.put(&path, Some(body)).map(|_| ())
        });
    }
}
