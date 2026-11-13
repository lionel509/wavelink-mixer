//! Player page: images (split out of player.rs, #64).
use super::*;

impl Player {
    // ---------- images ----------

    /// Four background fetchers pull URLs off a shared queue (newest first).
    pub(super) fn start_image_workers(&self) {
        for _ in 0..4 {
            let (q, tx) = (self.img_q.clone(), self.tx.clone());
            std::thread::spawn(move || loop {
                let next = q.lock().unwrap().pop();
                match next {
                    Some(url) => {
                        if let Some(b) = Api::image(&url) {
                            if tx.send(Msg::Image(url, b)).is_err() {
                                break;
                            }
                        }
                    }
                    None => std::thread::sleep(Duration::from_millis(60)),
                }
            });
        }
    }

    pub(super) fn set_image(&self, pic: &gtk::Picture, url: &str) {
        if url.is_empty() {
            pic.set_paintable(None::<&gdk::Paintable>);
            return;
        }
        if let Some(t) = self.textures.borrow().get(url) {
            pic.set_paintable(Some(t));
            return;
        }
        pic.set_paintable(None::<&gdk::Paintable>);
        let mut w = self.waiting.borrow_mut();
        let first = !w.contains_key(url);
        w.entry(url.to_string()).or_default().push(pic.downgrade());
        if first {
            self.img_q.lock().unwrap().push(url.to_string());
        }
    }
}
