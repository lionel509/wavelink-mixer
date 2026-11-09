//! Note scope (#23): one "super graph" for the Spectrum page. A scrolling spectrogram of the analysed
//! bus (time runs left to right, frequency bottom to top on a log axis), a piano keyboard on the left
//! edge, the live MIDI notes drawn on top as lines in their part colour, the Bass / Mid / High zones
//! marked, and the current spectrum as bars at the right edge.
//! Fed once per frame from the window tick: `push_bands` (the bus tap's 72 log bands, the same buffer
//! the bar analyser reads) and `set_notes`. No extra PipeWire stream, no extra FFT.
use crate::dash::BANDS;
use gtk::cairo;
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// The band axis the bus tap uses (dash.rs): BANDS log bands from F_LO to F_HI.
const F_LO: f64 = 25.0;
const F_HI: f64 = 18000.0;
/// Columns of history (one per frame at the 30 Hz meter rate: 8 s).
const COLS: usize = 240;
/// Seconds the scope spans, matching COLS at 30 Hz.
pub const SPAN_S: f64 = 8.0;
const KEYS_W: f64 = 30.0;
const NOW_W: f64 = 46.0;

/// One note to draw: seconds ago it started and ended (end 0 = still sounding), MIDI pitch,
/// colour, velocity 0..127.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Mark {
    pub ago_start: f64,
    pub ago_end: f64,
    pub pitch: u8,
    pub rgb: (f64, f64, f64),
    pub vel: u8,
}

pub struct NoteScope {
    pub area: gtk::DrawingArea,
    /// COLS x BANDS spectrogram, written one column per frame into a ring
    img: RefCell<cairo::ImageSurface>,
    head: Cell<usize>,
    now: RefCell<Vec<f32>>,
    notes: RefCell<Vec<Mark>>,
    target: RefCell<String>,
    /// (zone start Hz, name, colour) ascending
    zones: RefCell<Vec<(f64, String, (f64, f64, f64))>>,
}

fn hz(p: f64) -> f64 {
    440.0 * 2f64.powf((p - 69.0) / 12.0)
}

/// 0 at F_LO, 1 at F_HI (log).
fn fpos(f: f64) -> f64 {
    ((f / F_LO).ln() / (F_HI / F_LO).ln()).clamp(0.0, 1.0)
}

fn is_black(p: u8) -> bool {
    matches!(p % 12, 1 | 3 | 6 | 8 | 10)
}

impl NoteScope {
    pub fn new(target: &str) -> Rc<NoteScope> {
        let area = gtk::DrawingArea::new();
        area.set_content_height(300);
        area.set_hexpand(true);
        area.update_property(&[gtk::accessible::Property::Label("Spectrogram with piano notes, Bass, Mid and High zones")]);
        let img = cairo::ImageSurface::create(cairo::Format::ARgb32, COLS as i32, BANDS as i32).expect("scope surface");
        let s = Rc::new(NoteScope {
            area: area.clone(),
            img: RefCell::new(img),
            head: Cell::new(0),
            now: RefCell::new(vec![0.0; BANDS]),
            notes: RefCell::new(Vec::new()),
            target: RefCell::new(target.to_string()),
            zones: RefCell::new(Vec::new()),
        });
        let w = Rc::downgrade(&s);
        area.set_draw_func(move |_, cr, wd, ht| {
            if let Some(s) = w.upgrade() {
                s.draw(cr, wd as f64, ht as f64);
            }
        });
        s
    }

    pub fn set_target(&self, t: &str) {
        *self.target.borrow_mut() = t.to_string();
    }

    pub fn target(&self) -> String {
        self.target.borrow().clone()
    }

    pub fn set_zones(&self, z: Vec<(f64, String, (f64, f64, f64))>) {
        *self.zones.borrow_mut() = z;
    }

    fn zone_rgb(&self, f: f64) -> (f64, f64, f64) {
        self.zones.borrow().iter().rev().find(|z| f >= z.0).map(|z| z.2).unwrap_or((0.8, 0.8, 0.85))
    }

