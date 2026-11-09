//! Live piano (#23): a passive `pw-cat` tap -> `PianoTracker` -> the shared roll, and optionally the
//! "Wavelink Piano" ALSA sequencer port for other software. It adds no engine graph nodes and plays
//! nothing: the tap is `node.passive=true`, the same one `wavelink-mixer stems --source` uses.
use crate::notes::Note;
use crate::piano::{self, lane, Downmix, Ev, PianoTracker};
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// ALSA client and port names other software subscribes to.
pub const CLIENT_NAME: &str = "Wavelink Piano";
pub const PORT_NAME: &str = "Piano";
/// The tap's PipeWire node name.
pub const TAP_NAME: &str = "piano_tap";
/// Rate and channels of the tap (`stems::tap_args`).
const TAP_RATE: usize = 44100;
/// Notes older than this are dropped from the shared roll (Save MIDI keeps its own full list).
const ROLL_KEEP_S: f64 = 30.0;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// MIDI channel (0-based) for a pitch: Bass / Mid / High lanes on channels 1 / 2 / 3.
pub fn channel(pitch: u8) -> u8 {
    lane(pitch).index() as u8
}

/// Where MIDI goes. The ALSA port implements it; tests use a recorder.
pub trait Sink {
    fn note(&mut self, on: bool, ch: u8, key: u8, vel: u8) -> Result<(), String>;
    fn control(&mut self, ch: u8, cc: u8, val: u8) -> Result<(), String>;
}

/// Turns tracker events into MIDI on a sink and remembers what is sounding, so stop, a source change
/// and drop can silence exactly those notes (plus an All Notes Off on the lane channels).
pub struct MidiOut<S: Sink> {
    pub sink: S,
    sounding: Vec<(u8, u8)>,
    /// Note-offs (ch, key) the sink refused (a full non-blocking queue): retried on every later
    /// event and on `flush`, and resent by `all_off`, so a note-off is never lost.
    pending_off: Vec<(u8, u8)>,
    /// Channels whose CC 123 still has to go out.
    pending_cc: Vec<u8>,
    /// Note-ons held back because their key still had an undelivered note-off: they go out after
    /// it, so a stale off can never cut the newer note (R3-F6).
    pending_on: Vec<(u8, u8, u8)>,
}

impl<S: Sink> MidiOut<S> {
    pub fn new(sink: S) -> Self {
        MidiOut { sink, sounding: Vec::new(), pending_off: Vec::new(), pending_cc: Vec::new(), pending_on: Vec::new() }
    }

    fn off(&mut self, ch: u8, key: u8) {
        if self.sink.note(false, ch, key, 0).is_err() && !self.pending_off.contains(&(ch, key)) {
            self.pending_off.push((ch, key));
        }
    }

    /// Retries what the sink refused earlier. Never sleeps.
    pub fn flush(&mut self) {
        for (ch, key) in std::mem::take(&mut self.pending_off) {
            if self.sink.note(false, ch, key, 0).is_err() {
                self.pending_off.push((ch, key));
            }
        }
        for ch in std::mem::take(&mut self.pending_cc) {
            if self.sink.control(ch, 123, 0).is_err() {
                self.pending_cc.push(ch);
            }
        }
        // held-back note-ons whose key's off is out now
        for (ch, key, vel) in std::mem::take(&mut self.pending_on) {
            if self.pending_off.contains(&(ch, key)) || self.sink.note(true, ch, key, vel).is_err() {
                self.pending_on.push((ch, key, vel));
            }
        }
    }

    /// Sends a note-on unless an older note-off of the same key is still undelivered: then that off
    /// goes first, and if it is refused again the on waits behind it.
    fn on(&mut self, ch: u8, key: u8, vel: u8) {
        if let Some(i) = self.pending_off.iter().position(|x| *x == (ch, key)) {
            if self.sink.note(false, ch, key, 0).is_err() {
                self.pending_on.retain(|x| (x.0, x.1) != (ch, key));
                self.pending_on.push((ch, key, vel));
                return;
            }
            self.pending_off.swap_remove(i);
        }
        // a refused note-on is just dropped: its later note-off is harmless
        let _ = self.sink.note(true, ch, key, vel);
    }

    pub fn event(&mut self, e: &Ev) {
        self.flush();
        match *e {
            Ev::On { pitch, vel, .. } => {
                let ch = channel(pitch);
                if self.sounding.contains(&(ch, pitch)) {
                    self.off(ch, pitch);
                } else {
                    self.sounding.push((ch, pitch));
                }
                self.on(ch, pitch, vel.clamp(1, 127));
            }
            Ev::Off { pitch, .. } => {
                let ch = channel(pitch);
                if let Some(i) = self.sounding.iter().position(|x| *x == (ch, pitch)) {
                    self.sounding.swap_remove(i);
                    if let Some(j) = self.pending_on.iter().position(|x| (x.0, x.1) == (ch, pitch)) {
                        // its on never went out: nothing to turn off
                        self.pending_on.swap_remove(j);
                    } else {
                        self.off(ch, pitch);
                    }
                }
            }
            Ev::Pedal { down, .. } => {
                // the lanes' channels; a refused pedal is dropped (the next change corrects it)
                for ch in 0..3u8 {
                    let _ = self.sink.control(ch, 64, if down { 127 } else { 0 });
                }
            }
        }
    }

    #[cfg(test)]
    pub fn sounding(&self) -> usize {
        self.sounding.len()
    }

    #[cfg(test)]
    pub fn pending(&self) -> usize {
        self.pending_off.len() + self.pending_cc.len()
    }

    /// Note-off for every sounding note and every earlier refused one, then CC 123 (All Notes Off)
    /// on the three lane channels. What the sink refuses stays queued for `flush`.
    pub fn all_off(&mut self) {
        self.flush();
        self.pending_on.clear();
        for (ch, key) in std::mem::take(&mut self.sounding) {
            self.off(ch, key);
        }
        for ch in 0..3 {
            if self.sink.control(ch, 123, 0).is_err() && !self.pending_cc.contains(&ch) {
                self.pending_cc.push(ch);
            }
        }
    }
}

impl<S: Sink> Drop for MidiOut<S> {
    /// All notes off; what the sink still refuses gets a few short retries (here, off the analysis
    /// thread's hot path, sleeping is fine).
    fn drop(&mut self) {
        self.all_off();
        for _ in 0..20 {
            if self.pending_off.is_empty() && self.pending_cc.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
            self.flush();
        }
    }
}

/// The "Wavelink Piano" ALSA sequencer client with one readable, subscribable port.
pub struct AlsaSink {
    seq: alsa::Seq,
    port: i32,
}

impl AlsaSink {
    /// Opens the client non-blocking: a slow subscriber can never stall the analysis thread.
    pub fn open() -> Result<AlsaSink, String> {
        use alsa::seq::{PortCap, PortType};
        use std::ffi::CString;
        let seq = alsa::Seq::open(None, Some(alsa::Direction::Playback), true).map_err(|e| format!("ALSA sequencer: {e}"))?;
        seq.set_client_name(&CString::new(CLIENT_NAME).unwrap()).map_err(|e| format!("ALSA client name: {e}"))?;
        let port = seq
            .create_simple_port(
                &CString::new(PORT_NAME).unwrap(),
                PortCap::READ | PortCap::SUBS_READ,
                PortType::MIDI_GENERIC | PortType::APPLICATION,
            )
            .map_err(|e| format!("ALSA port: {e}"))?;
        Ok(AlsaSink { seq, port })
    }

