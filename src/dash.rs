//! Vintage-dashboard instruments drawn with cairo: needle gauges, knurled knobs and the
//! RGB spectrum analyser. Chrome bezels, engine-turned ivory faces, red needles.
use gtk::prelude::*;
use gtk::{cairo, glib};
use std::cell::Cell;
use std::f64::consts::PI;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

// ---------- colour helpers ----------


fn hsv(h: f64, s: f64, v: f64) -> (f64, f64, f64) {
    let h = h.rem_euclid(360.0) / 60.0;
    let c = v * s;
    let x = c * (1.0 - (h % 2.0 - 1.0).abs());
    let (r, g, b) = match h as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    (r + m, g + m, b + m)
}

fn text_centered(cr: &cairo::Context, x: f64, y: f64, s: &str) {
    if let Ok(e) = cr.text_extents(s) {
        cr.move_to(x - e.width() / 2.0 - e.x_bearing(), y - e.height() / 2.0 - e.y_bearing());
        let _ = cr.show_text(s);
    }
}

/// Meters and needles animate at most this often (display refresh is 60-240 Hz).
pub const METER_FPS: f64 = 30.0;
/// Accumulates frame-clock dt; yields one frame's elapsed time per 1/METER_FPS. The overshoot
/// past a period is carried, so 75 Hz and 144 Hz frames still give 30 emits a second.
pub struct FrameCap { acc: Cell<f64> }
impl FrameCap {
    pub fn new() -> FrameCap { FrameCap { acc: Cell::new(0.0) } }
    pub fn tick(&self, dt: f64) -> Option<f64> {
        let period = 1.0 / METER_FPS;
        let a = self.acc.get() + dt;
        if a + 1e-4 < period { self.acc.set(a); return None; }
        // a stall (window hidden, debugger) counts once, clamped, with no carry
        if a > 0.1 { self.acc.set(0.0); return Some(0.1); }
        self.acc.set(a - period);
        Some(period)
    }
}
/// Redraw only if something moved and the widget is on screen.
pub fn redraw_if(area: &gtk::DrawingArea, moved: bool) { if moved && area.is_mapped() { area.queue_draw(); } }
/// Exponential ease toward `target`; `moved` = changed by more than `eps`.
pub fn ease_toward(cur: f64, target: f64, dt: f64, tau: f64, eps: f64) -> (f64, bool) {
    let n = cur + (target - cur) * (1.0 - (-dt / tau).exp());
    (n, (n - cur).abs() > eps)
}
/// One damped-spring step of the VU needle; `moved` = the needle travelled more than 0.01.
pub fn vu_spring(pos: f64, vel: f64, dbfs: f64, dt: f64) -> (f64, f64, bool) {
    // fed with RMS; 0 VU = -12 dBFS RMS (suits mastered music, which averages ~-14..-9)
    let target = (dbfs + 12.0).max(VU_MIN - 3.0);
    let (k, c) = (110.0, 19.0);
    let a = k * (target - pos) - c * vel;
    let v = vel + a * dt;
    let p = (pos + v * dt).clamp(VU_MIN - 3.0, VU_MAX + 1.0);
    (p, v, (p - pos).abs() > 0.01)
}
/// Analyser band smoothing (fast rise, 0.9/s fall). True if any band moved more than 1e-3.
pub fn smooth_bands(shown: &mut [f64], src: &[f32], dt: f64) -> bool {
    let mut moved = false;
    for (cur, &v) in shown.iter_mut().zip(src) {
        let v = v as f64;
        let n = if v > *cur { *cur + (v - *cur) * 0.5 } else { (*cur - 0.9 * dt).max(v) };
        if (n - *cur).abs() > 1e-3 { moved = true; }
        *cur = n;
    }
    moved
}
/// Spectrum bar and cap smoothing with hold. True if a bar or cap moved more than 1e-4
/// (hold-timer changes don't count).
pub fn smooth_spectrum(shown: &mut [f64], peaks: &mut [(f64, f64)], src: &[f32], dt: f64) -> bool {
    let mut moved = false;
    for i in 0..shown.len().min(peaks.len()).min(src.len()) {
        let v = src[i] as f64;
        let before = (shown[i], peaks[i].0);
        shown[i] = if v > shown[i] { shown[i] + (v - shown[i]) * 0.6 } else { (shown[i] - 1.1 * dt).max(v) };
        if shown[i] >= peaks[i].0 {
            peaks[i] = (shown[i], 1.2);
        } else if peaks[i].1 > 0.0 {
            peaks[i].1 -= dt;
        } else {
            peaks[i].0 = (peaks[i].0 - 0.35 * dt).max(shown[i]);
        }
        if (shown[i] - before.0).abs() > 1e-4 || (peaks[i].0 - before.1).abs() > 1e-4 { moved = true; }
    }
    moved
}

// ---------- fader taper (shared by faders and meters) ----------

/// Console-style taper: more travel around unity, compressed at the bottom.
const TAPER: &[(f64, f64)] = &[(-60.0, 0.0), (-40.0, 0.12), (-30.0, 0.24), (-20.0, 0.40), (-10.0, 0.60), (-5.0, 0.70), (0.0, 0.80), (6.0, 0.90), (12.0, 1.0)];

pub fn db_to_pos(db: f64) -> f64 {
    if db <= TAPER[0].0 {
        return 0.0;
    }
    for w in TAPER.windows(2) {
        let ((d0, p0), (d1, p1)) = (w[0], w[1]);
        if db <= d1 {
            return p0 + (p1 - p0) * (db - d0) / (d1 - d0);
        }
    }
    1.0
}
pub fn pos_to_db(p: f64) -> f64 {
    if p <= 0.0 {
        return TAPER[0].0;
    }
    for w in TAPER.windows(2) {
        let ((d0, p0), (d1, p1)) = (w[0], w[1]);
        if p <= p1 {
            return d0 + (d1 - d0) * (p - p0) / (p1 - p0);
        }
    }
    TAPER[TAPER.len() - 1].0
}

// ---------- stereo LED meter ----------

pub const AMBER_DB: f64 = -6.0; // Scarlett Solo 3rd Gen halo: amber at -6 dBFS
pub const RED_DB: f64 = 0.0; // red at 0 dBFS (digital clipping)
pub const HOLD_S: f64 = 3.0; // peak flag hold
pub const SILENT_DB: f64 = -90.0; // what Meter reports for silence
const RELEASE_DB_S: f64 = 24.0;
const FALL_DB_S: f64 = 12.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Zone { Green, Amber, Red }
pub fn zone(db: f64) -> Zone {
    if db >= RED_DB { Zone::Red } else if db >= AMBER_DB { Zone::Amber } else { Zone::Green }
}
fn zone_rgb(z: Zone) -> (f64, f64, f64) {
    match z { Zone::Green => (0.3, 0.85, 0.42), Zone::Amber => (0.98, 0.72, 0.2), Zone::Red => (0.95, 0.22, 0.16) }
}
const FLAG_RGB: (f64, f64, f64) = (1.0, 0.2, 0.15);
/// Level an input sends to a bus: post-strip peak + send gain (wpctl cubic volume => level_db is amplitude dB).
pub fn send_db(post_db: f64, on: bool, level_db: f64) -> f64 {
    if !on || level_db <= crate::config::MIN_DB { SILENT_DB } else { post_db + level_db }
}

const M_MIN: f64 = -60.0;
const M_MAX: f64 = 6.0;
const SEG: usize = 40;

fn m_pos(db: f64) -> f64 {
    // meters read dBFS on the same taper shape as the faders, so 0 lines up visually
    ((db.clamp(M_MIN, M_MAX) - M_MIN) / (M_MAX - M_MIN)).powf(1.6)
}

/// Pure peak-hold state for one channel: instant attack, 24 dB/s release, a peak that holds
/// for HOLD_S and then falls at 12 dB/s.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ballistics { pub level: f64, pub peak: f64, pub hold: f64 }
impl Ballistics {
    pub const fn new() -> Self { Ballistics { level: M_MIN, peak: M_MIN, hold: 0.0 } }
    /// One frame: `v` = new peak reading in dBFS, `dt` seconds. Returns true if level or peak moved.
    pub fn step(&mut self, v: f64, dt: f64) -> bool {
        let before = (self.level, self.peak);
        let v = v.min(M_MAX);
        self.level = if v > self.level { v } else { (self.level - RELEASE_DB_S * dt).max(v).max(M_MIN) };
        if v >= self.peak { self.peak = v; self.hold = HOLD_S; }
        else if self.hold > 0.0 { self.hold -= dt; }
        else { self.peak = (self.peak - FALL_DB_S * dt).max(self.level); }
        (self.level, self.peak) != before
    }
}

/// Segmented L/R peak meter with falling bars and a 3 s peak flag per channel.
pub struct LedMeter {
    pub area: gtk::DrawingArea,
    ch: [Cell<Ballistics>; 2],
}

impl LedMeter {
    pub fn new() -> Rc<LedMeter> {
        let area = gtk::DrawingArea::new();
        area.set_content_width(18);
        area.set_vexpand(true);
        let m = Rc::new(LedMeter {
            area: area.clone(),
            ch: [Cell::new(Ballistics::new()), Cell::new(Ballistics::new())],
        });
        let w = Rc::downgrade(&m);
        area.set_draw_func(move |_, cr, wd, ht| {
            if let Some(m) = w.upgrade() {
                m.draw(cr, wd as f64, ht as f64);
            }
        });
        m
    }

    /// New peak readings (dBFS) for L and R; `dt` seconds since the last frame.
    pub fn step(&self, l: f64, r: f64, dt: f64) {
        let mut moved = false;
        for (c, v) in [l, r].into_iter().enumerate() {
            let mut b = self.ch[c].get();
            moved |= b.step(v, dt);
            self.ch[c].set(b);
        }
        redraw_if(&self.area, moved);
    }

