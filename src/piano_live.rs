//! Live piano (#23): a passive `pw-cat` tap -> `PianoTracker` -> the shared roll, and optionally the
//! "Wavelink Piano" ALSA sequencer port for other software. It adds no engine graph nodes and plays
//! nothing: the tap is `node.passive=true`, the same one `wavelink-mixer stems --source` uses.
#![allow(dead_code)]
use crate::notes::Note;
use crate::piano::{self, lane, Downmix, Ev, PianoTracker};
use std::io::Read;
use std::process::{Child, Command, Stdio};
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
}

impl<S: Sink> MidiOut<S> {
    pub fn new(sink: S) -> Self {
        MidiOut { sink, sounding: Vec::new() }
    }

    pub fn event(&mut self, e: &Ev) {
        match *e {
            Ev::On { pitch, vel, .. } => {
                let ch = channel(pitch);
                if self.sounding.contains(&(ch, pitch)) {
                    let _ = self.sink.note(false, ch, pitch, 0);
                } else {
                    self.sounding.push((ch, pitch));
                }
                let _ = self.sink.note(true, ch, pitch, vel.clamp(1, 127));
            }
            Ev::Off { pitch, .. } => {
                let ch = channel(pitch);
                if let Some(i) = self.sounding.iter().position(|x| *x == (ch, pitch)) {
                    self.sounding.swap_remove(i);
                    let _ = self.sink.note(false, ch, pitch, 0);
                }
            }
        }
    }

    pub fn sounding(&self) -> usize {
        self.sounding.len()
    }

    /// Note-off for every sounding note, then CC 123 (All Notes Off) on the three lane channels.
    pub fn all_off(&mut self) {
        for (ch, key) in std::mem::take(&mut self.sounding) {
            let _ = self.sink.note(false, ch, key, 0);
        }
        for ch in 0..3 {
            let _ = self.sink.control(ch, 123, 0);
        }
    }
}

impl<S: Sink> Drop for MidiOut<S> {
    fn drop(&mut self) {
        self.all_off();
    }
}

/// The "Wavelink Piano" ALSA sequencer client with one readable, subscribable port.
pub struct AlsaSink {
    seq: alsa::Seq,
    port: i32,
}

impl AlsaSink {
    pub fn open() -> Result<AlsaSink, String> {
        use alsa::seq::{PortCap, PortType};
        use std::ffi::CString;
        let seq = alsa::Seq::open(None, Some(alsa::Direction::Playback), false).map_err(|e| format!("ALSA sequencer: {e}"))?;
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

/// What the UI and the CLI read from a running session.
#[derive(Default)]
pub struct Inner {
    /// Finished notes, last ROLL_KEEP_S seconds, times on the session clock.
    pub roll: Vec<Note>,
    /// Every finished note of the session, for Save MIDI.
    pub all: Vec<Note>,
    /// Sounding notes: (start, pitch, vel).
    pub open: Vec<(f64, u8, u8)>,
    /// Session clock: seconds of audio analysed.
    pub now: f64,
    pub status: String,
    /// CPU per 0.5 s of audio, ms (smoothed).
    pub cpu_ms: f64,
    /// Attack -> NoteOn on the analysis stream, ms (smoothed over recent notes).
    pub latency_ms: f64,
    pub port: Option<String>,
}

#[derive(Clone, Default)]
pub struct Shared(pub Arc<Mutex<Inner>>);

impl Shared {
    pub fn new() -> Shared {
        Shared::default()
    }
    pub fn get(&self) -> MutexGuard<'_, Inner> {
        lock(&self.0)
    }
}

impl Inner {
    /// Applies tracker events to the roll. `fed` is the analysis clock when they were returned.
    pub fn apply(&mut self, evs: &[Ev], fed: f64) {
        for e in evs {
            match *e {
                Ev::On { t, pitch, vel } => {
                    self.close(pitch, t);
                    self.open.push((t, pitch, vel));
                    let lat = (fed - t) * 1000.0;
                    self.latency_ms = if self.latency_ms == 0.0 { lat } else { self.latency_ms * 0.9 + lat * 0.1 };
                }
                Ev::Off { t, pitch } => self.close(pitch, t),
            }
        }
        self.now = fed;
        let keep = fed - ROLL_KEEP_S;
        self.roll.retain(|n| n.t + n.dur >= keep);
    }

    fn close(&mut self, pitch: u8, t: f64) {
        if let Some(i) = self.open.iter().position(|o| o.1 == pitch) {
            let (t0, p, v) = self.open.swap_remove(i);
            let n = Note { t: t0, dur: (t - t0).max(0.0), pitch: p, vel: v };
            self.roll.push(n.clone());
            self.all.push(n);
        }
    }