    /// "client:port", as `aconnect -l` shows it.
    pub fn address(&self) -> String {
        format!("{}:{}", self.seq.client_id().unwrap_or(-1), self.port)
    }

    /// Sends one event, once: a full output queue (EAGAIN on the non-blocking client) is an error
    /// the caller queues and retries later, so the analysis thread never sleeps holding the port.
    fn send(&self, mut ev: alsa::seq::Event) -> Result<(), String> {
        ev.set_source(self.port);
        ev.set_subs();
        ev.set_direct();
        self.seq.event_output_direct(&mut ev).map(|_| ()).map_err(|e| e.to_string())
    }
}

impl Sink for AlsaSink {
    fn note(&mut self, on: bool, ch: u8, key: u8, vel: u8) -> Result<(), String> {
        use alsa::seq::{EvNote, Event, EventType};
        let d = EvNote { channel: ch, note: key, velocity: if on { vel } else { 0 }, off_velocity: 0, duration: 0 };
        self.send(Event::new(if on { EventType::Noteon } else { EventType::Noteoff }, &d))
    }
    fn control(&mut self, ch: u8, cc: u8, val: u8) -> Result<(), String> {
        use alsa::seq::{EvCtrl, Event, EventType};
        self.send(Event::new(EventType::Controller, &EvCtrl { channel: ch, param: cc as u32, value: val as i32 }))
    }
}

/// The long-lived MIDI port slot: one "Wavelink Piano" client for the life of the panel or CLI run,
/// shared by every session, so a source change or restart never changes its client:port address.
/// The panel's "Live MIDI port" toggle fills or empties the slot; a running session follows it.
pub type Port = Arc<Mutex<Option<MidiOut<AlsaSink>>>>;

pub fn open_port() -> Result<Port, String> {
    Ok(Arc::new(Mutex::new(Some(MidiOut::new(AlsaSink::open()?)))))
}

/// "client:port" of the port in the slot, if any.
pub fn port_address(p: &Port) -> Option<String> {
    lock(p).as_ref().map(|m| m.sink.address())
}

/// What the UI and the CLI read. It outlives sessions: a restart keeps the roll and the take.
#[derive(Default)]
pub struct Inner {
    /// Finished notes, last ROLL_KEEP_S seconds, times on the take clock.
    pub roll: Vec<Note>,
    /// Every finished note of the take not yet handed out by `take_notes`.
    pub all: Vec<Note>,
    /// Sounding notes: (start, pitch, vel, already handed out by `take_notes`).
    pub open: Vec<(f64, u8, u8, bool)>,
    /// Take clock: seconds of audio analysed across all sessions of this take.
    pub now: f64,
    /// Where the current session's own clock starts on the take clock.
    pub base: f64,
    pub status: String,
    /// CPU per 0.5 s of audio, ms (smoothed).
    pub cpu_ms: f64,
    /// Attack -> NoteOn on the analysis stream, ms (smoothed over recent notes).
    pub latency_ms: f64,
    pub port: Option<String>,
    /// The arrangement's parts: name and colour. Index 0 is "Piano" (what the live tracker plays).
    pub parts: Vec<(String, (u8, u8, u8))>,
    /// Part index of each note in `roll` (same length and order as `roll`).
    pub roll_part: Vec<u8>,
    /// Part index of each sounding note in `open` (same length and order as `open`).
    pub open_part: Vec<u8>,
    /// Per-key spectrogram, last SPEC_KEEP_S seconds: (take time, 88 values 0..1, A0..C8), the
    /// tracker's harmonic salience on the whitened spectrum, square-root scaled against a peak
    /// that falls 3 dB/s.
    pub spec: VecDeque<(f64, Vec<f32>)>,
    /// What the subtraction kept, same layout as `spec`: per key the amplitude of the note found
    /// there (0..1 over 40 dB under the running loudest), 0 where none was found. Empty with the
    /// old tracker core.
    pub found: VecDeque<(f64, Vec<f32>)>,
    /// "notes" (transcription, the default) or "talking" (spectral resynthesis); read when a
    /// session starts. MIXER_PIANO_MODE overrides.
    pub mode: String,
    /// The detected chord right now, e.g. "F", "Bbmaj7", "Dm7"; empty when there is none.
    pub chord: String,
}

/// Seconds of `Inner::spec` kept.
pub const SPEC_KEEP_S: f64 = 8.0;

/// The live tracker's single part.
pub fn default_parts() -> Vec<(String, (u8, u8, u8))> {
    vec![("Piano".to_string(), (0xc8, 0xd6, 0xe5))]
}

#[derive(Clone)]
pub struct Shared(pub Arc<Mutex<Inner>>);

impl Default for Shared {
    fn default() -> Self {
        Shared(Arc::new(Mutex::new(Inner { parts: default_parts(), ..Inner::default() })))
    }
}

impl Shared {
    pub fn new() -> Shared {
        Shared::default()
    }
    pub fn get(&self) -> MutexGuard<'_, Inner> {
        lock(&self.0)
    }
}

impl Inner {
    /// Applies tracker events (times on the session clock). `fed` is the session clock when they
    /// were returned.
    pub fn apply(&mut self, evs: &[Ev], fed: f64) {
        let b = self.base;
        for e in evs {
            match *e {
                Ev::On { t, pitch, vel } => {
                    self.close(pitch, t + b);
                    self.open.push((t + b, pitch, vel, false));
                    self.open_part.push(0);
                    let lat = (fed - t) * 1000.0;
                    self.latency_ms = if self.latency_ms == 0.0 { lat } else { self.latency_ms * 0.9 + lat * 0.1 };
                }
                Ev::Off { t, pitch } => self.close(pitch, t + b),
                Ev::Pedal { .. } => {}
            }
        }
        self.now = fed + b;
        let keep = self.now - ROLL_KEEP_S;
        if self.roll.iter().any(|n| n.t + n.dur < keep) {
            let parts = std::mem::take(&mut self.roll_part);
            let mut it = parts.into_iter();
            let mut kept = Vec::with_capacity(self.roll.len());
            self.roll.retain(|n| {
                let part = it.next().unwrap_or(0);
                let k = n.t + n.dur >= keep;
                if k {
                    kept.push(part);
                }
                k
            });
            self.roll_part = kept;
        }
    }

    fn close(&mut self, pitch: u8, t: f64) {
        if let Some(i) = self.open.iter().position(|o| o.1 == pitch) {
            let (t0, p, v, handed) = self.open.swap_remove(i);
            let part = if i < self.open_part.len() { self.open_part.swap_remove(i) } else { 0 };
            let n = Note { t: t0, dur: (t - t0).max(0.0), pitch: p, vel: v };
            self.roll.push(n.clone());
            self.roll_part.push(part);
            if !handed {
                self.all.push(n);
            }
        }
    }

    fn sorted(mut v: Vec<Note>) -> Vec<Note> {
        v.sort_by(|a, b| a.t.total_cmp(&b.t).then(a.pitch.cmp(&b.pitch)));
        v
    }