    fn draw(&self, cr: &cairo::Context, w: f64, h: f64) {
        let (top, bot) = (6.0, h - 6.0);
        let span = bot - top;
        let colw = ((w - 3.0) / 2.0).floor();
        let seg_h = span / SEG as f64;
        for c in 0..2 {
            let x = c as f64 * (colw + 3.0);
            let bl = self.ch[c].get();
            let lit = m_pos(bl.level);
            for i in 0..SEG {
                let p0 = i as f64 / SEG as f64;
                let db_here = M_MIN + (M_MAX - M_MIN) * ((p0 + 0.5 / SEG as f64).powf(1.0 / 1.6));
                let (r, g, b) = zone_rgb(zone(db_here));
                let on = p0 < lit;
                cr.set_source_rgba(r, g, b, if on { 0.95 } else { 0.08 });
                let y = bot - (i + 1) as f64 * seg_h;
                cr.rectangle(x, y + 1.0, colw, (seg_h - 1.0).max(1.0));
                let _ = cr.fill();
            }
            // peak flag, held 3 s
            if bl.peak > M_MIN + 0.5 {
                let y = bot - m_pos(bl.peak) * span;
                cr.set_source_rgb(FLAG_RGB.0, FLAG_RGB.1, FLAG_RGB.2);
                cr.rectangle(x, y - 1.5, colw, 3.0);
                let _ = cr.fill();
            }
        }
    }
}

// ---------- knob ----------

const SWEEP: f64 = 1.5 * PI; // 270°
const START: f64 = 0.75 * PI;


/// Rotary knob over min..max with a centre/default detent. Drag up/down or scroll;
/// double-click returns to the default.
pub struct Knob {
    pub area: gtk::DrawingArea,
    pub value: Cell<f64>,
    min: f64,
    max: f64,
    default: f64,
    /// same path as a mouse turn (clamp, redraw, notify) — used by MIDI control
    setter: std::cell::RefCell<Option<Rc<dyn Fn(f64)>>>,
    /// send level halo (zone arc + peak flag) instead of the printed scale, once enabled
    halo: Cell<Option<Ballistics>>,
}

impl Knob {
    /// Symmetric dB knob (EQ): -range..+range, 0.5 dB steps.
    pub fn new(size: i32, value: f64, range: f64, on_change: impl Fn(f64) + 'static) -> Rc<Knob> {
        Self::ranged(size, value, -range, range, 0.0, 0.5, on_change)
    }

    pub fn ranged(size: i32, value: f64, min: f64, max: f64, default: f64, step: f64, on_change: impl Fn(f64) + 'static) -> Rc<Knob> {
        let area = gtk::DrawingArea::new();
        area.set_content_width(size);
        area.set_content_height(size);
        area.set_halign(gtk::Align::Center);
        let k = Rc::new(Knob { area: area.clone(), value: Cell::new(value), min, max, default, setter: std::cell::RefCell::new(None), halo: Cell::new(None) });
        let cb: Rc<dyn Fn(f64)> = Rc::new(on_change);
        let w = Rc::downgrade(&k);
        area.set_draw_func(move |_, cr, wd, ht| {
            if let Some(k) = w.upgrade() {
                k.draw(cr, wd as f64, ht as f64);
            }
        });
        let set = {
            let (w, cb) = (Rc::downgrade(&k), cb.clone());
            Rc::new(move |v: f64| {
                if let Some(k) = w.upgrade() {
                    let v = ((v.clamp(k.min, k.max) - k.min) / step).round() * step + k.min;
                    if (v - k.value.get()).abs() > 1e-9 {
                        k.value.set(v);
                        k.area.queue_draw();
                        cb(v);
                    }
                }
            })
        };
        *k.setter.borrow_mut() = Some(set.clone());
        let per_px = (max - min) / 220.0;
        let drag = gtk::GestureDrag::new();
        let start = Rc::new(Cell::new(0.0));
        {
            let (w, s) = (Rc::downgrade(&k), start.clone());
            drag.connect_drag_begin(move |_, _, _| {
                if let Some(k) = w.upgrade() {
                    s.set(k.value.get());
                }
            });
        }
        {
            let (s, set) = (start.clone(), set.clone());
            drag.connect_drag_update(move |_, _, dy| set(s.get() - dy * per_px));
        }
        area.add_controller(drag);
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        {
            let (w, set) = (Rc::downgrade(&k), set.clone());
            scroll.connect_scroll(move |_, _, dy| {
                if let Some(k) = w.upgrade() {
                    set(k.value.get() - dy.signum() * step.max((max - min) / 60.0));
                }
                glib::Propagation::Stop
            });
        }
        area.add_controller(scroll);
        let click = gtk::GestureClick::new();
        {
            let set = set.clone();
            click.connect_pressed(move |_, n, _, _| {
                if n == 2 {
                    set(default);
                }
            });
        }
        area.add_controller(click);
        k
    }

    pub fn set_value(&self, v: f64) {
        self.value.set(v);
        self.area.queue_draw();
    }

    /// Turn the knob as if by hand (applies the change), `frac` 0..1 across its range.
    pub fn turn_to(&self, frac: f64) {
        let v = self.min + (self.max - self.min) * frac.clamp(0.0, 1.0);
        let f = self.setter.borrow().clone();
        if let Some(f) = f {
            f(v);
        }
    }

    /// Current position 0..1 across its range.
    pub fn frac(&self) -> f64 {
        (self.value.get() - self.min) / (self.max - self.min)
    }

    /// Switch this knob to the send-level halo (used by the bus send knobs).
    pub fn enable_halo(&self) {
        self.halo.set(Some(Ballistics::new()));
        self.area.queue_draw();
    }

    /// Feed the send level in dBFS (`SILENT_DB` when off); `dt` seconds since the last frame.
    pub fn halo_step(&self, db: f64, dt: f64) {
        if let Some(mut b) = self.halo.get() {
            redraw_if(&self.area, b.step(db, dt));
            self.halo.set(Some(b));
        }
    }

    fn draw(&self, cr: &cairo::Context, w: f64, h: f64) {
        let (cx, cy) = (w / 2.0, h / 2.0);
        let r = w.min(h) / 2.0 - 1.0;
        let a_of = |v: f64| START + SWEEP * ((v - self.min) / (self.max - self.min));
        let v = self.value.get();
        if let Some(bl) = self.halo.get() {
            // send halo: three zone arcs (dim track, lit to the level), then the held peak flag
            let ang = |db: f64| START + SWEEP * m_pos(db);
            cr.set_line_cap(cairo::LineCap::Butt);
            cr.set_line_width(r * 0.18);
            let pieces = [(M_MIN, AMBER_DB), (AMBER_DB, RED_DB), (RED_DB, M_MAX)];
            for (lo, hi) in pieces {
                let (zr, zg, zb) = zone_rgb(zone(lo));
                cr.set_source_rgba(zr, zg, zb, 0.15);
                cr.arc(cx, cy, r * 0.9, ang(lo), ang(hi));
                let _ = cr.stroke();
            }
            for (lo, hi) in pieces {
                if lo < bl.level {
                    let (zr, zg, zb) = zone_rgb(zone(lo));
                    cr.set_source_rgba(zr, zg, zb, 0.95);
                    cr.arc(cx, cy, r * 0.9, ang(lo), ang(hi.min(bl.level)));
                    let _ = cr.stroke();
                }
            }
            if bl.peak > M_MIN + 0.5 {
                let a = ang(bl.peak);
                cr.set_line_width(2.0);
                cr.set_source_rgb(FLAG_RGB.0, FLAG_RGB.1, FLAG_RGB.2);
                cr.move_to(cx + a.cos() * r * 0.72, cy + a.sin() * r * 0.72);
                cr.line_to(cx + a.cos() * r, cy + a.sin() * r);
                let _ = cr.stroke();
            }
        } else {
            // printed scale: 11 ticks round the skirt
            cr.set_line_cap(cairo::LineCap::Butt);
            for i in 0..=10 {
                let a = START + SWEEP * i as f64 / 10.0;
                cr.set_line_width(if i % 5 == 0 { 1.4 } else { 0.8 });
                cr.set_source_rgba(0.85, 0.85, 0.85, if i % 5 == 0 { 0.8 } else { 0.4 });
                cr.move_to(cx + a.cos() * r * 0.84, cy + a.sin() * r * 0.84);
                cr.line_to(cx + a.cos() * r, cy + a.sin() * r);
                let _ = cr.stroke();
            }
            // amber arc from the detent to the value
            if (v - self.default).abs() > 1e-6 {
                cr.set_line_width(2.0);
                cr.set_source_rgb(1.0, 0.66, 0.2);
                let (a0, a1) = (a_of(self.default), a_of(v));
                cr.arc(cx, cy, r * 0.78, a0.min(a1), a0.max(a1));
                let _ = cr.stroke();
            }
        }
        // black skirt with grip notches
        let kr = r * 0.7;
        cr.set_source_rgb(0.06, 0.06, 0.07);
        cr.arc(cx, cy, kr, 0.0, 2.0 * PI);
        let _ = cr.fill();
        cr.set_line_width(0.8);
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.12);
        for i in 0..24 {
            let a = i as f64 * 2.0 * PI / 24.0;
            cr.move_to(cx + a.cos() * kr * 0.82, cy + a.sin() * kr * 0.82);
            cr.line_to(cx + a.cos() * kr * 0.98, cy + a.sin() * kr * 0.98);
        }
        let _ = cr.stroke();
        // brushed aluminium cap
        let cap = cairo::LinearGradient::new(cx - kr, cy - kr, cx + kr, cy + kr);
        for (o, g) in [(0.0, 0.86), (0.3, 0.62), (0.5, 0.8), (0.7, 0.55), (1.0, 0.7)] {
            cap.add_color_stop_rgb(o, g, g, g * 1.02);
        }
        cr.arc(cx, cy, kr * 0.72, 0.0, 2.0 * PI);
        let _ = cr.set_source(&cap);
        let _ = cr.fill();
        // indicator line
        let a = a_of(v);
        cr.set_line_cap(cairo::LineCap::Round);
        cr.set_line_width(2.0);
        cr.set_source_rgb(0.05, 0.05, 0.05);
        cr.move_to(cx + a.cos() * kr * 0.15, cy + a.sin() * kr * 0.15);
        cr.line_to(cx + a.cos() * kr * 0.68, cy + a.sin() * kr * 0.68);
        let _ = cr.stroke();
        cr.set_line_width(1.6);
        cr.set_source_rgb(1.0, 1.0, 1.0);
        cr.move_to(cx + a.cos() * kr * 0.78, cy + a.sin() * kr * 0.78);
        cr.line_to(cx + a.cos() * kr * 0.98, cy + a.sin() * kr * 0.98);
        let _ = cr.stroke();
    }
}