    /// Notes for Save MIDI: everything finished, plus the sounding ones cut at `now`.
    pub fn take_notes(&mut self) -> Vec<Note> {
        let mut v = std::mem::take(&mut self.all);
        v.extend(self.open.iter().map(|&(t, p, vel)| Note { t, dur: (self.now - t).max(0.0), pitch: p, vel }));
        v.sort_by(|a, b| a.t.total_cmp(&b.t).then(a.pitch.cmp(&b.pitch)));
        v
    }
}

pub struct Opts {
    /// PipeWire node to tap and whether it is a sink (`stemcfg::tap`).
    pub tap: String,
    pub tap_sink: bool,
    /// Open the ALSA port.
    pub port: bool,
    /// Replay this WAV at real-time pace instead of tapping (MIXER_PIANO_WAV: UI drives in test mode).
    pub wav: Option<std::path::PathBuf>,
}

/// One live run. Drop stops it: the stop flag, then the pw-cat child (ours, by handle) is killed and
/// reaped; the worker sees EOF, sends all-notes-off and closes the port.
pub struct Session {
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
}

impl Session {
    pub fn start(o: Opts, shared: Shared) -> Session {
        let stop = Arc::new(AtomicBool::new(false));
        let child: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
        *shared.get() = Inner { status: "starting…".into(), ..Inner::default() };
        let (s2, c2) = (stop.clone(), child.clone());
        std::thread::spawn(move || {
            if let Err(e) = run(&o, &shared, &s2, &c2) {
                if !s2.load(Ordering::Relaxed) {
                    shared.get().status = format!("error: {e}");
                }
            }
        });
        Session { stop, child }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(mut c) = lock(&self.child).take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn spawn_tap(o: &Opts) -> Result<Child, String> {
    let mut cmd = Command::new(crate::stems::TAP_BIN);
    cmd.args(crate::stems::tap_args(&o.tap, o.tap_sink, TAP_NAME)).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    crate::pw::die_with_parent(&mut cmd);
    cmd.spawn().map_err(|e| format!("{}: {e}", crate::stems::TAP_BIN))
}

fn run(o: &Opts, shared: &Shared, stop: &AtomicBool, child: &Mutex<Option<Child>>) -> Result<(), String> {
    let mut out = if o.port && !crate::dash::no_audio() {
        let s = AlsaSink::open()?;
        shared.get().port = Some(s.address());
        Some(MidiOut::new(s))
    } else {
        None
    };
    if let Some(path) = &o.wav {
        return replay(path, shared, stop, out.as_mut());
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
    let mut tr = PianoTracker::new();
    let mut dm = Downmix::new();
    let mut raw = vec![0u8; 4 * 2 * 441]; // 10 ms of f32 stereo
    let mut carry: Vec<u8> = Vec::new();
    let (mut audio_s, mut busy_s) = (0f64, 0f64);
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
        let evs = tr.push(&mono);
        busy_s += t.elapsed().as_secs_f64();
        audio_s += (stereo.len() / 2) as f64 / TAP_RATE as f64;
        if let Some(m) = out.as_mut() {
            for e in &evs {
                m.event(e);
            }
        }
        let mut g = shared.get();
        g.apply(&evs, tr.now());
        if audio_s >= 0.5 {
            let ms = busy_s * 1000.0 / audio_s * 0.5;
            g.cpu_ms = if g.cpu_ms == 0.0 { ms } else { g.cpu_ms * 0.8 + ms * 0.2 };
            audio_s = 0.0;
            busy_s = 0.0;
        }
    }
    let end = tr.now();
    let evs = tr.all_off(end);
    if let Some(m) = out.as_mut() {
        m.all_off();
    }
    shared.get().apply(&evs, end);
    Ok(())
}

/// Feeds a WAV through the tracker in 10 ms steps at real-time pace. Reads a file, plays nothing.
fn replay(path: &std::path::Path, shared: &Shared, stop: &AtomicBool, mut out: Option<&mut MidiOut<AlsaSink>>) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let (pcm, rate) = piano::read_wav(&bytes)?;
    let x = piano::to_rate(&pcm, rate);
    shared.get().status = format!("replaying {}", path.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default());
    let mut tr = PianoTracker::new();
    let step = piano::RATE as usize / 100;
    let t0 = Instant::now();
    let (mut audio_s, mut busy_s) = (0f64, 0f64);
    for (i, c) in x.chunks(step).enumerate() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let due = t0 + Duration::from_secs_f64(i as f64 * 0.01);
        if let Some(d) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(d);
        }
        let t = Instant::now();
        let evs = tr.push(c);
        busy_s += t.elapsed().as_secs_f64();
        audio_s += c.len() as f64 / piano::RATE as f64;
        if let Some(m) = out.as_deref_mut() {
            for e in &evs {
                m.event(e);
            }
        }
        let mut g = shared.get();
        g.apply(&evs, tr.now());
        if audio_s >= 0.5 {
            let ms = busy_s * 1000.0 / audio_s * 0.5;
            g.cpu_ms = if g.cpu_ms == 0.0 { ms } else { g.cpu_ms * 0.8 + ms * 0.2 };
            audio_s = 0.0;
            busy_s = 0.0;
        }
    }
    let end = tr.now();
    let evs = tr.all_off(end);
    if let Some(m) = out {
        m.all_off();
    }
    let mut g = shared.get();
    g.apply(&evs, end);
    g.status = "replay finished".into();
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
            other => return usage(&format!("unknown argument {other}")),
        }
    }
    if bench {
        return run_bench();
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
    let notes = piano::transcribe(&pcm, rate);
    let secs = pcm.len() as f64 / rate as f64;
    let el = t.elapsed().as_secs_f64();
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
    let session = Session::start(Opts { tap, tap_sink, port, wav: None }, shared.clone());
    let eof = Arc::new(AtomicBool::new(false));
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
    std::thread::sleep(Duration::from_millis(100));
    let mut g = shared.get();
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