    /// "New take": forget the finished notes and the roll; notes still sounding go into the new take.
    pub fn new_take(&mut self) {
        self.all.clear();
        self.roll.clear();
        self.roll_part.clear();
        self.spec.clear();
        self.found.clear();
        for o in self.open.iter_mut() {
            o.3 = false;
        }
    }

    /// Save MIDI: a copy of the whole take, with sounding notes cut at `now` in the copy only.
    pub fn snapshot(&self) -> Vec<Note> {
        let mut v = self.all.clone();
        v.extend(self.open.iter().filter(|o| !o.3).map(|&(t, p, vel, _)| Note { t, dur: (self.now - t).max(0.0), pitch: p, vel }));
        Self::sorted(v)
    }

    /// "Since the last call": hands out the finished notes and the sounding ones cut at `now`; a cut
    /// note is marked so it is never handed out again when it closes.
    pub fn take_notes(&mut self) -> Vec<Note> {
        let mut v = std::mem::take(&mut self.all);
        let now = self.now;
        for o in self.open.iter_mut().filter(|o| !o.3) {
            v.push(Note { t: o.0, dur: (now - o.0).max(0.0), pitch: o.1, vel: o.2 });
            o.3 = true;
        }
        Self::sorted(v)
    }
}

pub struct Opts {
    /// PipeWire node to tap and whether it is a sink (`stemcfg::tap`).
    pub tap: String,
    pub tap_sink: bool,
    /// Open a port for this session only (the CLI and tests; the panel passes its long-lived one).
    pub port: bool,
    /// Replay this WAV at real-time pace instead of tapping (MIXER_PIANO_WAV: UI drives in test mode).
    pub wav: Option<std::path::PathBuf>,
}

/// One live run. Drop stops it: the stop flag, the pw-cat child (ours, by handle) is killed and
/// reaped, and the worker is joined (bounded at 500 ms), so by the time drop returns every sounding
/// note got its note-off on the port and nothing writes the shared state any more.
pub struct Session {
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    done: Option<std::sync::mpsc::Receiver<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Session {
    #[cfg(test)]
    pub fn start(o: Opts, shared: Shared) -> Session {
        Self::start_with(o, shared, None)
    }

    /// `port` is a long-lived port to send to; with None, `o.port` opens one for this session.
    pub fn start_with(o: Opts, shared: Shared, port: Option<Port>) -> Session {
        let stop = Arc::new(AtomicBool::new(false));
        let child: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
        {
            // the take continues: the new session's clock starts where the last one ended
            let mut g = shared.get();
            g.base = g.now;
            g.open.clear();
            g.open_part.clear();
            g.status = "starting…".into();
        }
        let (s2, c2) = (stop.clone(), child.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            if let Err(e) = run(&o, &shared, &s2, &c2, port) {
                if !s2.load(Ordering::Relaxed) {
                    shared.get().status = format!("error: {e}");
                }
            }
            let _ = tx.send(());
        });
        Session { stop, child, done: Some(rx), worker: Some(worker) }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(mut c) = lock(&self.child).take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        let finished = self.done.take().map(|rx| rx.recv_timeout(Duration::from_millis(500)).is_ok()).unwrap_or(true);
        if finished {
            if let Some(h) = self.worker.take() {
                let _ = h.join();
            }
        }
    }
}

fn spawn_tap(o: &Opts) -> Result<Child, String> {
    let mut cmd = Command::new(crate::stems::TAP_BIN);
    cmd.args(crate::stems::tap_args(&o.tap, o.tap_sink, TAP_NAME)).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    crate::pw::die_with_parent(&mut cmd);
    cmd.spawn().map_err(|e| format!("{}: {e}", crate::stems::TAP_BIN))
}

/// Appends a row to a per-key ring, reusing the row that falls out of the SPEC_KEEP_S window.
fn push_ring(ring: &mut VecDeque<(f64, Vec<f32>)>, now: f64, vals: &[f32]) {
    let mut row = match ring.front() {
        Some(&(t, _)) if now - t > SPEC_KEEP_S => ring.pop_front().map(|x| x.1).unwrap_or_default(),
        _ => Vec::with_capacity(vals.len()),
    };
    row.clear();
    row.extend_from_slice(vals);
    ring.push_back((now, row));
    while ring.front().is_some_and(|&(t, _)| now - t > SPEC_KEEP_S + 0.5) {
        ring.pop_front();
    }
}

/// The live note source: the subtraction core (default) or the old tracker (MIXER_PIANO_CORE=tracker).
enum Core {
    Sub(Box<crate::subtract::SubTracker>),
    Tracker(Box<PianoTracker>),
    /// The talking piano: spectral resynthesis, no pianist layer.
    Spectral(Box<crate::spectral::Spectral>),
}

impl Core {
    fn new(mode: &str) -> Core {
        let mode = std::env::var("MIXER_PIANO_MODE").unwrap_or_else(|_| mode.to_string());
        if mode == "talking" {
            return Core::Spectral(Box::new(crate::spectral::Spectral::new()));
        }
        if std::env::var("MIXER_PIANO_CORE").as_deref() == Ok("tracker") {
            Core::Tracker(Box::new(PianoTracker::new()))
        } else {
            Core::Sub(Box::new(crate::subtract::SubTracker::new()))
        }
    }
    fn push(&mut self, x: &[f32]) -> Vec<Ev> {
        match self {
            Core::Sub(s) => s.push(x),
            Core::Tracker(t) => t.push(x),
            Core::Spectral(s) => s.push(x),
        }
    }
    fn now(&self) -> f64 {
        match self {
            Core::Sub(s) => s.now(),
            Core::Tracker(t) => t.now(),
            Core::Spectral(s) => s.now(),
        }
    }
    fn all_off(&mut self, t: f64) -> Vec<Ev> {
        match self {
            Core::Sub(s) => s.all_off(t),
            Core::Tracker(tr) => tr.all_off(t),
            Core::Spectral(s) => s.all_off(t),
        }
    }
}

/// Per-session state shared by the tap and replay loops.
struct Worker<'a> {
    shared: &'a Shared,
    port: Option<Port>,
    tr: Core,
    pianist: crate::pianist::Pianist,
    /// The spectrogram's running peak (falls 3 dB/s).
    spec_peak: f32,
    audio_s: f64,
    busy_s: f64,
}