// ---------- VU meter ----------

/// Amber backlit VU meter. Classic ~300 ms ballistics on an RMS feed, reads -20..+3 VU
/// (0 VU = -12 dBFS RMS).
pub struct Vu {
    pub area: gtk::DrawingArea,
    pos: Cell<f64>, // VU
    vel: Cell<f64>,
    pk: Cell<Ballistics>, // peak flag over the needle, held 3 s
}

const VU_MIN: f64 = -20.0;
const VU_MAX: f64 = 3.0;

fn vu_frac(vu: f64) -> f64 {
    // printed VU scales are roughly linear in voltage, so -20..0 is squeezed left
    let volts = |d: f64| 10f64.powf(d / 20.0);
    (volts(vu.clamp(VU_MIN - 3.0, VU_MAX + 1.0)) - volts(VU_MIN)) / (volts(VU_MAX) - volts(VU_MIN))
}

impl Vu {
    pub fn new(width: i32) -> Rc<Vu> {
        let area = gtk::DrawingArea::new();
        area.set_content_width(width);
        area.set_content_height((width as f64 * 0.62) as i32);
        area.set_halign(gtk::Align::Center);
        let v = Rc::new(Vu { area: area.clone(), pos: Cell::new(VU_MIN - 3.0), vel: Cell::new(0.0), pk: Cell::new(Ballistics::new()) });
        let w = Rc::downgrade(&v);
        area.set_draw_func(move |_, cr, wd, ht| {
            if let Some(v) = w.upgrade() {
                v.draw(cr, wd as f64, ht as f64);
            }
        });
        v
    }

    /// Feed an RMS level in dBFS; needle follows a damped spring (~300 ms rise).
    pub fn step(&self, dbfs: f64, dt: f64) {
        let (p, v, moved) = vu_spring(self.pos.get(), self.vel.get(), dbfs, dt);
        self.vel.set(v);
        self.pos.set(p);
        let mut b = self.pk.get();
        let pk_moved = b.step(self.pos.get() - 12.0, dt);
        self.pk.set(b);
        redraw_if(&self.area, moved | pk_moved);
    }

    fn draw(&self, cr: &cairo::Context, w: f64, h: f64) {
        // black bezel
        let rr = 4.0;
        let rounded = |x: f64, y: f64, w: f64, h: f64, r: f64| {
            cr.new_sub_path();
            cr.arc(x + w - r, y + r, r, -PI / 2.0, 0.0);
            cr.arc(x + w - r, y + h - r, r, 0.0, PI / 2.0);
            cr.arc(x + r, y + h - r, r, PI / 2.0, PI);
            cr.arc(x + r, y + r, r, PI, 1.5 * PI);
            cr.close_path();
        };
        rounded(0.0, 0.0, w, h, rr);
        cr.set_source_rgb(0.05, 0.05, 0.055);
        let _ = cr.fill();
        // amber backlit face
        let (fx, fy, fw, fh) = (3.0, 3.0, w - 6.0, h - 6.0);
        rounded(fx, fy, fw, fh, rr - 1.5);
        let face = cairo::RadialGradient::new(fx + fw / 2.0, fy + fh * 0.9, fh * 0.1, fx + fw / 2.0, fy + fh * 0.6, fw * 0.75);
        face.add_color_stop_rgb(0.0, 1.0, 0.86, 0.55);
        face.add_color_stop_rgb(0.6, 0.96, 0.74, 0.36);
        face.add_color_stop_rgb(1.0, 0.6, 0.38, 0.12);
        let _ = cr.set_source(&face);
        let _ = cr.fill();

        // arc geometry: pivot below the face
        let (px, py) = (fx + fw / 2.0, fy + fh * 1.12);
        let rad = fh * 0.92;
        let (a0, a1) = (-PI / 2.0 - 0.78, -PI / 2.0 + 0.78);
        let ang = |vu: f64| a0 + (a1 - a0) * vu_frac(vu);
        let _ = cr.save();
        rounded(fx, fy, fw, fh, rr - 1.5);
        cr.clip();
        // scale arc: black to 0, red above
        cr.set_line_width(1.2);
        cr.set_source_rgb(0.12, 0.08, 0.03);
        cr.arc(px, py, rad * 0.86, ang(VU_MIN), ang(0.0));
        let _ = cr.stroke();
        cr.set_line_width(3.0);
        cr.set_source_rgb(0.75, 0.1, 0.05);
        cr.arc(px, py, rad * 0.87, ang(0.0), ang(VU_MAX));
        let _ = cr.stroke();
        // peak flag, held 3 s
        let pv = self.pk.get().peak + 12.0;
        if pv > VU_MIN {
            let a = ang(pv);
            cr.set_source_rgb(FLAG_RGB.0, FLAG_RGB.1, FLAG_RGB.2);
            cr.set_line_width(2.0);
            cr.move_to(px + a.cos() * rad * 0.80, py + a.sin() * rad * 0.80);
            cr.line_to(px + a.cos() * rad * 0.97, py + a.sin() * rad * 0.97);
            let _ = cr.stroke();
        }
        cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
        cr.set_font_size((fh * 0.13).max(7.0));
        for (v, s) in [(-20.0, "20"), (-10.0, "10"), (-7.0, "7"), (-5.0, "5"), (-3.0, "3"), (-1.0, "1"), (0.0, "0"), (1.0, "+1"), (2.0, "2"), (3.0, "+3")] {
            let a = ang(v);
            let red = v >= 0.0;
            if red { cr.set_source_rgb(0.7, 0.08, 0.04) } else { cr.set_source_rgb(0.12, 0.08, 0.03) }
            cr.set_line_width(1.0);
            cr.move_to(px + a.cos() * rad * 0.86, py + a.sin() * rad * 0.86);
            cr.line_to(px + a.cos() * rad * 0.95, py + a.sin() * rad * 0.95);
            let _ = cr.stroke();
            if ["20", "10", "5", "3", "0", "+3"].contains(&s) {
                text_centered(cr, px + a.cos() * rad * 1.04, py + a.sin() * rad * 1.04, s);
            }
        }
        cr.set_font_size((fh * 0.16).max(8.0));
        cr.set_source_rgb(0.12, 0.08, 0.03);
        text_centered(cr, fx + fw * 0.82, fy + fh * 0.8, "VU");
        // needle
        let a = ang(self.pos.get());
        cr.set_line_width(1.3);
        cr.set_source_rgb(0.05, 0.05, 0.05);
        cr.move_to(px + a.cos() * rad * 0.25, py + a.sin() * rad * 0.25);
        cr.line_to(px + a.cos() * rad * 0.98, py + a.sin() * rad * 0.98);
        let _ = cr.stroke();
        let _ = cr.restore();
        // glass edge highlight
        rounded(fx + 0.5, fy + 0.5, fw - 1.0, fh - 1.0, rr - 1.5);
        cr.set_line_width(1.0);
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.25);
        let _ = cr.stroke();
    }
}

// ---------- panel screws ----------

/// A row with a slotted screw at each end, for the top/bottom of a rack panel.
pub fn screw_row() -> gtk::DrawingArea {
    let area = gtk::DrawingArea::new();
    area.set_content_height(12);
    area.set_hexpand(true);
    area.set_draw_func(|_, cr, w, h| {
        let (w, h) = (w as f64, h as f64);
        for (i, cx) in [6.0, w - 6.0].into_iter().enumerate() {
            let cy = h / 2.0;
            let g = cairo::RadialGradient::new(cx - 1.5, cy - 1.5, 0.5, cx, cy, 5.0);
            g.add_color_stop_rgb(0.0, 0.85, 0.85, 0.87);
            g.add_color_stop_rgb(1.0, 0.32, 0.32, 0.35);
            cr.arc(cx, cy, 4.5, 0.0, 2.0 * PI);
            let _ = cr.set_source(&g);
            let _ = cr.fill();
            // slot, each screw turned a little differently
            let a = 0.6 + i as f64 * 1.1 + (w * 0.37).sin();
            cr.set_line_width(1.2);
            cr.set_source_rgba(0.08, 0.08, 0.09, 0.9);
            cr.move_to(cx - a.cos() * 3.4, cy - a.sin() * 3.4);
            cr.line_to(cx + a.cos() * 3.4, cy + a.sin() * 3.4);
            let _ = cr.stroke();
        }
    });
    area
}

// ---------- RGB spectrum analyser ----------

const FFT_N: usize = 4096;
pub const BANDS: usize = 72;
const F_LO: f64 = 25.0;
const F_HI: f64 = 18000.0;
const RATE: f64 = 48000.0;

fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -2.0 * std::f32::consts::PI / len as f32;
        let (wr, wi) = (ang.cos(), ang.sin());
        for start in (0..n).step_by(len) {
            let (mut cr, mut ci) = (1.0f32, 0.0f32);
            for k in 0..len / 2 {
                let (a, b) = (start + k, start + k + len / 2);
                let (tr, ti) = (re[b] * cr - im[b] * ci, re[b] * ci + im[b] * cr);
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                let t = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = t;
            }
        }
        len <<= 1;
    }
}

/// Spectrum of one bus, drawn as rainbow bars with falling peak caps. Fed by the chosen bus
/// meter's own tap (no extra PipeWire stream).
pub struct Spectrum {
    pub area: gtk::DrawingArea,
    shown: std::cell::RefCell<Vec<f64>>,
    peaks: std::cell::RefCell<Vec<(f64, f64)>>, // (level, hold time)
    target: std::cell::RefCell<String>,
    born: Instant,
}

impl Spectrum {
    pub fn new(target: &str) -> Rc<Spectrum> {
        let area = gtk::DrawingArea::new();
        area.set_content_height(200);
        area.set_hexpand(true);
        let s = Rc::new(Spectrum {
            area: area.clone(),
            shown: std::cell::RefCell::new(vec![0.0; BANDS]),
            peaks: std::cell::RefCell::new(vec![(0.0, 0.0); BANDS]),
            target: std::cell::RefCell::new(target.to_string()),
            born: Instant::now(),
        });
        let w = Rc::downgrade(&s);
        area.set_draw_func(move |_, cr, wd, ht| {
            if let Some(s) = w.upgrade() {
                s.draw(cr, wd as f64, ht as f64);
            }
        });
        s
    }