    /// One frame of the bus tap's bands (0..1 each). Writes the newest spectrogram column.
    pub fn push_bands(&self, bands: &[f32]) {
        let col = self.head.get();
        {
            let mut img = self.img.borrow_mut();
            img.flush();
            let stride = img.stride() as usize;
            let tints: Vec<(f64, f64, f64)> = (0..BANDS)
                .map(|i| self.zone_rgb(F_LO * (F_HI / F_LO).powf((i as f64 + 0.5) / BANDS as f64)))
                .collect();
            if let Ok(mut data) = img.data() {
                for i in 0..BANDS {
                    let v = bands.get(i).copied().unwrap_or(0.0).clamp(0.0, 1.0) as f64;
                    // a little gamma so quiet partials still show
                    let a = v.powf(0.7);
                    let (r, g, b) = tints[i];
                    // row 0 is the top of the image = the highest band
                    let row = BANDS - 1 - i;
                    let o = row * stride + col * 4;
                    // premultiplied BGRA
                    data[o] = (b * a * 255.0) as u8;
                    data[o + 1] = (g * a * 255.0) as u8;
                    data[o + 2] = (r * a * 255.0) as u8;
                    data[o + 3] = (a * 255.0) as u8;
                }
            }
            img.mark_dirty();
        }
        self.head.set((col + 1) % COLS);
        let mut now = self.now.borrow_mut();
        now.clear();
        now.extend((0..BANDS).map(|i| bands.get(i).copied().unwrap_or(0.0)));
    }

    pub fn set_notes(&self, n: Vec<Mark>) {
        *self.notes.borrow_mut() = n;
    }

    /// Repaint if on screen (call once per frame after push/set).
    pub fn redraw(&self) {
        if self.area.is_mapped() {
            self.area.queue_draw();
        }
    }

    fn draw(&self, cr: &cairo::Context, w: f64, h: f64) {
        cr.set_source_rgb(0.05, 0.05, 0.06);
        let _ = cr.paint();
        let (gx, gw) = (KEYS_W, (w - KEYS_W - NOW_W).max(10.0));
        let (gy, gh) = (4.0, (h - 18.0).max(10.0));
        let fy = |f: f64| gy + gh * (1.0 - fpos(f));
        let py = |p: f64| fy(hz(p));

        // spectrogram: the ring drawn oldest -> newest, stretched to the graph
        {
            let img = self.img.borrow();
            let head = self.head.get();
            let cw = gw / COLS as f64;
            for (src0, src1, dst0) in [(head, COLS, 0usize), (0, head, COLS - head)] {
                if src1 <= src0 {
                    continue;
                }
                cr.save().ok();
                cr.rectangle(gx + dst0 as f64 * cw, gy, (src1 - src0) as f64 * cw, gh);
                cr.clip();
                cr.translate(gx + (dst0 as f64 - src0 as f64) * cw, gy);
                cr.scale(cw, gh / BANDS as f64);
                let _ = cr.set_source_surface(&*img, 0.0, 0.0);
                cr.source().set_filter(cairo::Filter::Good);
                let _ = cr.paint();
                cr.restore().ok();
            }
        }

        // octave lines on every C, and zone dividers with names
        cr.set_line_width(1.0);
        for p in (24u8..=108).step_by(12) {
            let y = py(p as f64 - 0.5).round() + 0.5;
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.07);
            cr.move_to(gx, y);
            cr.line_to(gx + gw, y);
            let _ = cr.stroke();
        }
        cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
        cr.set_font_size(10.0);
        let zones = self.zones.borrow().clone();
        for (k, z) in zones.iter().enumerate() {
            let top = zones.get(k + 1).map(|n| n.0).unwrap_or(F_HI);
            if z.0 > F_LO {
                let y = fy(z.0).round() + 0.5;
                cr.set_source_rgba(z.2 .0, z.2 .1, z.2 .2, 0.35);
                cr.set_dash(&[4.0, 4.0], 0.0);
                cr.move_to(gx, y);
                cr.line_to(gx + gw, y);
                let _ = cr.stroke();
                cr.set_dash(&[], 0.0);
            }
            cr.set_source_rgba(z.2 .0, z.2 .1, z.2 .2, 0.85);
            let yc = (fy(z.0.max(F_LO)) + fy(top)) / 2.0 + 4.0;
            cr.move_to(gx + gw - 40.0, yc);
            let _ = cr.show_text(&z.1);
        }