impl Worker<'_> {
    /// Runs mono samples at RATE through the note core, the port and the shared roll.
    fn feed(&mut self, mono: &[f32], busy: Duration) {
        let t = Instant::now();
        let raw = self.tr.push(mono);
        let mut evs = Vec::with_capacity(raw.len() + 4);
        if matches!(self.tr, Core::Spectral(_)) {
            evs.extend_from_slice(&raw);
        } else {
            self.pianist.push(&raw, self.tr.now(), &mut evs);
        }
        self.busy_s += (t.elapsed() + busy).as_secs_f64();
        self.audio_s += mono.len() as f64 / piano::RATE as f64;
        if let Some(p) = &self.port {
            if let Some(m) = lock(p).as_mut() {
                m.flush();
                for e in &evs {
                    m.event(e);
                }
            }
        }
        let dt = mono.len() as f32 / piano::RATE as f32;
        let mut raw_row = [0f32; piano::KEYS];
        let mut found_row: Option<[f32; piano::KEYS]> = None;
        match &self.tr {
            Core::Sub(st) => {
                raw_row = st.raw_view;
                found_row = Some(st.found_view);
            }
            Core::Spectral(sp) => {
                raw_row = sp.last_view;
            }
            Core::Tracker(tr) => {
                let sal = tr.key_salience();
                let mx = sal.iter().fold(0f32, |a, &v| a.max(v));
                self.spec_peak = (self.spec_peak * 10f32.powf(-0.15 * dt)).max(mx).max(1e-6);
                for (o, &v) in raw_row.iter_mut().zip(sal) {
                    *o = (v.max(0.0) / self.spec_peak).sqrt().min(1.0);
                }
            }
        }
        let mut g = self.shared.get();
        g.apply(&evs, self.tr.now());
        let now = g.now;
        push_ring(&mut g.spec, now, &raw_row);
        if let Some(f) = found_row {
            push_ring(&mut g.found, now, &f);
        }
        if self.audio_s >= 0.5 {
            let ms = self.busy_s * 1000.0 / self.audio_s * 0.5;
            g.cpu_ms = if g.cpu_ms == 0.0 { ms } else { g.cpu_ms * 0.8 + ms * 0.2 };
            self.audio_s = 0.0;
            self.busy_s = 0.0;
        }
    }

    /// End of the session: every sounding note off on the port and in the roll.
    fn finish(&mut self) {
        let end = self.tr.now();
        let raw = self.tr.all_off(end);
        let mut evs = Vec::with_capacity(raw.len() + 8);
        self.pianist.push(&raw, end, &mut evs);
        self.pianist.finish(end, &mut evs);
        if let Some(p) = &self.port {
            if let Some(m) = lock(p).as_mut() {
                m.all_off();
            }
        }
        self.shared.get().apply(&evs, end);
    }
}

fn run(o: &Opts, shared: &Shared, stop: &AtomicBool, child: &Mutex<Option<Child>>, port: Option<Port>) -> Result<(), String> {
    let port = match port {
        Some(p) => Some(p),
        None if o.port && !crate::dash::no_audio() => Some(open_port()?),
        None => None,
    };
    shared.get().port = port.as_ref().and_then(port_address);
    let mut w = Worker { shared, port, tr: Core::new(&shared.get().mode.clone()), pianist: crate::pianist::Pianist::new(), spec_peak: 0.0, audio_s: 0.0, busy_s: 0.0 };
    if let Some(path) = &o.wav {
        let r = replay(path, &mut w, stop);
        w.finish();
        if r.is_ok() {
            shared.get().status = "replay finished".into();
        }
        return r;
    }
    let mut stdout = {
        let mut g = lock(child);
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let mut c = spawn_tap(o)?;
        let so = c.stdout.take().ok_or("tap: no stdout")?;
        *g = Some(c);
        so
    };
    shared.get().status = format!("listening to {}", o.tap);
    let mut dm = Downmix::new();
    let mut raw = vec![0u8; 4 * 2 * 441]; // 10 ms of f32 stereo
    let mut carry: Vec<u8> = Vec::new();
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let n = match stdout.read(&mut raw) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        carry.extend_from_slice(&raw[..n]);
        let whole = carry.len() / 8 * 8;
        let stereo: Vec<f32> = carry[..whole].chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
        carry.drain(..whole);
        let t = Instant::now();
        let mono = dm.push(&stereo);
        let busy = t.elapsed();
        w.feed(&mono, busy);
    }
    w.finish();
    Ok(())
}

/// Feeds a WAV through the tracker in 10 ms steps at real-time pace. Reads a file, plays nothing.
fn replay(path: &std::path::Path, w: &mut Worker, stop: &AtomicBool) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let (pcm, rate) = piano::read_wav(&bytes)?;
    let x = piano::to_rate(&pcm, rate);
    w.shared.get().status = format!("replaying {}", path.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default());
    let step = piano::RATE as usize / 100;
    let t0 = Instant::now();
    for (i, c) in x.chunks(step).enumerate() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let due = t0 + Duration::from_secs_f64(i as f64 * 0.01);
        if let Some(d) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(d);
        }
        w.feed(c, Duration::ZERO);
    }
    Ok(())
}

fn usage(msg: &str) -> i32 {
    eprintln!("piano: {msg}");
    eprintln!("usage: piano --wav IN.wav [--out OUT.mid] [--merged] [--ref REF.mid]");
    eprintln!("       piano --source ID [--port] [--seconds N] [--out OUT.mid] [--merged]");
    eprintln!("       piano --bench");
    2
}

fn name(p: u8) -> String {
    const N: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
    format!("{}{}", N[(p % 12) as usize], p as i32 / 12 - 1)
}

/// `wavelink-mixer piano ...`
pub fn cli(args: Vec<String>) -> i32 {
    let (mut wav, mut out, mut reference, mut source) = (None::<String>, None::<String>, None::<String>, None::<String>);
    let (mut merged, mut port, mut bench, mut seconds) = (false, false, false, None::<f64>);
    let mut arrange = None::<bool>;
    let (mut subtract, mut vocal) = (false, None::<String>);
    let mut v3 = None::<bool>;
    let mut drums = None::<String>;
    let mut eval = None::<String>;
    let mut spectral = false;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--wav" => wav = it.next(),
            "--out" => out = it.next(),
            "--ref" => reference = it.next(),
            "--source" => source = it.next(),
            "--seconds" => seconds = it.next().and_then(|v| v.parse().ok()),
            "--merged" => merged = true,
            "--port" => port = true,
            "--bench" => bench = true,
            "--arrange" => arrange = Some(false),
            "--piano-only" => arrange = Some(true),
            "--subtract" => subtract = true,
            "--arrange3" => v3 = Some(false),
            "--piano3" => v3 = Some(true),
            "--vocal" => vocal = it.next(),
            "--drums" => drums = it.next(),
            "--eval" => eval = it.next(),
            "--spectral" => spectral = true,
            other => return usage(&format!("unknown argument {other}")),
        }
    }
    if bench {
        return run_bench();
    }
    if let (Some(w), true) = (&wav, spectral) {
        let (pcm, rate) = match std::fs::read(w).map_err(|e| e.to_string()).and_then(|b| piano::read_wav(&b)) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("{w}: {e}");
                return 1;
            }
        };
        let x = piano::to_rate(&pcm, rate);
        let t = Instant::now();
        let (notes, kpf) = crate::spectral::notes_stats(&x);
        let secs = x.len() as f64 / piano::RATE as f64;
        println!(
            "talking piano: {} strikes over {secs:.1} s ({:.0} note-ons/s, {kpf:.1} keys per frame, period {:.0} ms) in {:.2} ms per 0.5 s",
            notes.len(),
            notes.len() as f64 / secs,
            crate::spectral::period_s() * 1000.0,
            t.elapsed().as_secs_f64() * 1000.0 / (secs / 0.5)
        );
        if let Some(path) = out.as_deref() {
            if let Err(e) = write_midi(path, &notes, true, Some(0.0)) {
                eprintln!("{e}");
                return 1;
            }
            println!("wrote {path}");
        }
        return 0;
    }
    if let Some(w) = eval {
        let (pcm, rate) = match std::fs::read(&w).map_err(|e| e.to_string()).and_then(|b| piano::read_wav(&b)) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("{w}: {e}");
                return 1;
            }
        };
        let x = piano::to_rate(&pcm, rate);
        // sections: PIANO_EVAL_SECTIONS="intro=0-32,verse=32-92,chorus=104-134"
        let secs: Vec<(String, f64, f64)> = std::env::var("PIANO_EVAL_SECTIONS")
            .unwrap_or_else(|_| "intro=0-32,verse=32-92,chorus=104-134".into())
            .split(',')
            .filter_map(|kv| {
                let (k, v) = kv.split_once('=')?;
                let (a, b) = v.split_once('-')?;
                Some((k.to_string(), a.parse().ok()?, b.parse().ok()?))
            })
            .collect();
        print!("{}", crate::piano_eval::eval(&x, &secs));
        return 0;
    }
    if let (Some(w), Some(po)) = (&wav, v3) {
        return arrange3_cli(w, out.as_deref(), po);
    }
    if let (Some(w), Some(d)) = (&wav, &drums) {
        return drums_cli(w, d);
    }
    if let (Some(w), true) = (&wav, subtract || vocal.is_some()) {
        return subtract_cli(w, out.as_deref(), vocal.as_deref());
    }
    if let (Some(w), Some(po)) = (&wav, arrange) {
        return arrange_cli(w, out.as_deref(), po);
    }
    if let Some(w) = wav {
        return offline(&w, out.as_deref(), merged, reference.as_deref());
    }
    let Some(src) = source else {
        return usage("need --wav, --source or --bench");
    };
    live_cli(&src, port, seconds, out.as_deref(), merged)
}