    /// Which bus this spectrum is labelled and keyed by.
    pub fn set_target(&self, t: &str) {
        *self.target.borrow_mut() = t.to_string();
    }

    pub fn target(&self) -> String {
        self.target.borrow().clone()
    }

    /// Advance smoothing and redraw (call ~25-60 Hz). `src` is the chosen bus meter's band buffer.
    pub fn step(&self, dt: f64, src: Option<&Arc<Mutex<Vec<f32>>>>) {
        let d = src.map(|a| a.lock().unwrap().clone()).unwrap_or_else(|| vec![0.0; BANDS]);
        let mut sh = self.shown.borrow_mut();
        let mut pk = self.peaks.borrow_mut();
        let moved = smooth_spectrum(&mut sh, &mut pk, &d, dt);
        drop((sh, pk));
        redraw_if(&self.area, moved);
    }

    fn draw(&self, cr: &cairo::Context, w: f64, h: f64) {
        let (lx, ly, lw, lh) = (30.0, 6.0, w - 36.0, h - 24.0);
        cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
        cr.set_font_size(9.0);
        cr.set_line_width(1.0);
        for db in [0, 24, 48] {
            let y = (ly + lh * db as f64 / 72.0).round() + 0.5;
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.06);
            cr.move_to(lx, y);
            cr.line_to(lx + lw, y);
            let _ = cr.stroke();
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.35);
            cr.move_to(4.0, y + 3.0);
            let _ = cr.show_text(&if db == 0 { "0".to_string() } else { format!("-{db}") });
        }
        let fx = |f: f64| lx + lw * (f / F_LO).ln() / (F_HI / F_LO).ln();
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.35);
        for (f, s) in [(50.0, "50"), (100.0, "100"), (250.0, "250"), (500.0, "500"), (1000.0, "1k"), (2500.0, "2.5k"), (5000.0, "5k"), (10000.0, "10k")] {
            text_centered(cr, fx(f), ly + lh + 10.0, s);
        }
        // bars: hue runs across the frequency range and slowly cycles
        let t = self.born.elapsed().as_secs_f64();
        let sh = self.shown.borrow();
        let pk = self.peaks.borrow();
        let bw = lw / BANDS as f64;
        for i in 0..BANDS {
            let (r, g, b) = hsv(i as f64 / BANDS as f64 * 300.0 + t * 20.0, 0.75, 0.95);
            let x = lx + i as f64 * bw;
            let bh = sh[i] * lh;
            cr.set_source_rgba(r, g, b, 0.85);
            cr.rectangle(x, ly + lh - bh, (bw - 2.0).max(1.0), bh);
            let _ = cr.fill();
            // peak line left behind
            let py = (ly + lh - pk[i].0 * lh).min(ly + lh - 1.0);
            cr.set_source_rgba(r, g, b, 1.0);
            cr.rectangle(x, py - 1.5, (bw - 2.0).max(1.0), 1.5);
            let _ = cr.fill();
        }
    }
}

// ---------- patch bay ----------

/// Everything the patch bay draws: who plays into which input, and where each input is sent.
#[derive(Clone, Default, PartialEq)]
pub struct Patch {
    pub apps: Vec<(String, usize, u64)>,          // app name -> input index (+ stream serial)
    pub inputs: Vec<(String, (f64, f64, f64))>,   // name, colour
    pub chains: Vec<Vec<(String, bool)>>,         // per input: processing blocks in order, lit when active
    pub buses: Vec<(String, String)>,             // bus name, destination
    pub routes: Vec<(usize, usize, bool, f64)>,   // input, bus, on, level dB
}

/// What was clicked in the patch bay.
#[derive(Clone, Copy, Debug)]
pub enum Hit {
    App(usize),           // app jack (index into Patch.apps)
    Block(usize, usize),  // input, block in its chain
    Sends(usize),         // input's output jack -> its bus routes
}

pub struct PatchBay {
    pub area: gtk::DrawingArea,
    data: std::cell::RefCell<Patch>,
    hits: std::cell::RefCell<Vec<(f64, f64, f64, f64, Hit)>>,
}

pub fn hex_rgb(hex: &str) -> (f64, f64, f64) {
    let h = hex.trim_start_matches('#');
    let p = |i: usize| u8::from_str_radix(h.get(i..i + 2).unwrap_or("cc"), 16).unwrap_or(200) as f64 / 255.0;
    (p(0), p(2), p(4))
}

fn jack(cr: &cairo::Context, x: f64, y: f64) {
    let g = cairo::RadialGradient::new(x - 2.0, y - 2.0, 0.5, x, y, 7.0);
    g.add_color_stop_rgb(0.0, 0.9, 0.9, 0.92);
    g.add_color_stop_rgb(1.0, 0.35, 0.35, 0.38);
    cr.arc(x, y, 6.5, 0.0, 2.0 * PI);
    let _ = cr.set_source(&g);
    let _ = cr.fill();
    cr.arc(x, y, 3.2, 0.0, 2.0 * PI);
    cr.set_source_rgb(0.02, 0.02, 0.02);
    let _ = cr.fill();
}

fn cable(cr: &cairo::Context, (x0, y0): (f64, f64), (x1, y1): (f64, f64), (r, g, b): (f64, f64, f64), alpha: f64, floor: f64) {
    // sagging cable: control points pulled down like gravity, but never below the panel floor
    let sag = (10.0 + (x1 - x0).abs() * 0.05).min((floor - y0.max(y1)).max(0.0) * 1.3);
    let (c0, c1) = ((x0 + (x1 - x0) * 0.3, y0.max(y1) + sag), (x0 + (x1 - x0) * 0.7, y0.max(y1) + sag));
    cr.set_line_cap(cairo::LineCap::Round);
    for (wd, (rr, gg, bb), a) in [(6.0, (0.0, 0.0, 0.0), 0.45 * alpha), (4.0, (r, g, b), alpha)] {
        cr.set_line_width(wd);
        cr.set_source_rgba(rr, gg, bb, a);
        cr.move_to(x0, y0);
        cr.curve_to(c0.0, c0.1, c1.0, c1.1, x1, y1);
        let _ = cr.stroke();
    }
    // plug sleeves
    for (x, y) in [(x0, y0), (x1, y1)] {
        cr.arc(x, y, 4.2, 0.0, 2.0 * PI);
        cr.set_source_rgba(r * 0.7, g * 0.7, b * 0.7, alpha);
        let _ = cr.fill();
    }
}

impl PatchBay {
    pub fn new() -> Rc<PatchBay> {
        let area = gtk::DrawingArea::new();
        area.set_content_height(230);
        area.set_hexpand(true);
        let p = Rc::new(PatchBay { area: area.clone(), data: std::cell::RefCell::new(Patch::default()), hits: std::cell::RefCell::new(vec![]) });
        // pointer cursor over anything clickable
        let motion = gtk::EventControllerMotion::new();
        let w2 = Rc::downgrade(&p);
        motion.connect_motion(move |_, x, y| {
            if let Some(p) = w2.upgrade() {
                let over = p.hit(x, y).is_some();
                p.area.set_cursor_from_name(if over { Some("pointer") } else { None });
            }
        });
        area.add_controller(motion);
        let w = Rc::downgrade(&p);
        area.set_draw_func(move |_, cr, wd, ht| {
            if let Some(p) = w.upgrade() {
                p.draw(cr, wd as f64, ht as f64);
            }
        });
        p
    }

    pub fn hit(&self, x: f64, y: f64) -> Option<Hit> {
        self.hits.borrow().iter().find(|(x0, y0, x1, y1, _)| x >= *x0 && x <= *x1 && y >= *y0 && y <= *y1).map(|h| h.4)
    }

    /// Call `f(hit, x, y)` when something clickable is clicked.
    pub fn on_click(self: &Rc<Self>, f: impl Fn(Hit, f64, f64) + 'static) {
        let click = gtk::GestureClick::new();
        let w = Rc::downgrade(self);
        click.connect_released(move |_, _, x, y| {
            if let Some(h) = w.upgrade().and_then(|p| p.hit(x, y)) {
                f(h, x, y);
            }
        });
        self.area.add_controller(click);
    }

    pub fn data(&self) -> Patch {
        self.data.borrow().clone()
    }

    pub fn set(&self, d: Patch) {
        if *self.data.borrow() != d {
            *self.data.borrow_mut() = d;
            self.area.queue_draw();
        }
    }