        // notes: a bar per note at its fundamental, bright edge + glow; sounding notes reach "now"
        let notes = self.notes.borrow();
        let tx = |ago: f64| gx + gw * (1.0 - ago / SPAN_S);
        for m in notes.iter() {
            if m.ago_start < m.ago_end || m.ago_end > SPAN_S {
                continue;
            }
            let x0 = tx(m.ago_start.min(SPAN_S));
            let x1 = tx(m.ago_end.max(0.0)).max(x0 + 2.0);
            let (yt, yb) = (py(m.pitch as f64 + 0.5), py(m.pitch as f64 - 0.5));
            let hh = (yb - yt).max(3.0);
            let yc = (yt + yb) / 2.0;
            let (r, g, b) = m.rgb;
            let a = 0.55 + 0.45 * m.vel as f64 / 127.0;
            cr.set_source_rgba(r, g, b, 0.25 * a);
            cr.rectangle(x0, yc - hh, x1 - x0, hh * 2.0);
            let _ = cr.fill();
            cr.set_source_rgba(r, g, b, a);
            cr.rectangle(x0, yc - hh / 2.0, x1 - x0, hh);
            let _ = cr.fill();
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.5 * a);
            cr.rectangle(x0, yc - hh / 2.0, 1.5, hh);
            let _ = cr.fill();
        }

        // keyboard on the left, keys lit while their note sounds
        let lit: Vec<(u8, (f64, f64, f64))> = notes.iter().filter(|m| m.ago_end <= 0.0).map(|m| (m.pitch, m.rgb)).collect();
        for pass in [false, true] {
            for p in 21u8..=108 {
                if is_black(p) != pass {
                    continue;
                }
                let (yt, yb) = (py(p as f64 + 0.5), py(p as f64 - 0.5));
                let wk = if pass { KEYS_W * 0.6 } else { KEYS_W - 2.0 };
                match lit.iter().find(|l| l.0 == p) {
                    Some((_, (r, g, b))) => cr.set_source_rgb(*r, *g, *b),
                    None if pass => cr.set_source_rgb(0.12, 0.12, 0.13),
                    None => cr.set_source_rgb(0.82, 0.82, 0.84),
                }
                cr.rectangle(1.0, yt, wk, (yb - yt - 0.4).max(0.6));
                let _ = cr.fill();
            }
        }
        cr.set_font_size(8.0);
        cr.set_source_rgba(0.1, 0.1, 0.1, 0.9);
        for (p, s) in [(24u8, "C1"), (36, "C2"), (48, "C3"), (60, "C4"), (72, "C5"), (84, "C6"), (96, "C7"), (108, "C8")] {
            cr.move_to(KEYS_W - 15.0, py(p as f64) + 3.0);
            let _ = cr.show_text(s);
        }

        // now: the current spectrum as horizontal bars on the right
        let now = self.now.borrow();
        let nx = gx + gw + 4.0;
        let bh = gh / BANDS as f64;
        for i in 0..BANDS {
            let v = now.get(i).copied().unwrap_or(0.0).clamp(0.0, 1.0) as f64;
            let f = F_LO * (F_HI / F_LO).powf((i as f64 + 0.5) / BANDS as f64);
            let (r, g, b) = self.zone_rgb(f);
            cr.set_source_rgba(r, g, b, 0.9);
            let y = gy + gh - (i as f64 + 1.0) * bh;
            cr.rectangle(nx, y + 0.5, v * (NOW_W - 6.0), (bh - 1.0).max(0.8));
            let _ = cr.fill();
        }
        // playhead and time labels
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.6);
        cr.rectangle(gx + gw - 1.0, gy, 1.0, gh);
        let _ = cr.fill();
        cr.set_font_size(9.0);
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.35);
        for s in [0.0, 2.0, 4.0, 6.0, 8.0] {
            let x = tx(s);
            cr.move_to((x - 8.0).max(gx), gy + gh + 13.0);
            let _ = cr.show_text(&if s == 0.0 { "now".to_string() } else { format!("-{s:.0}s") });
        }
    }
}