fn write_midi(path: &str, notes: &[Note], merged: bool, origin: Option<f64>) -> Result<(), String> {
    std::fs::write(path, crate::notes::piano_midi(notes, !merged, origin)).map_err(|e| format!("{path}: {e}"))
}

/// `piano --wav X --arrange3|--piano3 [--out f.mid]`: v3, the song's own figures cleaned plus the
/// vocal line (both from the subtraction core). Also writes <out>.melody.mid (the melody alone).
fn arrange3_cli(wav: &str, out: Option<&str>, piano_only: bool) -> i32 {
    let (l, r, rate) = match std::fs::read(wav).map_err(|e| e.to_string()).and_then(|b| crate::subtract::read_stereo(&b)) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{wav}: {e}");
            return 1;
        }
    };
    let (l, r) = (piano::to_rate(&l, rate), piano::to_rate(&r, rate));
    let secs = l.len() as f64 / piano::RATE as f64;
    let t = Instant::now();
    let fr = crate::subtract::frames(&l, &r);
    let acc = crate::subtract::notes(&fr);
    let line = crate::subtract::vocal_line(&fr);
    let mel = crate::subtract::line_notes(&fr, &line);
    let mut a = crate::arrange::analyze_from(fr.act.clone(), fr.frame_s, fr.t0, secs);
    a.melody = mel;
    let parts = crate::arrange::arrange_v3(&a, &acc, piano_only);
    println!("analysed {secs:.1} s in {:.2} s: tempo {:.1} BPM, key {}, {} beats", t.elapsed().as_secs_f64(), a.bpm, a.key_name(), a.beats.len());
    let mut line_s = String::new();
    let mut last = String::new();
    for (i, c) in a.chords.iter().enumerate() {
        let n = c.name();
        if n != last {
            line_s += &format!("{:.1}s {n}  ", a.beats[i]);
            last = n;
        }
    }
    println!("chords: {line_s}");
    let ms: Vec<String> = a.melody.iter().take(40).map(|n| format!("{:.2}:{}{}", n.t, crate::arrange::NAMES[(n.pitch % 12) as usize], n.pitch / 12 - 1)).collect();
    println!("melody (first 40): {}", ms.join(" "));
    for p in &parts {
        println!("part {} (ch {}, GM {}): {} notes", p.name, p.ch + 1, p.prog, p.notes.len());
    }
    if let Some(path) = out {
        let mp = std::path::Path::new(path).with_extension("melody.mid");
        if let Err(e) = std::fs::write(path, crate::arrange::midi(&parts)).and_then(|_| std::fs::write(&mp, crate::notes::piano_midi(&a.melody, false, Some(0.0)))) {
            eprintln!("{path}: {e}");
            return 1;
        }
        println!("wrote {path} and {}", mp.display());
    }
    0
}

/// `piano --wav X --drums stem.wav`: the drum stem (template subtraction) and stem.mid (GM ch 10).
fn drums_cli(wav: &str, stem: &str) -> i32 {
    let (pcm, rate) = match std::fs::read(wav).map_err(|e| e.to_string()).and_then(|b| piano::read_wav(&b)) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{wav}: {e}");
            return 1;
        }
    };
    let x = piano::to_rate(&pcm, rate);
    let t = Instant::now();
    let (st, notes) = crate::drums::stem_and_notes(&x);
    let secs = x.len() as f64 / piano::RATE as f64;
    let count = |p: u8| notes.iter().filter(|n| n.pitch == p).count();
    println!(
        "drums: {} hits (kick {}, snare {}, hat {}, cowbell {}) in {:.2} ms per 0.5 s",
        notes.len(),
        count(36),
        count(38),
        count(42),
        count(56),
        t.elapsed().as_secs_f64() * 1000.0 / (secs / 0.5)
    );
    let mp = std::path::Path::new(stem).with_extension("mid");
    let mid = crate::notes::smf_bytes(&[(crate::notes::TrackSpec { ch: 9, prog: None, name: Some("Drums".into()) }, notes)], Some(0.0));
    if let Err(e) = std::fs::write(stem, crate::subtract::wav16(&st, piano::RATE)).and_then(|_| std::fs::write(&mp, mid)) {
        eprintln!("{stem}: {e}");
        return 1;
    }
    println!("wrote {stem} and {}", mp.display());
    0
}