    fn draw(&self, cr: &cairo::Context, w: f64, h: f64) {
        let d = self.data.borrow();
        let mut hits = self.hits.borrow_mut();
        hits.clear();
        // columns: apps | input in | chain ... | input out | buses | destinations
        let (x_app, x_in, x_c0, x_c1, x_out, x_bus, x_dst) = (w * 0.07, w * 0.19, w * 0.25, w * 0.55, w * 0.6, w * 0.76, w * 0.91);
        let (top, bot) = (34.0, h - 22.0);
        let ypos = |i: usize, n: usize| if n <= 1 { (top + bot) / 2.0 } else { top + (bot - top) * i as f64 / (n - 1) as f64 };
        cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
        cr.set_font_size(9.0);
        for (x, t) in [(x_app, "APPS"), (x_in, "INPUTS"), ((x_c0 + x_c1) / 2.0, "SIGNAL CHAIN"), (x_bus, "BUSES"), (x_dst, "DESTINATIONS")] {
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.35);
            text_centered(cr, x, 12.0, t);
        }
        let (na, ni, nb) = (d.apps.len(), d.inputs.len(), d.buses.len());
        let floor = h - 4.0;
        // app -> input cables
        for (k, (_, inp, _)) in d.apps.iter().enumerate() {
            let col = d.inputs.get(*inp).map(|i| i.1).unwrap_or((0.7, 0.7, 0.7));
            cable(cr, (x_app + 12.0, ypos(k, na)), (x_in - 8.0, ypos(*inp, ni)), col, 0.9, floor);
        }
        // signal chain per input: straight wire through each block
        for (i, (_, col)) in d.inputs.iter().enumerate() {
            let y = ypos(i, ni);
            let chain = d.chains.get(i).cloned().unwrap_or_default();
            let n = chain.len().max(1);
            let bx = |k: usize| x_c0 + (x_c1 - x_c0) * (k as f64 + 0.5) / n as f64;
            cr.set_line_width(3.0);
            cr.set_source_rgba(col.0, col.1, col.2, 0.85);
            cr.move_to(x_in + 8.0, y);
            cr.line_to(x_out - 8.0, y);
            let _ = cr.stroke();
            let bw = ((x_c1 - x_c0) / n as f64 - 6.0).clamp(26.0, 52.0);
            cr.set_font_size(8.5);
            for (k, (name, on)) in chain.iter().enumerate() {
                let (cx, bh) = (bx(k), 16.0);
                let (x0, y0) = (cx - bw / 2.0, y - bh / 2.0);
                hits.push((x0, y0, x0 + bw, y0 + bh, Hit::Block(i, k)));
                cr.new_sub_path();
                cr.arc(x0 + bw - 3.0, y0 + 3.0, 3.0, -PI / 2.0, 0.0);
                cr.arc(x0 + bw - 3.0, y0 + bh - 3.0, 3.0, 0.0, PI / 2.0);
                cr.arc(x0 + 3.0, y0 + bh - 3.0, 3.0, PI / 2.0, PI);
                cr.arc(x0 + 3.0, y0 + 3.0, 3.0, PI, 1.5 * PI);
                cr.close_path();
                if *on { cr.set_source_rgb(0.24, 0.17, 0.07) } else { cr.set_source_rgb(0.1, 0.1, 0.11) }
                let _ = cr.fill_preserve();
                cr.set_line_width(1.0);
                if *on { cr.set_source_rgb(1.0, 0.66, 0.2) } else { cr.set_source_rgb(0.25, 0.25, 0.28) }
                let _ = cr.stroke();
                // power LED
                cr.arc(x0 + 5.0, y, 1.8, 0.0, 2.0 * PI);
                if *on { cr.set_source_rgb(1.0, 0.7, 0.25) } else { cr.set_source_rgb(0.2, 0.2, 0.22) }
                let _ = cr.fill();
                if *on { cr.set_source_rgb(1.0, 0.8, 0.5) } else { cr.set_source_rgba(1.0, 1.0, 1.0, 0.3) }
                text_centered(cr, cx + 2.0, y, name);
            }
        }
        // input out -> bus cables, bus -> destination
        for &(i, b, on, db) in &d.routes {
            if !on || i >= ni || b >= nb {
                continue;
            }
            let a = (0.35 + 0.65 * ((db + 40.0) / 40.0).clamp(0.0, 1.0)).min(0.95);
            cable(cr, (x_out + 8.0, ypos(i, ni)), (x_bus - 12.0, ypos(b, nb)), d.inputs[i].1, a, floor);
        }
        for b in 0..nb {
            cable(cr, (x_bus + 12.0, ypos(b, nb)), (x_dst - 12.0, ypos(b, nb)), (0.78, 0.78, 0.8), 0.85, floor);
        }
        // jacks + labels
        cr.set_font_size(10.0);
        let text_at = |cr: &cairo::Context, x: f64, y: f64, t: &str, right_align: bool| {
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.85);
            if let Ok(e) = cr.text_extents(t) {
                cr.move_to(if right_align { x - e.width() - e.x_bearing() } else { x }, y + 4.0);
                let _ = cr.show_text(t);
            }
        };
        for (k, (n, _, _)) in d.apps.iter().enumerate() {
            hits.push((x_app - 140.0, ypos(k, na) - 11.0, x_app + 24.0, ypos(k, na) + 11.0, Hit::App(k)));
            jack(cr, x_app + 12.0, ypos(k, na));
            text_at(cr, x_app, ypos(k, na), n, true);
        }
        if d.apps.is_empty() {
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.3);
            text_centered(cr, x_app, (top + bot) / 2.0, "nothing playing");
        }
        for (k, (n, _)) in d.inputs.iter().enumerate() {
            let y = ypos(k, ni);
            jack(cr, x_in - 8.0, y);
            jack(cr, x_out + 8.0, y);
            hits.push((x_out - 4.0, y - 11.0, x_out + 20.0, y + 11.0, Hit::Sends(k)));
            text_at(cr, x_in + 2.0, y - 10.0, n, false);
        }
        for (k, (n, dst)) in d.buses.iter().enumerate() {
            let y = ypos(k, nb);
            jack(cr, x_bus - 12.0, y);
            jack(cr, x_bus + 12.0, y);
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.85);
            text_centered(cr, x_bus, y - 13.0, n);
            jack(cr, x_dst - 12.0, y);
            text_at(cr, x_dst, y, dst, false);
        }
    }
}

// ---------- before/after analyser for one channel ----------

/// Rolling FFT over a stereo stream, producing BANDS log bands (0..1 for -72..0 dB).
/// Fed by the meters' own capture, so analysers never open extra PipeWire streams.
pub struct Fft {
    edges: Vec<f64>,
    window: Vec<f32>,
    ring: Vec<f32>,
    pos: usize,
    fresh: usize,
    re: Vec<f32>,
    im: Vec<f32>,
}

impl Fft {
    pub fn new() -> Fft {
        Fft {
            edges: (0..=BANDS).map(|i| F_LO * (F_HI / F_LO).powf(i as f64 / BANDS as f64) * FFT_N as f64 / RATE).collect(),
            window: (0..FFT_N).map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / (FFT_N - 1) as f32).cos()).collect(),
            ring: vec![0.0; FFT_N],
            pos: 0,
            fresh: 0,
            re: vec![0.0; FFT_N],
            im: vec![0.0; FFT_N],
        }
    }

    pub fn push(&mut self, l: f32, r: f32) {
        self.ring[self.pos] = 0.5 * (l + r);
        self.pos = (self.pos + 1) % FFT_N;
        self.fresh += 1;
    }

    /// New bands roughly 20 times a second, otherwise None.
    pub fn take(&mut self) -> Option<Vec<f32>> {
        if self.fresh < 2400 {
            return None;
        }
        self.fresh = 0;
        for i in 0..FFT_N {
            self.re[i] = self.ring[(self.pos + i) % FFT_N] * self.window[i];
            self.im[i] = 0.0;
        }
        fft(&mut self.re, &mut self.im);
        let (re, im) = (&self.re, &self.im);
        let mag = |b: usize| (re[b] * re[b] + im[b] * im[b]).sqrt() * 4.0 / FFT_N as f32;
        let mut bands = vec![0.0f32; BANDS];
        for (k, band) in bands.iter_mut().enumerate() {
            let (a, b) = (self.edges[k], self.edges[k + 1]);
            let (ia, ib) = (a.floor() as usize, (b.ceil() as usize).min(FFT_N / 2 - 1));
            let m = if ib <= ia + 1 {
                let c = (a + b) / 2.0;
                let i0 = c.floor() as usize;
                let t = (c - i0 as f64) as f32;
                mag(i0) * (1.0 - t) + mag(i0 + 1) * t
            } else {
                (ia..ib).map(mag).fold(0.0f32, f32::max)
            };
            let f = F_LO * (F_HI / F_LO).powf((k as f64 + 0.5) / BANDS as f64);
            let db = 20.0 * m.max(1e-9).log10() + 3.0 * (f / 1000.0).log2() as f32;
            *band = ((db + 72.0) / 72.0).clamp(0.0, 1.0);
        }
        Some(bands)
    }
}

/// True for test/screenshot runs: never open audio streams on the live PipeWire.
pub fn no_audio() -> bool {
    std::env::var_os("MIXER_NO_AUDIO").is_some()
}

/// RBJ biquad magnitude (dB) at frequency f for one EQ band.
fn biquad_db(kind: u8, f0: f64, q: f64, gain: f64, f: f64) -> f64 {
    if gain.abs() < 0.01 {
        return 0.0;
    }
    let fs = RATE;
    let a = 10f64.powf(gain / 40.0);
    let w0 = 2.0 * PI * f0 / fs;
    let (cw, sw) = (w0.cos(), w0.sin());
    let alpha = sw / (2.0 * q);
    let (b0, b1, b2, a0, a1, a2) = match kind {
        0 => {
            let s = 2.0 * a.sqrt() * alpha;
            (a * ((a + 1.0) - (a - 1.0) * cw + s), 2.0 * a * ((a - 1.0) - (a + 1.0) * cw), a * ((a + 1.0) - (a - 1.0) * cw - s),
             (a + 1.0) + (a - 1.0) * cw + s, -2.0 * ((a - 1.0) + (a + 1.0) * cw), (a + 1.0) + (a - 1.0) * cw - s)
        }
        2 => {
            let s = 2.0 * a.sqrt() * alpha;
            (a * ((a + 1.0) + (a - 1.0) * cw + s), -2.0 * a * ((a - 1.0) + (a + 1.0) * cw), a * ((a + 1.0) + (a - 1.0) * cw - s),
             (a + 1.0) - (a - 1.0) * cw + s, 2.0 * ((a - 1.0) - (a + 1.0) * cw), (a + 1.0) - (a - 1.0) * cw - s)
        }
        _ => (1.0 + alpha * a, -2.0 * cw, 1.0 - alpha * a, 1.0 + alpha / a, -2.0 * cw, 1.0 - alpha / a),
    };
    let w = 2.0 * PI * f / fs;
    let (c1, s1, c2, s2) = (w.cos(), w.sin(), (2.0 * w).cos(), (2.0 * w).sin());
    let (nr, ni) = (b0 + b1 * c1 + b2 * c2, -(b1 * s1 + b2 * s2));
    let (dr, di) = (a0 + a1 * c1 + a2 * c2, -(a1 * s1 + a2 * s2));
    10.0 * ((nr * nr + ni * ni) / (dr * dr + di * di)).log10()
}

/// Combined response of the 12-band EQ at f (same filters the engine builds).
pub fn eq_response_db(bands: &[(f64, &str)], gains: &[f64], f: f64) -> f64 {
    let last = bands.len() - 1;
    bands
        .iter()
        .enumerate()
        .map(|(k, (f0, _))| {
            let (kind, q) = if k == 0 { (0, 0.707) } else if k == last { (2, 0.707) } else { (1, 1.6) };
            biquad_db(kind, *f0, q, gains.get(k).copied().unwrap_or(0.0), f)
        })
        .sum()
}