/// `piano --wav X --subtract [--out notes.mid] [--vocal stem.wav]`: notes by complex harmonic
/// subtraction (through the pianist layer), and the vocal stem plus its melody (stem.mid).
fn subtract_cli(wav: &str, out: Option<&str>, vocal: Option<&str>) -> i32 {
    let (l, r, rate) = match std::fs::read(wav).map_err(|e| e.to_string()).and_then(|b| crate::subtract::read_stereo(&b)) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{wav}: {e}");
            return 1;
        }
    };
    let (l, r) = (piano::to_rate(&l, rate), piano::to_rate(&r, rate));
    let secs = l.len() as f64 / piano::RATE as f64;
    let t = Instant::now();
    let fr = crate::subtract::frames(&l, &r);
    let el = t.elapsed().as_secs_f64();
    let raw = if std::env::var_os("PIANO_STREAM").is_some() {
        // the live core, fed in 10 ms chunks of the mono mix
        let mono: Vec<f32> = l.iter().zip(&r).map(|(a, b)| 0.5 * (a + b)).collect();
        let mut st = crate::subtract::SubTracker::new();
        let t1 = Instant::now();
        let mut evs = Vec::new();
        for c in mono.chunks(220) {
            evs.extend(st.push(c));
        }
        evs.extend(st.flush());
        let e1 = t1.elapsed().as_secs_f64();
        println!("live core: {:.2} ms per 0.5 s", e1 * 1000.0 / (secs / 0.5));
        piano::events_to_notes(&evs)
    } else {
        crate::subtract::notes(&fr)
    };
    let notes = if std::env::var_os("PIANO_RAW").is_some() { raw.clone() } else { crate::pianist::perform(&raw) };
    let poly = fr.found.iter().map(|f| f.len()).sum::<usize>() as f64 / fr.found.len().max(1) as f64;
    println!("{} notes ({} raw) from {secs:.1} s in {el:.2} s ({:.2} ms per 0.5 s), {poly:.1} series per frame", notes.len(), raw.len(), el * 1000.0 / (secs / 0.5));
    if std::env::var_os("PIANO_DUMP").is_some() {
        for n in notes.iter() {
            println!("N {:8.3} {:6.3} {:3} {:3}", n.t, n.dur, n.pitch, n.vel);
        }
    }
    if let Some(path) = out {
        if let Err(e) = write_midi(path, &notes, true, Some(0.0)) {
            eprintln!("{e}");
            return 1;
        }
        println!("wrote {path}");
    }
    if let Some(vp) = vocal {
        let line = crate::subtract::vocal_line(&fr);
        let stem = crate::subtract::resynth(&fr, &line, l.len());
        let mel = crate::subtract::line_notes(&fr, &line);
        let voiced = line.iter().filter(|x| x.is_some()).count() as f64 / line.len().max(1) as f64;
        let mp = std::path::Path::new(vp).with_extension("mid");
        if let Err(e) = std::fs::write(vp, crate::subtract::wav16(&stem, piano::RATE)).and_then(|_| std::fs::write(&mp, crate::notes::piano_midi(&mel, false, Some(0.0)))) {
            eprintln!("{vp}: {e}");
            return 1;
        }
        println!("vocal: voiced {:.0}% of frames, {} melody notes; wrote {vp} and {}", voiced * 100.0, mel.len(), mp.display());
    }
    0
}

/// `piano --wav X --arrange|--piano-only [--out f.mid]`: tempo, key, chords, and the arranged parts.
fn arrange_cli(wav: &str, out: Option<&str>, piano_only: bool) -> i32 {
    let (pcm, rate) = match std::fs::read(wav).map_err(|e| e.to_string()).and_then(|b| piano::read_wav(&b)) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{wav}: {e}");
            return 1;
        }
    };
    let t = Instant::now();
    let a = crate::arrange::analyze(&pcm, rate);
    let parts = crate::arrange::arrange(&a, piano_only);
    println!("analysed {:.1} s in {:.2} s: tempo {:.1} BPM, key {}, {} beats", pcm.len() as f64 / rate as f64, t.elapsed().as_secs_f64(), a.bpm, a.key_name(), a.beats.len());
    let mut line = String::new();
    let mut last = String::new();
    for (i, c) in a.chords.iter().enumerate() {
        let n = c.name();
        if n != last {
            line += &format!("{:.1}s {n}  ", a.beats[i]);
            last = n;
        }
    }
    println!("chords: {line}");
    let mel: Vec<String> = a.melody.iter().take(40).map(|n| format!("{}{}", crate::arrange::NAMES[(n.pitch % 12) as usize], n.pitch / 12 - 1)).collect();
    println!("melody (first 40): {}", mel.join(" "));
    for p in &parts {
        println!("part {} (ch {}, GM {}): {} notes", p.name, p.ch + 1, p.prog, p.notes.len());
    }
    if let Some(path) = out {
        if let Err(e) = std::fs::write(path, crate::arrange::midi(&parts)) {
            eprintln!("{path}: {e}");
            return 1;
        }
        println!("wrote {path}");
    }
    0
}

fn offline(wav: &str, out: Option<&str>, merged: bool, reference: Option<&str>) -> i32 {
    let bytes = match std::fs::read(wav) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{wav}: {e}");
            return 1;
        }
    };
    let (pcm, rate) = match piano::read_wav(&bytes) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{wav}: {e}");
            return 1;
        }
    };
    let t = Instant::now();
    let raw = piano::transcribe(&pcm, rate);
    let notes = if std::env::var_os("PIANO_RAW").is_some() { raw } else { crate::pianist::perform(&raw) };
    let secs = pcm.len() as f64 / rate as f64;
    let el = t.elapsed().as_secs_f64();
    if std::env::var_os("PIANO_DUMP").is_some() {
        for n in notes.iter() {
            println!("N {:8.3} {:6.3} {:3} {:3}", n.t, n.dur, n.pitch, n.vel);
        }
    }
    let lanes = piano::split_lanes(&notes);
    println!(
        "{} notes from {secs:.1} s of audio in {:.2} s ({:.2} ms per 0.5 s, incl. resampling) — Bass {} / Mid {} / High {}",
        notes.len(),
        el,
        el * 1000.0 / (secs / 0.5).max(1e-9),
        lanes[0].len(),
        lanes[1].len(),
        lanes[2].len()
    );
    if let Some(path) = out {
        if let Err(e) = write_midi(path, &notes, merged, Some(0.0)) {
            eprintln!("{e}");
            return 1;
        }
        println!("wrote {path}");
    }
    if let Some(r) = reference {
        let gt: Vec<Note> = match std::fs::read(r).map_err(|e| e.to_string()).and_then(|b| crate::notes::parse_smf(&b)) {
            Ok(v) => v.into_iter().map(|p| p.note).collect(),
            Err(e) => {
                eprintln!("{r}: {e}");
                return 1;
            }
        };
        let (p, rc, f) = piano::note_f1(&notes, &gt, 0.05);
        println!("vs {r}: {} reference notes — P {p:.3} R {rc:.3} F1 {f:.3} (onset ±50 ms, exact pitch)", gt.len());
    }
    0
}

static SIGNALLED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_sig: i32) {
    // async-signal-safe: one atomic store
    SIGNALLED.store(true, Ordering::Relaxed);
}

extern "C" {
    fn signal(sig: i32, handler: extern "C" fn(i32)) -> usize;
}

/// SIGINT, SIGTERM and SIGHUP set SIGNALLED instead of killing the process (the GUI polls it and
/// closes its window, which stops the session and sends every note-off).
pub fn install_quit_signals() {
    // SAFETY: installs a handler that only stores to a static atomic.
    unsafe {
        signal(1, on_signal);
        signal(2, on_signal);
        signal(15, on_signal);
    }
}

pub fn quit_requested() -> bool {
    SIGNALLED.load(Ordering::Relaxed)
}