/// IN (grey area) vs OUT (channel colour) spectrum with the EQ curve on top.
pub struct Analyzer {
    pub area: gtk::DrawingArea,
    pre: Arc<Mutex<Vec<f32>>>,
    post: Arc<Mutex<Vec<f32>>>,
    shown: std::cell::RefCell<(Vec<f64>, Vec<f64>)>,
}

impl Analyzer {
    /// `eq` returns the channel's current EQ gains each frame; `colour` is the channel colour.
    /// `pre`/`post` are band buffers filled by the channel's IN and OUT meters.
    pub fn new(pre: Arc<Mutex<Vec<f32>>>, post: Arc<Mutex<Vec<f32>>>, colour: (f64, f64, f64), eq: impl Fn() -> Vec<f64> + 'static, bands: &'static [(f64, &'static str)]) -> Rc<Analyzer> {
        let area = gtk::DrawingArea::new();
        area.set_content_height(64);
        area.set_hexpand(true);
        let a = Rc::new(Analyzer {
            area: area.clone(),
            pre,
            post,
            shown: std::cell::RefCell::new((vec![0.0; BANDS], vec![0.0; BANDS])),
        });
        let w = Rc::downgrade(&a);
        area.set_draw_func(move |_, cr, wd, ht| {
            if let Some(a) = w.upgrade() {
                a.draw(cr, wd as f64, ht as f64, colour, &eq(), bands);
            }
        });
        a
    }

    pub fn step(&self, dt: f64) {
        let (p, q) = (self.pre.lock().unwrap().clone(), self.post.lock().unwrap().clone());
        let mut sh = self.shown.borrow_mut();
        let (a, b) = &mut *sh;
        // non-short-circuit: both sides always step
        let moved = smooth_bands(a, &p, dt) | smooth_bands(b, &q, dt);
        drop(sh);
        redraw_if(&self.area, moved);
    }

    fn draw(&self, cr: &cairo::Context, w: f64, h: f64, col: (f64, f64, f64), eq: &[f64], bands: &[(f64, &str)]) {
        cr.set_source_rgb(0.04, 0.04, 0.05);
        cr.rectangle(0.0, 0.0, w, h);
        let _ = cr.fill();
        let x_of = |i: f64| w * i / (BANDS - 1) as f64;
        let sh = self.shown.borrow();
        // IN: grey filled area
        cr.move_to(0.0, h);
        for i in 0..BANDS {
            cr.line_to(x_of(i as f64), h - sh.0[i] * h);
        }
        cr.line_to(w, h);
        cr.close_path();
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.16);
        let _ = cr.fill();
        // OUT: channel-coloured line
        cr.set_line_width(1.6);
        cr.set_source_rgba(col.0, col.1, col.2, 0.95);
        for i in 0..BANDS {
            let (x, y) = (x_of(i as f64), h - sh.1[i] * h);
            if i == 0 { cr.move_to(x, y) } else { cr.line_to(x, y) }
        }
        let _ = cr.stroke();
        // EQ curve: centre line = 0 dB, ±15 dB to the panel edges
        cr.set_line_width(1.0);
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.12);
        cr.move_to(0.0, h / 2.0);
        cr.line_to(w, h / 2.0);
        let _ = cr.stroke();
        cr.set_line_width(1.8);
        cr.set_source_rgba(1.0, 0.66, 0.2, 0.95);
        let n = 120;
        for k in 0..=n {
            let f = F_LO * (F_HI / F_LO).powf(k as f64 / n as f64);
            let db = eq_response_db(bands, eq, f).clamp(-15.0, 15.0);
            let (x, y) = (w * k as f64 / n as f64, h / 2.0 - db / 15.0 * (h / 2.0 - 3.0));
            if k == 0 { cr.move_to(x, y) } else { cr.line_to(x, y) }
        }
        let _ = cr.stroke();
        // legend
        cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
        cr.set_font_size(8.5);
        for (k, (t, c)) in [("IN", (0.7, 0.7, 0.7)), ("OUT", col), ("EQ", (1.0, 0.66, 0.2))].iter().enumerate() {
            cr.set_source_rgb(c.0, c.1, c.2);
            cr.move_to(6.0 + k as f64 * 30.0, 11.0);
            let _ = cr.show_text(t);
        }
    }
}

// ---------- rack unit meters ----------

/// Small gain-reduction needle: rests at 0 on the right, swings left as GR increases.
pub struct GrMeter {
    pub area: gtk::DrawingArea,
    pos: Cell<f64>, // dB of reduction, >= 0
}

impl GrMeter {
    pub fn new(width: i32) -> Rc<GrMeter> {
        let area = gtk::DrawingArea::new();
        area.set_content_width(width);
        area.set_content_height((width as f64 * 0.62) as i32);
        area.set_valign(gtk::Align::Center);
        let g = Rc::new(GrMeter { area: area.clone(), pos: Cell::new(0.0) });
        let w = Rc::downgrade(&g);
        area.set_draw_func(move |_, cr, wd, ht| {
            if let Some(g) = w.upgrade() {
                g.draw(cr, wd as f64, ht as f64);
            }
        });
        g
    }

    /// Move toward `gr` dB: fast when reducing more (attack), slower recovery (release).
    pub fn step(&self, gr: f64, attack: f64, release: f64, dt: f64) {
        let cur = self.pos.get();
        let tau = if gr > cur { 0.005 + attack * 0.08 } else { 0.05 + release * 0.6 };
        let (n, moved) = ease_toward(cur, gr, dt, tau, 0.01);
        self.pos.set(n);
        redraw_if(&self.area, moved);
    }

    fn draw(&self, cr: &cairo::Context, w: f64, h: f64) {
        cr.set_source_rgb(0.05, 0.05, 0.055);
        cr.rectangle(0.0, 0.0, w, h);
        let _ = cr.fill();
        let (fx, fy, fw, fh) = (3.0, 3.0, w - 6.0, h - 6.0);
        let face = cairo::LinearGradient::new(0.0, fy, 0.0, fy + fh);
        face.add_color_stop_rgb(0.0, 0.92, 0.9, 0.84);
        face.add_color_stop_rgb(1.0, 0.75, 0.72, 0.64);
        cr.rectangle(fx, fy, fw, fh);
        let _ = cr.set_source(&face);
        let _ = cr.fill();
        let (px, py) = (fx + fw / 2.0, fy + fh * 1.12);
        let rad = fh * 0.92;
        let (a0, a1) = (-PI / 2.0 - 0.75, -PI / 2.0 + 0.75);
        let ang = |gr: f64| a1 - (a1 - a0) * (gr.clamp(0.0, 20.0) / 20.0);
        let _ = cr.save();
        cr.rectangle(fx, fy, fw, fh);
        cr.clip();
        cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
        cr.set_font_size((fh * 0.14).max(7.0));
        cr.set_source_rgb(0.12, 0.1, 0.08);
        for g in [0.0, 3.0, 6.0, 10.0, 20.0] {
            let a = ang(g);
            cr.set_line_width(1.0);
            cr.move_to(px + a.cos() * rad * 0.84, py + a.sin() * rad * 0.84);
            cr.line_to(px + a.cos() * rad * 0.94, py + a.sin() * rad * 0.94);
            let _ = cr.stroke();
            text_centered(cr, px + a.cos() * rad * 1.03, py + a.sin() * rad * 1.03, &format!("{}", g as i32));
        }
        cr.set_font_size((fh * 0.13).max(7.0));
        text_centered(cr, fx + fw * 0.5, fy + fh * 0.72, "GR est.");
        let a = ang(self.pos.get());
        cr.set_line_width(1.3);
        cr.set_source_rgb(0.75, 0.08, 0.05);
        cr.move_to(px + a.cos() * rad * 0.25, py + a.sin() * rad * 0.25);
        cr.line_to(px + a.cos() * rad * 0.97, py + a.sin() * rad * 0.97);
        let _ = cr.stroke();
        let _ = cr.restore();
    }
}

/// Gate status lamp: green OPEN / red SHUT.
pub struct Lamp {
    pub area: gtk::DrawingArea,
    open: Cell<bool>,
    on: Cell<bool>,
}

impl Lamp {
    pub fn new() -> Rc<Lamp> {
        let area = gtk::DrawingArea::new();
        area.set_content_width(52);
        area.set_content_height(40);
        area.set_valign(gtk::Align::Center);
        let l = Rc::new(Lamp { area: area.clone(), open: Cell::new(false), on: Cell::new(false) });
        let w = Rc::downgrade(&l);
        area.set_draw_func(move |_, cr, wd, _| {
            if let Some(l) = w.upgrade() {
                let cx = wd as f64 / 2.0;
                let (r, g, b) = if !l.on.get() { (0.2, 0.2, 0.22) } else if l.open.get() { (0.3, 0.95, 0.45) } else { (0.95, 0.22, 0.16) };
                let glow = cairo::RadialGradient::new(cx, 14.0, 1.0, cx, 14.0, 11.0);
                glow.add_color_stop_rgba(0.0, r, g, b, 1.0);
                glow.add_color_stop_rgba(1.0, r, g, b, 0.0);
                cr.arc(cx, 14.0, 11.0, 0.0, 2.0 * PI);
                let _ = cr.set_source(&glow);
                let _ = cr.fill();
                cr.arc(cx, 14.0, 5.5, 0.0, 2.0 * PI);
                cr.set_source_rgb(r, g, b);
                let _ = cr.fill();
                cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
                cr.set_font_size(8.5);
                cr.set_source_rgba(1.0, 1.0, 1.0, 0.7);
                text_centered(cr, cx, 34.0, if !l.on.get() { "BYPASS" } else if l.open.get() { "OPEN" } else { "SHUT" });
            }
        });
        l
    }

    pub fn set(&self, on: bool, open: bool) {
        if self.on.get() != on || self.open.get() != open {
            self.on.set(on);
            self.open.set(open);
            self.area.queue_draw();
        }
    }
}

// ---------- general needle dial ----------

pub const DIAL_BAND: f64 = 24.0; // label plate under the face: title + value

/// Ink box of `s` centred on (cx, cy), placed exactly as `text_centered` places it.
fn ink_box(cr: &cairo::Context, s: &str, cx: f64, cy: f64) -> [f64; 4] {
    match cr.text_extents(s) {
        Ok(e) => {
            let x0 = cx - e.width() / 2.0;
            let y0 = cy - e.height() / 2.0;
            [x0, y0, x0 + e.width(), y0 + e.height()]
        }
        Err(_) => [cx, cy, cx, cy],
    }
}

/// Labels of a Dial, in draw order: (text, centre x, centre y, font size, ink box). Items 0-4
/// are the five tick labels on the face; item 5 is the title and item 6 the value, both on the
/// plate under the face. Measured with the same font as `Dial::draw`.
pub(crate) fn dial_texts(cr: &cairo::Context, w: f64, h: f64, title: &str, max: f64, value: &str) -> Vec<(String, f64, f64, f64, [f64; 4])> {
    let (fx, fy, fw, fh) = (3.0, 3.0, w - 6.0, h - 6.0 - DIAL_BAND);
    let (px, py) = (fx + fw / 2.0, fy + fh * 1.05);
    let rad = fh * 0.86;
    let (a0, a1) = (-PI / 2.0 - 0.85, -PI / 2.0 + 0.85);
    let ang = |v: f64| a0 + (a1 - a0) * (v / max).clamp(0.0, 1.02);
    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
    let mut out = Vec::with_capacity(7);
    let size = (fh * 0.085).max(7.0);
    cr.set_font_size(size);
    for k in 0..=4 {
        let v = max * k as f64 / 4.0;
        let a = ang(v);
        let t = if max >= 10000.0 { format!("{:.0}k", v / 1000.0) } else { format!("{:.0}", v) };
        let (cx, cy) = (px + a.cos() * rad * 0.72, py + a.sin() * rad * 0.72);
        let b = ink_box(cr, &t, cx, cy);
        out.push((t, cx, cy, size, b));
    }
    // title and value shrink to fit the face width
    let fit = |s: &str, start: f64| {
        let mut sz = start;
        cr.set_font_size(sz);
        while cr.text_extents(s).map(|e| e.width()).unwrap_or(0.0) > fw - 8.0 && sz > 6.0 {
            sz -= 0.5;
            cr.set_font_size(sz);
        }
        sz
    };
    let sz = fit(title, 8.0);
    let (cx, cy) = (px, fy + fh + 2.0 + 7.0);
    let b = ink_box(cr, title, cx, cy);
    out.push((title.to_string(), cx, cy, sz, b));
    let sz = fit(value, 10.0);
    let (cx, cy) = (px, fy + fh + 2.0 + 17.0);
    let b = ink_box(cr, value, cx, cy);
    out.push((value.to_string(), cx, cy, sz, b));
    out
}

/// Amber needle gauge for any value: `max` full scale, red zone from `danger`.
pub struct Dial {
    pub area: gtk::DrawingArea,
    pos: Cell<f64>,
    target: Cell<f64>,
    text: std::cell::RefCell<String>,
}

impl Dial {
    pub fn new(width: i32, title: &'static str, max: f64, danger: f64) -> Rc<Dial> {
        let area = gtk::DrawingArea::new();
        area.set_content_width(width);
        area.set_content_height((width as f64 * 0.66) as i32 + DIAL_BAND as i32);
        let d = Rc::new(Dial { area: area.clone(), pos: Cell::new(0.0), target: Cell::new(0.0), text: std::cell::RefCell::new(String::new()) });
        let w = Rc::downgrade(&d);
        area.set_draw_func(move |_, cr, wd, ht| {
            if let Some(d) = w.upgrade() {
                d.draw(cr, wd as f64, ht as f64, title, max, danger);
            }
        });
        d
    }

    pub fn set(&self, v: f64, text: &str) {
        let changed = *self.text.borrow() != text;
        if changed {
            *self.text.borrow_mut() = text.to_string();
        }
        self.target.set(v);
        redraw_if(&self.area, changed);
    }

    pub fn step(&self, dt: f64) {
        let (n, moved) = ease_toward(self.pos.get(), self.target.get(), dt, 0.25, 0.01);
        self.pos.set(n);
        redraw_if(&self.area, moved);
    }