/// SIGINT and SIGTERM set SIGNALLED instead of killing the process; a watcher copies it to `flag`.
fn on_signals(flag: Arc<AtomicBool>) {
    // SAFETY: installs a handler that only stores to a static atomic.
    unsafe {
        signal(2, on_signal);
        signal(15, on_signal);
    }
    std::thread::spawn(move || loop {
        if SIGNALLED.load(Ordering::Relaxed) {
            flag.store(true, Ordering::Relaxed);
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    });
}

fn live_cli(src: &str, port: bool, seconds: Option<f64>, out: Option<&str>, merged: bool) -> i32 {
    let cfg = crate::config::Config::load();
    let Some(id) = crate::stemcfg::resolve(&cfg, src) else {
        eprintln!("no source channel '{src}'");
        return 1;
    };
    let Some(inp) = cfg.inputs.iter().find(|i| i.id == id) else {
        eprintln!("source {id} not found");
        return 1;
    };
    let (tap, tap_sink) = crate::stemcfg::tap(inp);
    if crate::dash::no_audio() {
        eprintln!("MIXER_NO_AUDIO is set: no tap, no ALSA port");
        return 0;
    }
    let shared = Shared::new();
    let port = if port {
        match open_port() {
            Ok(p) => Some(p),
            Err(e) => {
                eprintln!("{e}");
                return 1;
            }
        }
    } else {
        None
    };
    let session = Session::start_with(Opts { tap, tap_sink, port: false, wav: None }, shared.clone(), port.clone());
    let eof = Arc::new(AtomicBool::new(false));
    // Ctrl-C / SIGTERM leave through the same path as EOF: the session is dropped (joined), which
    // sends every note-off and CC 123 before the process exits
    on_signals(eof.clone());
    if seconds.is_none() {
        let eof = eof.clone();
        std::thread::spawn(move || {
            let mut sink = [0u8; 256];
            let mut si = std::io::stdin();
            while let Ok(n) = si.read(&mut sink) {
                if n == 0 {
                    break;
                }
            }
            eof.store(true, Ordering::Relaxed);
        });
    }
    let t0 = Instant::now();
    let (mut printed, mut status, mut port_shown) = (0usize, String::new(), false);
    loop {
        std::thread::sleep(Duration::from_millis(50));
        {
            let g = shared.get();
            if g.status != status {
                status = g.status.clone();
                eprintln!("[{status}]");
            }
            if !port_shown {
                if let Some(p) = &g.port {
                    eprintln!("[ALSA port {CLIENT_NAME}:{PORT_NAME} at {p}]");
                    port_shown = true;
                }
            }
            for n in g.all.iter().skip(printed) {
                println!("{:8.3}s {:>4} ({:3}) vel {:3} dur {:.3}s {}", n.t, name(n.pitch), n.pitch, n.vel, n.dur, lane(n.pitch).name());
            }
            printed = g.all.len();
        }
        if eof.load(Ordering::Relaxed) || seconds.is_some_and(|s| t0.elapsed().as_secs_f64() >= s) {
            break;
        }
    }
    drop(session);
    drop(port);
    let mut g = shared.get();
    for n in g.all.iter().skip(printed) {
        println!("{:8.3}s {:>4} ({:3}) vel {:3} dur {:.3}s {}", n.t, name(n.pitch), n.pitch, n.vel, n.dur, lane(n.pitch).name());
    }
    eprintln!("[cpu {:.2} ms per 0.5 s, attack->NoteOn {:.0} ms on the analysis stream]", g.cpu_ms, g.latency_ms);
    let notes = g.take_notes();
    if let Some(path) = out {
        if let Err(e) = write_midi(path, &notes, merged, None) {
            eprintln!("{e}");
            return 1;
        }
        eprintln!("wrote {path} ({} notes)", notes.len());
    }
    0
}

/// The #20 bench format: the live path (44.1 kHz stereo -> downmix -> tracker) timed per 0.5 s chunk
/// on 60 s of dense synthetic piano chords (5 notes, a new chord every 0.5 s), 5 warm-up chunks,
/// then n chunks (BENCH_N, default 100), one core. Plus attack -> NoteOn latency fed in 10 ms chunks.
fn run_bench() -> i32 {
    let n: usize = std::env::var("BENCH_N").ok().and_then(|v| v.parse().ok()).unwrap_or(100);
    let secs = (n + 5) as f64 * 0.5 + 1.0;
    let mut gt = Vec::new();
    for i in 0..(secs * 2.0) as usize {
        for iv in [0u8, 4, 7, 11, 14] {
            gt.push(Note { t: 0.5 * i as f64, dur: 0.45, pitch: 36 + (i * 5 % 36) as u8 + iv, vel: 70 + (i * 13 % 50) as u8 });
        }
    }
    let mono = piano::synth_piano(&gt, secs, 8);
    // 44.1 kHz stereo, as the tap delivers it (sample-and-hold up; the decimator removes the images)
    let stereo: Vec<f32> = mono.iter().flat_map(|&v| [v, v, v, v]).collect();
    let chunk = TAP_RATE / 2 * 2; // 0.5 s of interleaved stereo
    let mut tr = PianoTracker::new();
    let mut dm = Downmix::new();
    let mut ms = Vec::new();
    for (i, c) in stereo.chunks(chunk).enumerate().take(n + 5) {
        let t = Instant::now();
        let m = dm.push(c);
        std::hint::black_box(tr.push(&m));
        if i >= 5 {
            ms.push(t.elapsed().as_secs_f64() * 1000.0);
        }
    }
    ms.sort_by(|a, b| a.total_cmp(b));
    let q = |f: f64| ms[((ms.len() as f64 * f).ceil() as usize).clamp(1, ms.len()) - 1];
    let (p50, p99, max) = (q(0.5), q(0.99), ms[ms.len() - 1]);
    println!("BENCH ep=cpu model=piano-dsp n={} p50_ms={p50:.2} p99_ms={p99:.2} max_ms={max:.2} rtf={:.4}", ms.len(), p50 / 500.0);
    let mut lats = Vec::new();
    for pitch in [24u8, 36, 48, 60, 72, 84, 96, 105] {
        let s = piano::synth_piano(&[Note { t: 0.5, dur: 0.6, pitch, vel: 90 }], 1.5, 7);
        let mut tr = PianoTracker::new();
        let mut fed = 0usize;
        for c in s.chunks(220) {
            fed += c.len();
            if tr.push(c).iter().any(|e| matches!(e, Ev::On { pitch: p, .. } if *p == pitch)) {
                lats.push((pitch, fed as f64 / piano::RATE as f64 - 0.5));
                break;
            }
        }
    }
    let l: Vec<String> = lats.iter().map(|(p, l)| format!("{}:{:.0}", name(*p), l * 1000.0)).collect();
    let mean = lats.iter().map(|x| x.1).sum::<f64>() / lats.len().max(1) as f64 * 1000.0;
    let worst = lats.iter().map(|x| x.1).fold(0.0, f64::max) * 1000.0;
    println!("LATENCY attack->NoteOn ms (analysis stream, 10 ms feeds, excl. the 10 ms tap buffer): {} mean={mean:.0} worst={worst:.0}", l.join(" "));
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Rec(Vec<String>);
    impl Sink for Rec {
        fn note(&mut self, on: bool, ch: u8, key: u8, vel: u8) -> Result<(), String> {
            self.0.push(format!("{} {ch} {key} {vel}", if on { "on" } else { "off" }));
            Ok(())
        }
        fn control(&mut self, ch: u8, cc: u8, val: u8) -> Result<(), String> {
            self.0.push(format!("cc {ch} {cc} {val}"));
            Ok(())
        }
    }

    #[test]
    fn lanes_map_to_channels_and_stop_silences_everything() {
        let mut m = MidiOut::new(Rec::default());
        m.event(&Ev::On { t: 0.0, pitch: 40, vel: 90 });
        m.event(&Ev::On { t: 0.0, pitch: 60, vel: 0 });
        m.event(&Ev::On { t: 0.0, pitch: 80, vel: 200 });
        m.event(&Ev::Off { t: 0.5, pitch: 60 });
        m.event(&Ev::Off { t: 0.5, pitch: 61 }); // never sounded: nothing sent
        assert_eq!(m.sounding(), 2);
        m.all_off();
        assert_eq!(m.sounding(), 0);
        let got = std::mem::take(&mut m.sink.0);
        assert_eq!(&got[..4], &["on 0 40 90", "on 1 60 1", "on 2 80 127", "off 1 60 0"]);
        assert!(got.contains(&"off 0 40 0".to_string()) && got.contains(&"off 2 80 0".to_string()), "{got:?}");
        assert_eq!(&got[got.len() - 3..], &["cc 0 123 0", "cc 1 123 0", "cc 2 123 0"]);
        // a re-strike of a sounding key turns it off first, so a receiver never stacks two
        m.event(&Ev::On { t: 1.0, pitch: 50, vel: 80 });
        m.event(&Ev::On { t: 1.2, pitch: 50, vel: 80 });
        assert_eq!(m.sink.0, vec!["on 1 50 80", "off 1 50 0", "on 1 50 80"]);
        m.sink.0.clear();
        drop(m); // drop silences what is left (checked by the recorder being dropped with it)
    }

    #[test]
    fn drop_sends_all_off() {
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        struct Shared2(Arc<Mutex<Vec<String>>>);
        impl Sink for Shared2 {
            fn note(&mut self, on: bool, ch: u8, key: u8, _vel: u8) -> Result<(), String> {
                lock(&self.0).push(format!("{} {ch} {key}", if on { "on" } else { "off" }));
                Ok(())
            }
            fn control(&mut self, ch: u8, cc: u8, _val: u8) -> Result<(), String> {
                lock(&self.0).push(format!("cc {ch} {cc}"));
                Ok(())
            }
        }
        {
            let mut m = MidiOut::new(Shared2(log.clone()));
            m.event(&Ev::On { t: 0.0, pitch: 70, vel: 90 });
        }
        assert_eq!(*lock(&log), vec!["on 1 70", "off 1 70", "cc 0 123", "cc 1 123", "cc 2 123"]);
    }

    /// A sink that refuses its first `refuse` sends (a full non-blocking queue).
    struct Flaky(Arc<Mutex<Vec<String>>>, usize);
    impl Sink for Flaky {
        fn note(&mut self, on: bool, ch: u8, key: u8, _vel: u8) -> Result<(), String> {
            if self.1 > 0 {
                self.1 -= 1;
                return Err("EAGAIN".into());
            }
            lock(&self.0).push(format!("{} {ch} {key}", if on { "on" } else { "off" }));
            Ok(())
        }
        fn control(&mut self, ch: u8, cc: u8, _val: u8) -> Result<(), String> {
            if self.1 > 0 {
                self.1 -= 1;
                return Err("EAGAIN".into());
            }
            lock(&self.0).push(format!("cc {ch} {cc}"));
            Ok(())
        }
    }

    /// R2-F6: a note-off the sink refuses is queued and goes out on the next event or flush, and
    /// all_off resends it; nothing is lost and nothing sleeps on the event path.
    #[test]
    fn refused_note_offs_are_retried_not_lost() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut m = MidiOut::new(Flaky(log.clone(), 0));
        m.event(&Ev::On { t: 0.0, pitch: 60, vel: 90 });
        m.sink.1 = 1; // the next send is refused: the note-off
        m.event(&Ev::Off { t: 0.5, pitch: 60 });
        assert_eq!(m.pending(), 1);
        assert!(!lock(&log).iter().any(|l| l == "off 1 60"));
        m.flush();
        assert_eq!(m.pending(), 0);
        assert!(lock(&log).iter().any(|l| l == "off 1 60"));
        // all_off with a refusing sink keeps the offs and CCs queued, the drop delivers them
        m.event(&Ev::On { t: 1.0, pitch: 40, vel: 90 });
        m.sink.1 = 2;
        m.all_off();
        assert!(m.pending() >= 1);
        drop(m);
        let l = lock(&log);
        assert!(l.iter().any(|x| x == "off 0 40"), "{l:?}");
        for ch in 0..3 {
            assert!(l.iter().any(|x| *x == format!("cc {ch} 123")), "{l:?}");
        }
    }

    /// R2-2: a new take starts empty; a note sounding across it lands in the new take.
    #[test]
    fn new_take_forgets_the_old_one() {
        let mut g = Inner::default();
        g.apply(&[Ev::On { t: 0.5, pitch: 60, vel: 90 }, Ev::Off { t: 0.9, pitch: 60 }, Ev::On { t: 1.0, pitch: 64, vel: 90 }], 1.2);
        g.new_take();
        assert!(g.snapshot().iter().all(|n| n.pitch == 64), "{:?}", g.snapshot());
        assert!(g.roll.is_empty());
        g.apply(&[Ev::Off { t: 2.0, pitch: 64 }], 2.1);
        assert_eq!(g.snapshot().len(), 1);
    }

    #[test]
    fn roll_pairs_events_and_save_cuts_open_notes() {
        let mut g = Inner::default();
        g.apply(&[Ev::On { t: 1.0, pitch: 60, vel: 90 }, Ev::On { t: 1.0, pitch: 64, vel: 80 }], 1.2);
        g.apply(&[Ev::Off { t: 1.5, pitch: 60 }], 1.7);
        assert_eq!(g.roll.len(), 1);
        assert_eq!(g.open.len(), 1);
        assert!((g.latency_ms - 200.0).abs() < 1e-6);
        let notes = g.take_notes();
        assert_eq!(notes.len(), 2);
        assert!((notes[1].dur - 0.7).abs() < 1e-9, "{notes:?}");
        // the roll forgets old notes, Save MIDI does not
        g.apply(&[Ev::On { t: 2.0, pitch: 50, vel: 90 }, Ev::Off { t: 2.5, pitch: 50 }], 100.0);
        assert!(g.roll.is_empty());
        assert_eq!(g.all.len(), 1);
    }

    /// The real ALSA port: opens, shows up as a client, sends, and closes. Needs /dev/snd/seq, so ignored.
    #[test]
    #[ignore]
    fn alsa_port_lifecycle() {
        let s = AlsaSink::open().expect("ALSA sequencer");
        let addr = s.address();
        let clients = std::fs::read_to_string("/proc/asound/seq/clients").unwrap_or_default();
        assert!(clients.contains(CLIENT_NAME), "{CLIENT_NAME} not in /proc/asound/seq/clients");
        let mut m = MidiOut::new(s);
        m.event(&Ev::On { t: 0.0, pitch: 60, vel: 90 });
        m.event(&Ev::Off { t: 0.1, pitch: 60 });
        drop(m);
        let after = std::fs::read_to_string("/proc/asound/seq/clients").unwrap_or_default();
        assert!(!after.contains(CLIENT_NAME), "client {addr} still listed after drop");
    }
}

#[cfg(test)]
#[path = "piano_live_breaker_tests.rs"]
mod breaker_tests;