    fn draw(&self, cr: &cairo::Context, w: f64, h: f64, title: &str, max: f64, danger: f64) {
        cr.set_source_rgb(0.05, 0.05, 0.055);
        cr.rectangle(0.0, 0.0, w, h);
        let _ = cr.fill();
        let (fx, fy, fw, fh) = (3.0, 3.0, w - 6.0, h - 6.0 - DIAL_BAND);
        let face = cairo::RadialGradient::new(fx + fw / 2.0, fy + fh * 0.9, fh * 0.1, fx + fw / 2.0, fy + fh * 0.6, fw * 0.75);
        face.add_color_stop_rgb(0.0, 1.0, 0.86, 0.55);
        face.add_color_stop_rgb(0.6, 0.96, 0.74, 0.36);
        face.add_color_stop_rgb(1.0, 0.6, 0.38, 0.12);
        cr.rectangle(fx, fy, fw, fh);
        let _ = cr.set_source(&face);
        let _ = cr.fill();
        let (px, py) = (fx + fw / 2.0, fy + fh * 1.05);
        let rad = fh * 0.86;
        let (a0, a1) = (-PI / 2.0 - 0.85, -PI / 2.0 + 0.85);
        let ang = |v: f64| a0 + (a1 - a0) * (v / max).clamp(0.0, 1.02);
        let _ = cr.save();
        cr.rectangle(fx, fy, fw, fh);
        cr.clip();
        cr.set_line_width(1.2);
        cr.set_source_rgb(0.12, 0.08, 0.03);
        cr.arc(px, py, rad * 0.86, ang(0.0), ang(danger));
        let _ = cr.stroke();
        cr.set_line_width(3.0);
        cr.set_source_rgb(0.75, 0.1, 0.05);
        cr.arc(px, py, rad * 0.87, ang(danger), ang(max));
        let _ = cr.stroke();
        let items = dial_texts(cr, w, h, title, max, &self.text.borrow());
        cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
        for k in 0..=4 {
            let v = max * k as f64 / 4.0;
            let a = ang(v);
            if v >= danger { cr.set_source_rgb(0.7, 0.08, 0.04) } else { cr.set_source_rgb(0.12, 0.08, 0.03) }
            cr.set_line_width(1.0);
            cr.move_to(px + a.cos() * rad * 0.84, py + a.sin() * rad * 0.84);
            cr.line_to(px + a.cos() * rad * 0.95, py + a.sin() * rad * 0.95);
            let _ = cr.stroke();
        }
        for (k, it) in items.iter().take(5).enumerate() {
            let v = max * k as f64 / 4.0;
            if v >= danger { cr.set_source_rgb(0.7, 0.08, 0.04) } else { cr.set_source_rgb(0.12, 0.08, 0.03) }
            cr.set_font_size(it.3);
            text_centered(cr, it.1, it.2, &it.0);
        }
        let a = ang(self.pos.get());
        cr.set_line_width(1.4);
        cr.set_source_rgb(0.05, 0.05, 0.05);
        cr.move_to(px + a.cos() * rad * 0.25, py + a.sin() * rad * 0.25);
        cr.line_to(px + a.cos() * rad * 0.97, py + a.sin() * rad * 0.97);
        let _ = cr.stroke();
        let _ = cr.restore();
        // label plate under the face, outside the clip: the needle can't cross the text
        cr.set_source_rgb(0.10, 0.09, 0.08);
        cr.rectangle(fx, fy + fh + 2.0, fw, DIAL_BAND - 2.0);
        let _ = cr.fill();
        cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
        let (t, val) = (&items[5], &items[6]);
        cr.set_source_rgb(0.85, 0.70, 0.45);
        cr.set_font_size(t.3);
        text_centered(cr, t.1, t.2, &t.0);
        cr.set_source_rgb(1.0, 0.86, 0.55);
        cr.set_font_size(val.3);
        text_centered(cr, val.1, val.2, &val.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic uniform [0, 1) from an LCG (no `rand` crate).
    fn unit(x: &mut u64) -> f64 {
        *x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (*x >> 11) as f64 / (1u64 << 53) as f64
    }

    #[test]
    fn zone_matches_scarlett_3rd_gen() {
        assert_eq!(zone(-24.0), Zone::Green);
        assert_eq!(zone(-6.01), Zone::Green);
        assert_eq!(zone(-6.0), Zone::Amber);
        assert_eq!(zone(-0.01), Zone::Amber);
        assert_eq!(zone(0.0), Zone::Red);
        assert_eq!(zone(3.0), Zone::Red);
        assert_eq!(zone(SILENT_DB), Zone::Green);
        assert!(AMBER_DB == -6.0 && RED_DB == 0.0);
    }

    #[test]
    fn zone_is_monotonic() {
        for k in 0..=1020 {
            let db = -90.0 + k as f64 * 0.1;
            assert!(zone(db) >= zone(db - 0.1), "db {db}");
        }
    }

    #[test]
    fn peak_held_for_hold_s() {
        assert_eq!(HOLD_S, 3.0);
        let mut b = Ballistics::new();
        b.step(-10.0, 0.016);
        for _ in 0..180 {
            b.step(SILENT_DB, 0.016);
        }
        assert_eq!(b.peak, -10.0);
        assert!(b.level < -10.0);
    }

    #[test]
    fn peak_falls_after_hold() {
        let mut b = Ballistics::new();
        b.step(-10.0, 0.016);
        for _ in 0..250 {
            b.step(SILENT_DB, 0.016);
        }
        assert!(b.peak < -10.0);
        assert!(b.peak >= b.level);
        for _ in 250..1000 {
            b.step(SILENT_DB, 0.016);
        }
        assert_eq!(b.peak, M_MIN);
        assert_eq!(b.level, M_MIN);
    }

    #[test]
    fn higher_peak_restarts_hold() {
        let mut b = Ballistics::new();
        b.step(-20.0, 0.016);
        for _ in 0..100 {
            b.step(SILENT_DB, 0.016);
        }
        b.step(-10.0, 0.016);
        assert_eq!(b.peak, -10.0);
        assert_eq!(b.hold, HOLD_S);
    }

    #[test]
    fn release_is_24_db_per_s() {
        let mut b = Ballistics::new();
        b.step(-10.0, 0.016);
        for _ in 0..50 {
            b.step(M_MIN, 0.01);
        }
        assert!((b.level - -22.0).abs() < 1e-9, "level {}", b.level);
    }

    #[test]
    fn step_reports_motion_only() {
        let mut b = Ballistics::new();
        for _ in 0..1000 {
            b.step(SILENT_DB, 0.016);
        }
        assert!(!b.step(SILENT_DB, 0.016));
        assert!(b.step(-10.0, 0.016));
    }

    #[test]
    fn ballistics_invariant_under_random_feed() {
        let mut x: u64 = 1;
        let mut b = Ballistics::new();
        let mut since = 0.0_f64; // time since the peak was last set
        let mut silent_left = 0u32;
        for i in 0..100_000u32 {
            let v = if silent_left > 0 {
                silent_left -= 1;
                SILENT_DB
            } else if i % 50 == 0 {
                silent_left = 300;
                SILENT_DB
            } else {
                -90.0 + 102.0 * unit(&mut x)
            };
            let dt = 0.001 + 0.049 * unit(&mut x);
            let prev_peak = b.peak;
            let vc = v.min(M_MAX);
            b.step(v, dt);
            assert!(b.level >= M_MIN && b.level <= M_MAX, "step {i}: level {}", b.level);
            assert!(b.level <= b.peak && b.peak <= M_MAX, "step {i}: peak {} level {}", b.peak, b.level);
            if vc >= prev_peak {
                assert_eq!(b.peak, vc, "step {i}: a higher reading must set the peak");
                since = 0.0;
            } else {
                since += dt;
            }
            if since < HOLD_S {
                assert!(b.peak >= prev_peak, "step {i}: peak fell {prev_peak} -> {} inside its hold", b.peak);
            }
        }
    }

    #[test]
    fn send_db_math() {
        assert_eq!(send_db(-10.0, true, -6.0), -16.0);
        assert_eq!(send_db(-10.0, false, 0.0), SILENT_DB);
        assert_eq!(send_db(-10.0, true, crate::config::MIN_DB), SILENT_DB);
        assert_eq!(send_db(-3.0, true, 6.0), 3.0);
        assert_eq!(zone(send_db(-3.0, true, 6.0)), Zone::Red);
    }

    #[test]
    fn send_level_db_is_amplitude_db() {
        for d in [-50.0, -20.0, -6.0, 0.0, 6.0] {
            let v = crate::config::db_to_vol(d).powi(3);
            assert!((v - 10f64.powf(d / 20.0)).abs() < 1e-9, "d {d}");
        }
    }
}

#[cfg(test)]
mod redraw_tests {
    use super::*;

    /// Deterministic uniform [0, 1) from an LCG (no `rand` crate).
    fn unit(x: &mut u64) -> f64 {
        *x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (*x >> 11) as f64 / (1u64 << 53) as f64
    }
    fn uni(x: &mut u64, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * unit(x)
    }
    /// Frame dt in the range FrameCap emits at 60-240 Hz.
    fn frame_dt(x: &mut u64) -> f64 {
        uni(x, 1.0 / 40.0, 1.0 / 27.0)
    }

    #[test]
    fn frame_cap_rates() {
        for hz in [60.0_f64, 75.0, 120.0, 144.0, 165.0, 240.0] {
            let cap = FrameCap::new();
            let dt = 1.0 / hz;
            let (mut n, mut sum) = (0usize, 0.0_f64);
            for _ in 0..(10.0 / dt).round() as usize {
                if let Some(d) = cap.tick(dt) {
                    n += 1;
                    sum += d;
                }
            }
            assert!((270..=300).contains(&n), "{hz} Hz: {n} emits in 10 s");
            // time still accumulating after the last emit is never returned, so allow one emit period
            assert!((sum - 10.0).abs() <= 1.0 / METER_FPS + 1e-9, "{hz} Hz: summed dt {sum}");
        }
        let cap = FrameCap::new();
        assert_eq!(cap.tick(0.5), Some(0.1));
    }

    fn settle_vu(x: &mut u64, case: usize) {
        let (mut pos, mut vel) = (VU_MIN, 0.0_f64);
        for _ in 0..50 {
            let db = uni(x, -90.0, 6.0);
            let (p, v, _) = vu_spring(pos, vel, db, frame_dt(x));
            pos = p;
            vel = v;
        }
        let hold = uni(x, -30.0, -12.0);
        for _ in 0..300 {
            let (p, v, _) = vu_spring(pos, vel, hold, frame_dt(x));
            pos = p;
            vel = v;
        }
        for _ in 0..30 {
            let (p, v, moved) = vu_spring(pos, vel, hold, frame_dt(x));
            assert!(!moved, "case {case}: vu moved while settled");
            pos = p;
            vel = v;
        }
        let (_, _, moved) = vu_spring(pos, vel, hold + 10.0, frame_dt(x));
        assert!(moved, "case {case}: vu did not wake on +10 dB");
    }

    fn random_bands(x: &mut u64) -> Vec<f32> {
        (0..BANDS).map(|_| uni(x, 0.0, 1.0) as f32).collect()
    }

    fn settle_bands(x: &mut u64, case: usize) {
        let mut shown = vec![0.0f64; BANDS];
        for _ in 0..50 {
            let src = random_bands(x);
            smooth_bands(&mut shown, &src, frame_dt(x));
        }
        let src = random_bands(x);
        for _ in 0..300 {
            smooth_bands(&mut shown, &src, frame_dt(x));
        }
        for _ in 0..30 {
            assert!(!smooth_bands(&mut shown, &src, frame_dt(x)), "case {case}: bands moved while settled");
        }
        let mut bumped = src.clone();
        let k = (unit(x) * BANDS as f64) as usize % BANDS;
        bumped[k] += 0.2;
        assert!(smooth_bands(&mut shown, &bumped, frame_dt(x)), "case {case}: band {k} did not wake");
    }

    fn settle_spectrum(x: &mut u64, case: usize) {
        let mut shown = vec![0.0f64; BANDS];
        let mut peaks = vec![(0.0f64, 0.0f64); BANDS];
        for _ in 0..50 {
            let src = random_bands(x);
            smooth_spectrum(&mut shown, &mut peaks, &src, frame_dt(x));
        }
        let src = random_bands(x);
        for _ in 0..300 {
            smooth_spectrum(&mut shown, &mut peaks, &src, frame_dt(x));
        }
        for _ in 0..30 {
            assert!(!smooth_spectrum(&mut shown, &mut peaks, &src, frame_dt(x)), "case {case}: spectrum moved while settled");
        }
        let mut bumped = src.clone();
        let k = (unit(x) * BANDS as f64) as usize % BANDS;
        bumped[k] += 0.2;
        assert!(smooth_spectrum(&mut shown, &mut peaks, &bumped, frame_dt(x)), "case {case}: spectrum band {k} did not wake");
    }

    fn settle_ease(x: &mut u64, case: usize, hi: f64, tau_lo: f64, tau_hi: f64) {
        let tau = uni(x, tau_lo, tau_hi);
        let mut cur = uni(x, 0.0, hi);
        for _ in 0..50 {
            let t = uni(x, 0.0, hi);
            cur = ease_toward(cur, t, frame_dt(x), tau, 0.01).0;
        }
        let target = uni(x, 0.0, hi);
        for _ in 0..300 {
            cur = ease_toward(cur, target, frame_dt(x), tau, 0.01).0;
        }
        for _ in 0..30 {
            let (n, moved) = ease_toward(cur, target, frame_dt(x), tau, 0.01);
            assert!(!moved, "case {case}: ease moved while settled");
            cur = n;
        }
        let (_, moved) = ease_toward(cur, target + 1.0, frame_dt(x), tau, 0.01);
        assert!(moved, "case {case}: ease did not wake on +1.0");
    }

    #[test]
    fn widgets_settle_and_wake() {
        let mut x: u64 = 0x5eed;
        for case in 0..500 {
            match (unit(&mut x) * 5.0) as u32 {
                0 => settle_vu(&mut x, case),
                1 => settle_bands(&mut x, case),
                2 => settle_spectrum(&mut x, case),
                3 => settle_ease(&mut x, case, 1024.0, 0.25, 0.25),
                _ => settle_ease(&mut x, case, 30.0, 0.005, 0.65),
            }
        }
    }

    #[test]
    fn dial_text_no_overlap() {
        let w = 124.0_f64;
        for (title, max, value) in [("AUDIO CPU", 100.0, "888% of 1 core"), ("AUDIO CPU", 100.0, "0% of 1 core"), ("PIPEWIRE FILES", 1024.0, "1024 / 1024")] {
            let hi = (w * 0.66) as i32 + DIAL_BAND as i32;
            let h = hi as f64;
            let surf = cairo::ImageSurface::create(cairo::Format::ARgb32, 124, hi).unwrap();
            let cr = cairo::Context::new(&surf).unwrap();
            let items = dial_texts(&cr, w, h, title, max, value);
            assert_eq!(items.len(), 7);
            // the title's ink must be real, so this check cannot pass with no font loaded
            let t: [f64; 4] = items[5].4;
            assert!(t[2] - t[0] > 20.0, "{title}: title ink width {}", t[2] - t[0]);
            for (i, a) in items.iter().enumerate() {
                let r = a.4;
                assert!(r[0] >= 0.0 && r[1] >= 0.0 && r[2] <= w && r[3] <= h, "item {i} outside the dial: {r:?}");
                for (j, b) in items.iter().enumerate().skip(i + 1) {
                    let s = b.4;
                    let apart = r[2] + 1.0 <= s[0] || s[2] + 1.0 <= r[0] || r[3] + 1.0 <= s[1] || s[3] + 1.0 <= r[1];
                    assert!(apart, "{value}: items {i} and {j} overlap: {r:?} {s:?}");
                }
            }
            for k in [5, 6] {
                assert!(items[k].4[1] >= 3.0 + (h - 6.0 - DIAL_BAND), "item {k} sits above the plate: {:?}", items[k].4);
            }
        }
    }

    #[test]
    fn cpu_dial_unit_is_one_core() {
        let ui = include_str!("ui.rs");
        assert!(!ui.contains("AUDIO CPU %"), "the dial still says AUDIO CPU %");
        assert!(ui.contains("% of 1 core"));
    }
}
