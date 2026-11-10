//! Stems session: taps a channel, separates it, plays and/or shows the stems, and writes notes to the MIDI channel.
#![allow(dead_code)]
use crate::config::Config;
use crate::dash::{Fft, BANDS};
use crate::dsp::{envelope, onsets, xcorr_lag, BP_RATE};
use crate::models::{self, ModelSet, S_FILES, U_FILES};
use crate::notes::{synth_events, Note, Synth, Timeline, MIDI_LAG, STEMS};
use crate::pw::{die_with_parent, Graph};
use crate::sep::{calibrate, deinterleave, Separator, Streamer, Transcriber, BLOCK, BLOCK_MS, BP_SAMPLES, PARALLEL};
use crate::stemcfg::{self, Plays, SepEngine};
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

pub const RATE: usize = 44100;
pub const CH: usize = 2;
/// 10 ms, the Quality writer's pacing.
pub const SLICE: usize = 441;
pub const CHUNK_BLOCKS: usize = 172;
/// CHUNK_BLOCKS * sep::BLOCK.
pub const CHUNK: usize = 22016;
pub const BP_WINDOW: usize = 88200;
pub const DRUM_WINDOW: usize = 66150;
/// Not pw-record: the leak watch counts the GUI's pw-record children.
pub const TAP_BIN: &str = "pw-cat";
/// (left, hop, pad) in seconds, from the most to the least demanding.
pub const LADDER: [(f64, f64, f64); 5] = [(1.0, 0.5, 0.5), (1.0, 0.25, 0.25), (1.0, 0.2, 0.2), (1.0, 0.1, 0.15), (1.0, 0.1, 0.1)];

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Writes the status only while the handle's session is still the current one (see `Shared::live`).
fn set_status(shared: &Shared, s: impl Into<String>) {
    if let Some(_g) = shared.live() {
        *lock(&shared.status) = s.into();
    }
}

fn frames(secs: f64) -> usize {
    (secs * RATE as f64).round() as usize
}

fn strs(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|s| s.to_string()).collect()
}

fn bytes_to_f32(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

fn median(v: &[f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    s[s.len() / 2]
}

/// Seconds of delay for ladder tier `idx` when separation costs `ms_per_s`: hop + pad + 1.5 t + 0.03.
pub fn tier_delay(idx: usize, ms_per_s: f64) -> f64 {
    let (l, h, p) = LADDER[idx];
    let t = ms_per_s * (l + h + p) / 1000.0;
    h + p + 1.5 * t + 0.03
}

/// Returns (ladder index, hop+pad+1.5 t + 0.03) for the cheapest tier this machine keeps up with.
pub fn pick_tier(ms_per_s: f64) -> (usize, f64) {
    for idx in (0..LADDER.len()).rev() {
        let (l, h, p) = LADDER[idx];
        let t = ms_per_s * (l + h + p) / 1000.0;
        if t <= 0.5 * h {
            return (idx, tier_delay(idx, ms_per_s));
        }
    }
    (0, 1.5)
}

/// Lindley-style cushion for a live buffer, from per-call wall times and the hop; clamped to 10..=200 ms.
pub fn cushion_ms(call_ms: &[f64], hop_ms: f64) -> u32 {
    let (mut finish, mut worst) = (0.0f64, 0.0f64);
    for (i, &c) in call_ms.iter().enumerate() {
        let due = i as f64 * hop_ms;
        finish = finish.max(due) + c;
        worst = worst.max(finish - due);
    }
    (((worst + 5.0) / 5.0).ceil() * 5.0).clamp(10.0, 200.0) as u32
}

/// `pw-cat --record --target <target> -P "{ node.name=<name> node.passive=true media.class=Stream/Input/Audio [stream.capture.sink=true] node.dont-fallback=true }" ...`
pub fn tap_args(target: &str, capture_sink: bool, name: &str) -> Vec<String> {
    let sink = if capture_sink { " stream.capture.sink=true" } else { "" };
    let props = format!("{{ node.name={name} node.passive=true media.class=Stream/Input/Audio{sink} node.dont-fallback=true }}");
    let mut v = strs(&["--record", "--target"]);
    v.push(target.to_string());
    v.push("-P".to_string());
    v.push(props);
    v.extend(strs(&["--format", "f32", "--rate", "44100", "--channels", "2", "--latency", "10ms", "--raw", "-"]));
    v
}

/// `pw-cat --playback --target <node> ...`
pub fn play_args(node: &str) -> Vec<String> {
    let mut v = strs(&["--playback", "--target"]);
    v.push(node.to_string());
    v.push("-P".to_string());
    v.push("{ node.name=stems_out node.dont-fallback=true }".to_string());
    v.extend(strs(&["--format", "f32", "--rate", "44100", "--channels", "2", "--latency", "10ms", "--raw", "-"]));
    v
}

/// Windows are [id*hop - left, id*hop + hop + pad) in frames, zeros before 0.
pub struct Chunker {
    left: usize,
    hop: usize,
    pad: usize,
    /// Interleaved stereo, starting at absolute frame `start`.
    buf: Vec<f32>,
    start: i64,
    next: u64,
}

impl Chunker {
    pub fn new(left: usize, hop: usize, pad: usize) -> Self {
        Chunker { left, hop, pad, buf: Vec::new(), start: 0, next: 0 }
    }

    /// Interleaved stereo.
    pub fn push(&mut self, pcm: &[f32]) {
        self.buf.extend_from_slice(pcm);
    }

    pub fn next_window(&mut self) -> Option<Window> {
        let s = self.next as i64 * self.hop as i64 - self.left as i64;
        let len = (self.left + self.hop + self.pad) as i64;
        let e = s + len;
        let avail_end = self.start + (self.buf.len() / CH) as i64;
        if e > avail_end {
            return None;
        }
        let pre = (-s).clamp(0, len) as usize;
        let mut pcm = vec![0.0f32; pre * CH];
        let from = (s.max(0) - self.start) as usize;
        let to = (e - self.start) as usize;
        pcm.extend_from_slice(&self.buf[from * CH..to * CH]);
        let id = self.next;
        self.next += 1;
        // Frames before the next window's start are never read again.
        let keep = (self.next as i64 * self.hop as i64 - self.left as i64).max(0);
        if keep > self.start {
            let cut = ((keep - self.start) as usize * CH).min(self.buf.len());
            self.buf.drain(..cut);
            self.start = keep;
        }
        Some(Window { id, pcm })
    }
}

/// Interleaved stereo, frames as in Chunker.
pub struct Window {
    pub id: u64,
    pub pcm: Vec<f32>,
}

/// hop*CH samples per stem: window frames [left, left + hop).
pub struct HopResult {
    pub id: u64,
    pub stems: [Vec<f32>; 3],
}

pub enum Slot {
    Ready(HopResult),
    Late,
}

/// Results keyed by hop id. A hop is Ready only if it arrived before its deadline; `take` moves the cursor past it.
pub struct Scheduler {
    ready: BTreeMap<u64, HopResult>,
    next: u64,
}

impl Scheduler {
    pub fn new() -> Self {
        Scheduler { ready: BTreeMap::new(), next: 0 }
    }

    pub fn put(&mut self, r: HopResult) {
        if r.id < self.next {
            return;
        }
        self.ready.insert(r.id, r);
    }

    pub fn take(&mut self, id: u64) -> Slot {
        self.next = self.next.max(id + 1);
        let hit = self.ready.remove(&id);
        self.ready.retain(|&k, _| k > id);
        match hit {
            Some(r) => Slot::Ready(r),
            None => Slot::Late,
        }
    }

    pub fn pending(&self) -> usize {
        self.ready.len()
    }
}

#[derive(Serialize)]
pub struct ChunkHeader {
    pub op: &'static str,
    pub id: u64,
    pub t0: f64,
    pub rate: u32,
    pub frames: usize,
    pub stems: [&'static str; 3],
    pub notes: BTreeMap<&'static str, Vec<[f64; 4]>>,
}

/// t0 = id * 22016 / 44100; notes as [t - t0, dur, pitch, vel].
pub fn chunk_header(id: u64, notes: &[Vec<Note>; 3]) -> ChunkHeader {
    let t0 = id as f64 * CHUNK as f64 / RATE as f64;
    let mut map = BTreeMap::new();
    for (i, name) in STEMS.iter().enumerate() {
        let v = notes[i].iter().map(|n| [n.t - t0, n.dur, n.pitch as f64, n.vel as f64]).collect();
        map.insert(*name, v);
    }
    ChunkHeader { op: "chunk", id, t0, rate: RATE as u32, frames: CHUNK, stems: STEMS, notes: map }
}

/// Shared with the UI: bands are 3 x BANDS; played is in frames at 44.1 kHz.
/// `current` counts sessions. `Session::start` gives its handle a new `gen` and bumps `current`; a handle
/// whose `gen` is no longer current (a stopped or replaced session) cannot write any more.
#[derive(Clone)]
pub struct Shared {
    pub bands: Vec<Arc<Mutex<Vec<f32>>>>,
    pub timeline: Arc<Mutex<Timeline>>,
    pub played: Arc<AtomicU64>,
    pub status: Arc<Mutex<String>>,
    current: Arc<AtomicU64>,
    gen: u64,
}

impl Shared {
    pub fn new() -> Shared {
        Shared {
            bands: (0..STEMS.len()).map(|_| Arc::new(Mutex::new(vec![0.0f32; BANDS]))).collect(),
            timeline: Arc::new(Mutex::new(Timeline::new())),
            played: Arc::new(AtomicU64::new(0)),
            status: Arc::new(Mutex::new(String::new())),
            current: Arc::new(AtomicU64::new(0)),
            gen: 0,
        }
    }

    /// played / 44100.
    pub fn now(&self) -> f64 {
        self.played.load(Ordering::Relaxed) as f64 / RATE as f64
    }

    /// The timeline lock, but only while this handle's session is current. Every session write takes it
    /// first, and `Session::start` bumps `current` before it takes the same lock to clear, so a stale
    /// write either lands before the clear or is refused.
    fn live(&self) -> Option<MutexGuard<'_, Timeline>> {
        let g = lock(&self.timeline);
        (self.current.load(Ordering::SeqCst) == self.gen).then_some(g)
    }
}

pub struct Opts {
    pub tap: String,
    pub tap_sink: bool,
    pub engine: SepEngine,
    pub plays: Plays,
    /// "ch_stems"
    pub out: String,
    /// 0 = auto
    pub buf_ms: u32,
    pub tier: Option<usize>,
}

/// Every pw-cat child and its role. `dead` is set by Drop, so a thread that is still starting up cannot leave an orphan.
#[derive(Default)]
struct KidList {
    dead: bool,
    kids: Vec<(&'static str, Child)>,
}

struct Kids(Mutex<KidList>);

type Pipes = (Option<ChildStdin>, Option<ChildStdout>);

impl Kids {
    fn new() -> Kids {
        Kids(Mutex::new(KidList::default()))
    }

    fn spawn(&self, role: &'static str, args: &[String], stdin: bool, stdout: bool) -> Result<Pipes, String> {
        let mut g = lock(&self.0);
        if g.dead {
            return Err("stopped".into());
        }
        let mut cmd = Command::new(TAP_BIN);
        cmd.args(args)
            .stdin(if stdin { Stdio::piped() } else { Stdio::null() })
            .stdout(if stdout { Stdio::piped() } else { Stdio::null() })
            .stderr(Stdio::null());
        die_with_parent(&mut cmd);
        let mut child = cmd.spawn().map_err(|e| format!("{TAP_BIN}: {e}"))?;
        let pipes = (child.stdin.take(), child.stdout.take());
        g.kids.push((role, child));
        Ok(pipes)
    }

    /// Kills and reaps every child with `role`.
    fn kill(&self, role: &str) {
        let mut g = lock(&self.0);
        let mut i = 0;
        while i < g.kids.len() {
            if g.kids[i].0 == role {
                let (_, mut c) = g.kids.remove(i);
                let _ = c.kill();
                let _ = c.wait();
            } else {
                i += 1;
            }
        }
    }

    fn kill_all(&self) {
        let mut g = lock(&self.0);
        g.dead = true;
        for (_, mut c) in g.kids.drain(..) {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

pub struct Session {
    stop: Arc<AtomicBool>,
    kids: Arc<Kids>,
}

impl Session {
    pub fn start(o: Opts, ui: Shared, chunks: Option<mpsc::Sender<(u64, [Vec<f32>; 3])>>) -> Session {
        let stop = Arc::new(AtomicBool::new(false));
        let kids = Arc::new(Kids::new());
        let gen = ui.current.fetch_add(1, Ordering::SeqCst) + 1;
        let shared = Shared { gen, ..ui };
        // A new run starts from nothing: no notes, no bands, clock at 0. Notes from the last run stay
        // until this point, so Stop → Save MIDI still works.
        if let Some(mut tl) = shared.live() {
            *tl = Timeline::new();
            shared.played.store(0, Ordering::Relaxed);
            for b in &shared.bands {
                lock(b).iter_mut().for_each(|x| *x = 0.0);
            }
        }
        set_status(&shared, "fetching models…");
        let (s2, k2) = (stop.clone(), kids.clone());
        std::thread::spawn(move || {
            if let Err(e) = session_main(&o, &shared, chunks, &s2, &k2) {
                if !s2.load(Ordering::Relaxed) {
                    set_status(&shared, format!("error: {e}"));
                }
            }
        });
        Session { stop, kids }
    }
}

impl Drop for Session {
    /// Stop flag; kill and wait every child; never blocks the GTK thread on a join.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.kids.kill_all();
    }
}

/// Where separated audio goes: playback (Stems), the rolling notes window (both), and the chunk stream (CLI).
struct Out {
    plays: Plays,
    play: Option<ChildStdin>,
    ffts: [Fft; 3],
    rings: [Vec<f32>; 3],
    chunk: [Vec<f32>; 3],
    next_chunk: u64,
    played: u64,
    last_prune: u64,
    shared: Shared,
    tx: mpsc::SyncSender<(u64, [Vec<f32>; 3])>,
    chunks: Option<mpsc::Sender<(u64, [Vec<f32>; 3])>>,
    notes_rx: mpsc::Receiver<(usize, Vec<Note>)>,
    synth: Option<Synth>,
}

/// Interleaved stereo, the last BP_WINDOW frames of a ring.
fn tail(v: &[f32]) -> Vec<f32> {
    v[v.len().saturating_sub(BP_WINDOW * CH)..].to_vec()
}

fn mix_down(stems: &[&[f32]; 3]) -> Vec<f32> {
    (0..stems[0].len()).map(|i| (stems[0][i] + stems[1][i] + stems[2][i]).clamp(-1.0, 1.0)).collect()
}

impl Out {
    /// Raw bytes straight to playback. Not counted in `played`.
    fn raw(&mut self, bytes: &[u8]) -> bool {
        match self.play.as_mut() {
            Some(p) => match p.write_all(bytes) {
                Ok(()) => p.flush().is_ok(),
                Err(_) => false,
            },
            None => false,
        }
    }

    /// Plays, shows and records one span of stems. False means playback ended and the session must stop.
    fn emit(&mut self, stems: [&[f32]; 3]) -> bool {
        let n = stems[0].len() / CH;
        if self.plays == Plays::Stems {
            let bytes: Vec<u8> = mix_down(&stems).iter().flat_map(|x| x.to_le_bytes()).collect();
            if !self.raw(&bytes) {
                set_status(&self.shared, "error: playback ended");
                return false;
            }
        }
        let cap = BP_WINDOW * CH;
        for i in 0..3 {
            let s = stems[i];
            for f in 0..n {
                self.ffts[i].push(s[2 * f], s[2 * f + 1]);
                if let Some(b) = self.ffts[i].take() {
                    if let Some(_g) = self.shared.live() {
                        *lock(&self.shared.bands[i]) = b;
                    }
                }
            }
            self.rings[i].extend_from_slice(s);
            if self.rings[i].len() > 2 * cap {
                let cut = self.rings[i].len() - cap;
                self.rings[i].drain(..cut);
            }
            self.chunk[i].extend_from_slice(s);
        }
        let before = self.played;
        self.played += n as u64;
        if let Some(_g) = self.shared.live() {
            self.shared.played.store(self.played, Ordering::Relaxed);
        }
        if self.played / CHUNK as u64 > before / CHUNK as u64 {
            let snap = [tail(&self.rings[0]), tail(&self.rings[1]), tail(&self.rings[2])];
            let _ = self.tx.try_send((self.played, snap));
        }
        let need = CHUNK * CH;
        while self.chunk[0].len() >= need {
            let mut part: [Vec<f32>; 3] = Default::default();
            for i in 0..3 {
                part[i] = self.chunk[i].drain(..need).collect();
            }
            let id = self.next_chunk;
            self.next_chunk += 1;
            if let Some(c) = &self.chunks {
                let _ = c.send((id, part));
            }
        }
        while let Ok((stem, notes)) = self.notes_rx.try_recv() {
            if self.plays == Plays::Midi {
                if let Some(s) = self.synth.as_mut() {
                    s.queue(synth_events(stem, &notes, MIDI_LAG));
                }
            }
        }
        if self.plays == Plays::Midi {
            if let Some(s) = self.synth.as_mut() {
                s.pump(self.shared.now());
            }
        }
        if self.played - self.last_prune >= 30 * RATE as u64 {
            self.last_prune = self.played;
            let before = self.shared.now() - 1200.0;
            if let Some(mut tl) = self.shared.live() {
                tl.prune(before);
            }
        }
        true
    }
}

fn wait_node(name: &str, stop: &AtomicBool) -> Result<(), String> {
    let t = Instant::now();
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err("stopped".into());
        }
        if Graph::read().is_some_and(|g| g.has(name)) {
            return Ok(());
        }
        if t.elapsed() > Duration::from_secs(30) {
            return Err(format!("{name} not found"));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn sleep_until(t: Instant) {
    let now = Instant::now();
    if t > now {
        std::thread::sleep(t - now);
    }
}

/// Startup shared by both engines: models, transcriber, playback or synth, then the engine itself.
fn session_main(o: &Opts, shared: &Shared, chunks: Option<mpsc::Sender<(u64, [Vec<f32>; 3])>>, stop: &Arc<AtomicBool>, kids: &Arc<Kids>) -> Result<(), String> {
    let names: &[&str] = match o.engine {
        SepEngine::Live => &S_FILES,
        SepEngine::Quality => &U_FILES,
    };
    let m = models::ensure(names, &|s: String| set_status(shared, s))?;
    set_status(shared, "loading…");

    let (ntx, nrx) = mpsc::channel::<(usize, Vec<Note>)>();
    let (rtx, rrx) = mpsc::sync_channel::<(u64, [Vec<f32>; 3])>(1);
    match Transcriber::load(&m) {
        Ok(tr) => {
            let (sh, sp) = (shared.clone(), stop.clone());
            std::thread::spawn(move || transcribe(tr, rrx, ntx, &sh, &sp));
        }
        Err(e) => set_status(shared, format!("notes off: {e}")),
    }

    let mut play = None;
    if o.plays == Plays::Stems {
        wait_node(&o.out, stop)?;
        let (i, _) = kids.spawn("play", &play_args(&o.out), true, false)?;
        play = i;
    }
    let synth = if o.plays == Plays::Midi {
        match Synth::spawn(&o.out) {
            Ok(s) => Some(s),
            Err(e) => {
                set_status(shared, format!("error: {e}"));
                None
            }
        }
    } else {
        None
    };

    let mut out = Out {
        plays: o.plays,
        play,
        ffts: [Fft::with_rate(RATE as f64), Fft::with_rate(RATE as f64), Fft::with_rate(RATE as f64)],
        rings: Default::default(),
        chunk: Default::default(),
        next_chunk: 0,
        played: 0,
        last_prune: 0,
        shared: shared.clone(),
        tx: rtx,
        chunks,
        notes_rx: nrx,
        synth,
    };
    match o.engine {
        SepEngine::Live => run_live(o, shared, stop, kids, &m, &mut out),
        SepEngine::Quality => run_quality(o, shared, stop, kids, m, &mut out),
    }
}

/// Live: one thread reads 128-frame blocks from the tap and runs StemgenRT on each.
fn run_live(o: &Opts, shared: &Shared, stop: &Arc<AtomicBool>, kids: &Kids, m: &ModelSet, out: &mut Out) -> Result<(), String> {
    let mut s = Streamer::load(m, PARALLEL)?;
    let playing = o.plays == Plays::Stems;
    let mut buf = o.buf_ms;
    if playing {
        if buf == 0 {
            buf = cushion_ms(&calibrate(&mut s, 345)?, BLOCK_MS);
        }
        let zeros = vec![0u8; buf as usize * 441 / 10 * CH * 4];
        if !out.raw(&zeros) {
            set_status(shared, "error: playback ended");
            return Ok(());
        }
    }
    let mut tap = open_tap(kids, o)?;
    let mut raw = vec![0u8; BLOCK * CH * 4];
    let silence = vec![0.0f32; SLICE * CH];
    let mut t_first: Option<Instant> = None;
    let (mut blocks, mut late) = (0u64, 0u64);
    let (mut win_min, mut base) = (f64::INFINITY, 0.0f64);
    let mut calls: Vec<f64> = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        if tap.read_exact(&mut raw).is_err() {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            s.reset();
            std::thread::sleep(Duration::from_millis(500));
            kids.kill("tap");
            tap = open_tap(kids, o)?;
            set_status(shared, "tap restarted");
            continue;
        }
        t_first.get_or_insert_with(Instant::now);
        let planar = deinterleave(&bytes_to_f32(&raw));
        let t = Instant::now();
        let res = s.process(&planar)?;
        calls.push(t.elapsed().as_secs_f64() * 1000.0);
        blocks += 1;
        if let Some([d, b, _v, oth]) = res {
            if !out.emit([&d[..], &b[..], &oth[..]]) {
                stop.store(true, Ordering::Relaxed);
                return Ok(());
            }
        }
        if playing {
            if let Some(t0) = t_first {
                let lag = t0.elapsed().as_secs_f64() * 1000.0 - out.played as f64 * 1000.0 / RATE as f64;
                if blocks == 345 || (blocks > 345 && blocks % 10350 == 0) {
                    base = win_min;
                    win_min = f64::INFINITY;
                }
                win_min = win_min.min(lag);
                if blocks >= 345 && lag - base > buf as f64 {
                    late += 1;
                    buf = (buf + 10).min(200);
                    if !out.emit([&silence[..], &silence[..], &silence[..]]) {
                        stop.store(true, Ordering::Relaxed);
                        return Ok(());
                    }
                }
            }
        }
        if blocks % 345 == 0 {
            let p50 = median(&calls);
            calls.clear();
            let st = if playing {
                format!("live · {} · buffer {buf} ms · model {p50:.1} ms/call · late {late}", o.tap)
            } else {
                format!("live · model {p50:.1} ms/call")
            };
            set_status(shared, st);
        }
    }
    Ok(())
}

fn open_tap(kids: &Kids, o: &Opts) -> Result<ChildStdout, String> {
    let (_, so) = kids.spawn("tap", &tap_args(&o.tap, o.tap_sink, "stems_tap"), false, true)?;
    so.ok_or_else(|| "tap has no output".to_string())
}

/// Quality: a separate thread reads, chunks and separates; the calling thread is the writer that paces the output.
fn run_quality(o: &Opts, shared: &Shared, stop: &Arc<AtomicBool>, kids: &Arc<Kids>, m: ModelSet, out: &mut Out) -> Result<(), String> {
    let plan: Arc<OnceLock<(usize, f64)>> = Arc::new(OnceLock::new());
    let t0: Arc<OnceLock<Instant>> = Arc::new(OnceLock::new());
    let sched = Arc::new(Mutex::new(Scheduler::new()));
    {
        let (tap_name, tap_sink, tier) = (o.tap.clone(), o.tap_sink, o.tier);
        let (sh, sp, kd, pl, tt, sc) = (shared.clone(), stop.clone(), kids.clone(), plan.clone(), t0.clone(), sched.clone());
        std::thread::spawn(move || {
            if let Err(e) = separate_loop(&m, &tap_name, tap_sink, tier, &sp, &kd, &pl, &tt, &sc) {
                if !sp.load(Ordering::Relaxed) {
                    set_status(&sh, format!("error: {e}"));
                }
            }
            sp.store(true, Ordering::Relaxed);
        });
    }
    let (idx, delay) = loop {
        if let Some(p) = plan.get() {
            break *p;
        }
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let t0v = loop {
        if let Some(t) = t0.get() {
            break *t;
        }
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let (_, h, p) = LADDER[idx];
    let hop = frames(h);
    let slices = hop / SLICE;
    if out.plays == Plays::Stems {
        let lead = frames(delay - h - p);
        if !out.raw(&vec![0u8; lead * CH * 4]) {
            set_status(shared, "error: playback ended");
            stop.store(true, Ordering::Relaxed);
            return Ok(());
        }
    }
    // Deadline of the sample at `frames` into the stream.
    let at = |frames: u64| t0v + Duration::from_secs_f64(delay + frames as f64 / RATE as f64);
    let silence = vec![0.0f32; SLICE * CH];
    let mut late = 0u64;
    let mut next_status = Instant::now();
    let mut id: u64 = 0;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let hop_start = id * hop as u64;
        sleep_until(at(hop_start));
        let slot = lock(&sched).take(id);
        let ok = match slot {
            Slot::Ready(r) => {
                let mut ok = true;
                for s in 0..slices {
                    sleep_until(at(hop_start + (s * SLICE) as u64));
                    let (a, b) = (s * SLICE * CH, (s + 1) * SLICE * CH);
                    if !out.emit([&r.stems[0][a..b], &r.stems[1][a..b], &r.stems[2][a..b]]) {
                        ok = false;
                        break;
                    }
                }
                ok
            }
            Slot::Late => {
                late += 1;
                let mut ok = true;
                for s in 0..slices {
                    sleep_until(at(hop_start + (s * SLICE) as u64));
                    if !out.emit([&silence[..], &silence[..], &silence[..]]) {
                        ok = false;
                        break;
                    }
                }
                ok
            }
        };
        if !ok {
            stop.store(true, Ordering::Relaxed);
            return Ok(());
        }
        if Instant::now() >= next_status {
            next_status = Instant::now() + Duration::from_secs(1);
            set_status(shared, format!("quality · tier {idx} · delay {:.0} ms · late {late}", delay * 1000.0));
        }
        id += 1;
    }
}

/// The Quality reader: picks the tier, then reads the tap, windows it, separates each window and schedules its hop.
fn separate_loop(
    m: &ModelSet,
    tap_name: &str,
    tap_sink: bool,
    tier: Option<usize>,
    stop: &AtomicBool,
    kids: &Kids,
    plan: &OnceLock<(usize, f64)>,
    t0: &OnceLock<Instant>,
    sched: &Mutex<Scheduler>,
) -> Result<(), String> {
    let mut sep = Separator::load(m)?;
    let ms = sep.ms_per_s()?;
    let (idx, delay) = match tier {
        Some(k) => {
            let k = k.min(LADDER.len() - 1);
            (k, tier_delay(k, ms))
        }
        None => pick_tier(ms),
    };
    let _ = plan.set((idx, delay));
    let (l, h, p) = LADDER[idx];
    let (left, hop) = (frames(l), frames(h));
    let mut ch = Chunker::new(left, hop, frames(p));
    let mut tap = open_tap_for(kids, tap_name, tap_sink)?;
    let mut raw = vec![0u8; 128 * CH * 4];
    let (a, b) = (left * CH, (left + hop) * CH);
    while !stop.load(Ordering::Relaxed) {
        if tap.read_exact(&mut raw).is_err() {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            return Err("source ended, turn Stems off and on".into());
        }
        t0.get_or_init(Instant::now);
        ch.push(&bytes_to_f32(&raw));
        while let Some(w) = ch.next_window() {
            let st = sep.separate(&w.pcm)?;
            let stems = [st[0][a..b].to_vec(), st[1][a..b].to_vec(), st[2][a..b].to_vec()];
            lock(sched).put(HopResult { id: w.id, stems });
        }
    }
    Ok(())
}

fn open_tap_for(kids: &Kids, name: &str, sink: bool) -> Result<ChildStdout, String> {
    let (_, so) = kids.spawn("tap", &tap_args(name, sink, "stems_tap"), false, true)?;
    so.ok_or_else(|| "tap has no output".to_string())
}

/// Consumes finished blocks: keeps notes for the window ending at `end`, adds them to the timeline and the MIDI queue.
fn transcribe(mut tr: Transcriber, rx: mpsc::Receiver<(u64, [Vec<f32>; 3])>, tx: mpsc::Sender<(usize, Vec<Note>)>, shared: &Shared, stop: &AtomicBool) {
    while let Ok((end, rings)) = rx.recv() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let end_t = end as f64 / RATE as f64;
        for stem in 0..3 {
            let kept: Vec<Note> = if stem == 0 {
                let mono = mono_tail(&rings[0], DRUM_WINDOW);
                onsets(&mono, RATE)
                    .into_iter()
                    .map(|(t, s)| Note {
                        t: end_t - DRUM_WINDOW as f64 / RATE as f64 + t,
                        dur: 0.1,
                        pitch: 0,
                        vel: (127.0 * s).round().clamp(0.0, 127.0) as u8,
                    })
                    .filter(|n| in_window(n.t, end_t))
                    .collect()
            } else {
                let mono = mono_tail(&rings[stem], BP_WINDOW);
                match tr.notes(&mono) {
                    Ok(ns) => ns
                        .into_iter()
                        .map(|n| Note { t: end_t - BP_SAMPLES as f64 / BP_RATE as f64 + n.t, ..n })
                        .filter(|n| in_window(n.t, end_t))
                        .collect(),
                    Err(e) => {
                        set_status(shared, format!("notes off: {e}"));
                        return;
                    }
                }
            };
            if let Some(mut tl) = shared.live() {
                tl.add(stem, &kept);
            }
            let _ = tx.send((stem, kept));
        }
    }
}

fn in_window(t: f64, end_t: f64) -> bool {
    t >= end_t - 1.0 && t < end_t - 0.5
}

/// Mono of the last `frames` frames of an interleaved stereo ring, zero-padded at the front.
fn mono_tail(stereo: &[f32], frames: usize) -> Vec<f32> {
    let have = (stereo.len() / CH).min(frames);
    let mut m = vec![0.0f32; frames - have];
    m.extend(stereo[stereo.len() - have * CH..].chunks_exact(CH).map(|c| 0.5 * (c[0] + c[1])));
    m
}

/// Two passive taps of `secs` each, in memory; returns (heard delay ms, r).
pub fn measure_delay(a: (&str, bool), b: (&str, bool), secs: f64) -> Result<(f64, f32), String> {
    let n = (secs * RATE as f64) as usize;
    let mut kids: Vec<Child> = Vec::new();
    let got = probe(a.0, a.1, "stems_probe_a", n, &mut kids).and_then(|ha| {
        let hb = probe(b.0, b.1, "stems_probe_b", n, &mut kids)?;
        Ok((ha, hb))
    });
    let joined = got.and_then(|(ha, hb)| {
        let pa = ha.join().map_err(|_| "probe thread panicked".to_string())??;
        let pb = hb.join().map_err(|_| "probe thread panicked".to_string())??;
        Ok((pa, pb))
    });
    for mut c in kids {
        let _ = c.kill();
        let _ = c.wait();
    }
    let (pa, pb) = joined?;
    let (lag, r) = xcorr_lag(&envelope(&pa, 44), &envelope(&pb, 44), 1000);
    Ok((lag as f64 * 44.0 / 44.1, r))
}

fn probe(target: &str, sink: bool, name: &str, n: usize, kids: &mut Vec<Child>) -> Result<std::thread::JoinHandle<Result<Vec<f32>, String>>, String> {
    let mut cmd = Command::new(TAP_BIN);
    cmd.args(tap_args(target, sink, name)).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    die_with_parent(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| format!("{TAP_BIN}: {e}"))?;
    let out = child.stdout.take();
    kids.push(child);
    let mut out = out.ok_or_else(|| "probe has no output".to_string())?;
    Ok(std::thread::spawn(move || {
        let mut raw = vec![0u8; n * CH * 4];
        out.read_exact(&mut raw).map_err(|e| format!("probe read: {e}"))?;
        Ok(bytes_to_f32(&raw))
    }))
}

/// `stems [--source ID] [--engine live|quality] [--buf-ms N] [--tier K] [--measure-delay]`
pub fn cli(args: Vec<String>) -> i32 {
    let st = stemcfg::load();
    let mut source = st.source.clone();
    let mut engine = st.engine;
    let mut measure = false;
    let mut buf_ms = 0u32;
    let mut tier: Option<usize> = None;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--source" => match it.next() {
                Some(v) => source = v,
                None => return usage("--source needs an id"),
            },
            "--engine" => {
                engine = match it.next().as_deref() {
                    Some("live") => SepEngine::Live,
                    Some("quality") => SepEngine::Quality,
                    _ => return usage("--engine live|quality"),
                }
            }
            "--buf-ms" => match it.next().and_then(|v| v.parse().ok()) {
                Some(v) => buf_ms = v,
                None => return usage("--buf-ms needs a number"),
            },
            "--tier" => match it.next().and_then(|v| v.parse::<usize>().ok()) {
                Some(v) if v < LADDER.len() => tier = Some(v),
                _ => return usage("--tier needs 0..=4"),
            },
            "--measure-delay" => measure = true,
            other => return usage(&format!("unknown argument {other}")),
        }
    }
    let cfg = Config::load();
    let Some(id) = stemcfg::resolve(&cfg, &source) else {
        eprintln!("no source channel");
        return 1;
    };
    let Some(inp) = cfg.inputs.iter().find(|i| i.id == id) else {
        eprintln!("source {id} not found");
        return 1;
    };
    let (tap_name, tap_sink) = stemcfg::tap(inp);

    if measure {
        return match measure_delay((tap_name.as_str(), tap_sink), ("ch_stems", true), 4.0) {
            Ok((ms, r)) => {
                println!("heard delay ≈ {ms:.0} ms (r={r:.2})");
                if r < 0.5 {
                    println!("low correlation: is Stems on with \"Channel plays: Stems\"?");
                }
                0
            }
            Err(e) => {
                eprintln!("{e}");
                1
            }
        };
    }

    let shared = Shared::new();
    let (tx, rx) = mpsc::channel::<(u64, [Vec<f32>; 3])>();
    let opts = Opts { tap: tap_name, tap_sink, engine, plays: Plays::Nothing, out: "ch_stems".into(), buf_ms, tier };
    let _session = Session::start(opts, shared.clone(), Some(tx));
    let eof = Arc::new(AtomicBool::new(false));
    {
        let eof = eof.clone();
        std::thread::spawn(move || {
            let mut sink = [0u8; 256];
            let mut si = std::io::stdin();
            loop {
                match si.read(&mut sink) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            eof.store(true, Ordering::Relaxed);
        });
    }
    let mut out = std::io::stdout().lock();
    loop {
        if eof.load(Ordering::Relaxed) {
            break;
        }
        let (id, blobs) = match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(v) => v,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        };
        let notes = lock(&shared.timeline).drain_all();
        let header = chunk_header(id, &notes);
        let line = serde_json::to_string(&header).unwrap_or_default();
        if writeln!(out, "{line}").is_err() {
            break;
        }
        for blob in &blobs {
            let mut bytes = Vec::with_capacity(blob.len() * 4);
            for x in blob {
                bytes.extend_from_slice(&x.to_le_bytes());
            }
            if out.write_all(&bytes).is_err() {
                return 0;
            }
        }
        if out.flush().is_err() {
            break;
        }
    }
    0
}

fn usage(msg: &str) -> i32 {
    eprintln!("stems: {msg}");
    eprintln!("usage: stems [--source ID] [--engine live|quality] [--buf-ms N] [--tier K] [--measure-delay]");
    2
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sv(a: &[String]) -> Vec<&str> {
        a.iter().map(|s| s.as_str()).collect()
    }

    fn pval(a: &[String], flag: &str) -> String {
        let i = a.iter().position(|s| s == flag).expect("flag present");
        a[i + 1].clone()
    }

    fn hop(id: u64) -> HopResult {
        HopResult { id, stems: [vec![], vec![], vec![]] }
    }

    #[test]
    fn cushion_steady() {
        assert_eq!(cushion_ms(&vec![1.0; 1000], 2.902), 10);
    }

    #[test]
    fn cushion_burst() {
        let mut v = vec![5.0; 10];
        v.extend(vec![1.0; 990]);
        assert_eq!(cushion_ms(&v, 2.9), 30);
    }

    #[test]
    fn cushion_clamps() {
        assert_eq!(cushion_ms(&vec![4.0; 1000], 2.9), 200);
    }

    #[test]
    fn pick_tier_examples() {
        let close = |a: (usize, f64), b: (usize, f64)| a.0 == b.0 && (a.1 - b.1).abs() < 1e-6;
        assert!(close(pick_tier(35.0), (4, 0.293)), "{:?}", pick_tier(35.0));
        assert!(close(pick_tier(60.0), (2, 0.556)), "{:?}", pick_tier(60.0));
        assert!(close(pick_tier(120.0), (0, 1.39)), "{:?}", pick_tier(120.0));
    }

    #[test]
    fn ladder_hops_are_slice_multiples() {
        for (left, hop, _pad) in LADDER {
            assert_eq!((hop * 44100.0).round() as usize % SLICE, 0, "hop {hop}");
            assert_eq!((left * 44100.0).round() as usize % SLICE, 0, "left {left}");
        }
    }

    #[test]
    fn chunker_first_window_is_padded() {
        let mut c = Chunker::new(44100, 22050, 22050);
        c.push(&vec![1.0f32; 88200]);
        let w = c.next_window().expect("window 0");
        assert_eq!(w.id, 0);
        assert_eq!(w.pcm.len(), 176400);
        assert!(w.pcm[..88200].iter().all(|&v| v == 0.0), "leading pad must be zeros");
        assert!(w.pcm[88200..].iter().all(|&v| v == 1.0));
        assert!(c.next_window().is_none());
    }

    #[test]
    fn chunker_hops_overlap() {
        let mut c = Chunker::new(44100, 22050, 22050);
        c.push(&vec![1.0f32; 88200]);
        let _ = c.next_window();
        c.push(&vec![2.0f32; 44100]);
        let w = c.next_window().expect("window 1");
        assert_eq!(w.id, 1);
        assert_eq!(w.pcm.len(), 176400);
        assert!(w.pcm[..44100].iter().all(|&v| v == 0.0), "22050 zero frames");
        assert!(w.pcm[44100..88200 + 44100].iter().all(|&v| v == 1.0), "44100 frames of ones");
        assert!(w.pcm[88200 + 44100..].iter().all(|&v| v == 2.0), "22050 frames of twos");
    }

    #[test]
    fn scheduler_fixed_delay() {
        let mut s = Scheduler::new();
        s.put(hop(1));
        assert!(matches!(s.take(0), Slot::Late));
        assert!(matches!(s.take(1), Slot::Ready(_)));
        s.put(hop(0));
        assert_eq!(s.pending(), 0, "a late put must not stay pending");

        let mut o = Scheduler::new();
        o.put(hop(3));
        o.put(hop(2));
        assert!(matches!(o.take(2), Slot::Ready(_)));
        assert!(matches!(o.take(3), Slot::Ready(_)));
    }

    #[test]
    fn chunk_header_serialises() {
        let h = chunk_header(0, &[vec![], vec![], vec![]]);
        assert_eq!(
            serde_json::to_string(&h).unwrap(),
            r#"{"op":"chunk","id":0,"t0":0.0,"rate":44100,"frames":22016,"stems":["drums","bass","other"],"notes":{"bass":[],"drums":[],"other":[]}}"#
        );
        let h3 = chunk_header(3, &[vec![], vec![], vec![]]);
        assert!((h3.t0 - 3.0 * 22016.0 / 44100.0).abs() < 1e-12, "t0 {}", h3.t0);
    }

    #[test]
    fn constants_consistent() {
        assert_eq!(CHUNK, CHUNK_BLOCKS * crate::sep::BLOCK);
        assert_eq!(CHUNK, 22016);
        assert_eq!(RATE, crate::dsp::SEP_RATE as usize);
        assert_eq!(BP_WINDOW, 2 * RATE);
        assert_eq!(DRUM_WINDOW * 2, 3 * RATE);
    }

    #[test]
    fn tap_args_are_passive_pw_cat() {
        let a = tap_args("ch_spotify", true, "stems_tap");
        assert_eq!(&sv(&a)[..3], &["--record", "--target", "ch_spotify"]);
        let p = pval(&a, "-P");
        assert!(p.contains("node.passive=true"), "{p}");
        assert!(p.contains("stream.capture.sink=true"), "{p}");
        assert!(p.contains("node.name=stems_tap"), "{p}");
        assert_eq!(pval(&a, "--rate"), "44100");

        let b = tap_args("easyeffects_source", false, "stems_tap");
        assert!(!pval(&b, "-P").contains("stream.capture.sink"), "a non-sink tap must not ask for a sink capture");

        let pl = play_args("ch_stems");
        assert_eq!(&sv(&pl)[..3], &["--playback", "--target", "ch_stems"]);
        assert_eq!(TAP_BIN, "pw-cat");
    }

    #[test]
    fn no_audio_file_io() {
        // Needles are built at runtime so this test does not match its own text.
        let needles = [
            ["File", "::create"].concat(),
            ["fs::", "write"].concat(),
            ["Open", "Options"].concat(),
            ["temp", "file"].concat(),
            [".", "wav"].concat(),
        ];
        let sources = [
            ("stems.rs", include_str!("stems.rs")),
            ("sep.rs", include_str!("sep.rs")),
            ("dsp.rs", include_str!("dsp.rs")),
            ("notes.rs", include_str!("notes.rs")),
        ];
        for (name, src) in sources {
            for n in &needles {
                assert!(!src.contains(n.as_str()), "{name} matches {n}");
            }
        }
    }

    /// A handle from an older session cannot add notes or set status once a newer session has started.
    #[test]
    fn stale_session_handle_cannot_write() {
        let ui = Shared::new();
        let old = Shared { gen: ui.current.fetch_add(1, Ordering::SeqCst) + 1, ..ui.clone() };
        set_status(&old, "old run");
        assert_eq!(*lock(&ui.status), "old run", "the current session should write its status");
        if let Some(mut tl) = old.live() {
            tl.add(1, &[Note { t: 1.0, dur: 0.5, pitch: 40, vel: 90 }]);
        }
        let newer = Shared { gen: ui.current.fetch_add(1, Ordering::SeqCst) + 1, ..ui.clone() };
        assert!(old.live().is_none(), "a handle from a replaced session must be refused");
        set_status(&old, "late write");
        assert_eq!(*lock(&ui.status), "old run", "a stale status write got through");
        let n: usize = newer.live().expect("the newer handle is current").drain_all().iter().map(|v| v.len()).sum();
        assert_eq!(n, 1, "only the note written before the replacement may be there");
    }

    /// Grids for one 2 s window starting at `ws`: each (on, off, column) is held at 0.9 on the frames it covers,
    /// with an onset at the first frame when the note starts inside this window.
    fn held_win(ws: f64, notes: &[(f64, f64, usize)]) -> crate::sep::Grids {
        let mut note = vec![vec![0.0f32; 88]; 172];
        let mut onset = vec![vec![0.0f32; 88]; 172];
        for &(on, off, p) in notes {
            let mut first = true;
            for f in 0..172 {
                let t = ws + f as f64 / crate::sep::FPS;
                if t >= on && t < off {
                    note[f][p] = 0.9;
                    if first && on >= ws {
                        onset[f][p] = 0.9;
                    }
                    first = false;
                }
            }
        }
        crate::sep::Grids { note, onset }
    }

    /// Runs the carry-over over windows k in ks (window k ends at k * CHUNK / RATE) and feeds a Timeline.
    fn run_windows(
        notes: &[(f64, f64, usize)],
        ks: std::ops::RangeInclusive<usize>,
        extra_onset: Option<(f64, usize)>,
    ) -> (crate::notes::Timeline, Vec<crate::notes::Note>, usize) {
        let step = CHUNK as f64 / RATE as f64;
        let win_len = BP_WINDOW as f64 / RATE as f64;
        let mut open = Vec::new();
        let mut tl = crate::notes::Timeline::new();
        let mut ext_windows = 0;
        for k in ks {
            let end_t = k as f64 * step;
            let ws = end_t - win_len;
            let mut g = held_win(ws, notes);
            if let Some((at, p)) = extra_onset {
                let f = ((at - ws) * crate::sep::FPS).round();
                if f >= 0.0 && (f as usize) < 172 {
                    g.onset[f as usize][p] = 0.95;
                }
            }
            let (new, ext) = window_notes(Some(&mut g), end_t, &mut open);
            tl.add(1, &new);
            if !ext.is_empty() {
                ext_windows += 1;
            }
            tl.extend(1, &ext);
        }
        let out = tl.window(1, 0.0, 100.0);
        (tl, out, ext_windows)
    }

    #[test]
    fn window_notes_joins_held_note_across_windows() {
        let (_, out, ext_windows) = run_windows(&[(1.0, 5.0, 48)], 4..=14, None);
        assert_eq!(out.len(), 1, "one held note must come out as one note: {out:?}");
        let m = &out[0];
        assert_eq!(m.pitch, 69);
        assert!((m.t - 1.0).abs() <= 1.0 / crate::sep::FPS, "onset kept: {}", m.t);
        assert!((m.dur - 4.0).abs() <= 1.0 / crate::sep::FPS + 1e-9, "full length: {}", m.dur);
        assert_eq!(m.vel, 114);
        assert!(ext_windows >= 6, "the note was extended in {ext_windows} windows");
    }

    #[test]
    fn window_notes_closes_when_next_window_quiet() {
        let step = CHUNK as f64 / RATE as f64;
        let win_len = BP_WINDOW as f64 / RATE as f64;
        let mut open = Vec::new();
        let mut g4 = held_win(4.0 * step - win_len, &[(1.0, 2.3, 48)]);
        let (new4, _) = window_notes(Some(&mut g4), 4.0 * step, &mut open);
        let mut tl = crate::notes::Timeline::new();
        tl.add(1, &new4);
        let mut g5 = held_win(5.0 * step - win_len, &[(1.0, 2.3, 48)]);
        let (new5, ext5) = window_notes(Some(&mut g5), 5.0 * step, &mut open);
        tl.add(1, &new5);
        tl.extend(1, &ext5);
        let out = tl.window(1, 0.0, 100.0);
        assert_eq!(out.len(), 1, "{out:?}");
        assert!((out[0].dur - 1.3).abs() <= 1.0 / crate::sep::FPS, "closed at the real end: {}", out[0].dur);
        assert!(open.is_empty());

        // a silent window closes the open note at its current end, with no extension
        let mut open = Vec::new();
        let mut g4 = held_win(4.0 * step - win_len, &[(1.0, 2.3, 48)]);
        let (first, _) = window_notes(Some(&mut g4), 4.0 * step, &mut open);
        assert_eq!(open.len(), 1);
        let (none_new, none_ext) = window_notes(None, 5.0 * step, &mut open);
        assert!(none_new.is_empty() && none_ext.is_empty());
        assert!(open.is_empty());
        assert!(first[0].dur > 0.9, "keeps its first-window length: {}", first[0].dur);
    }

    #[test]
    fn window_notes_does_not_duplicate_the_held_pitch() {
        let (_, out, _) = run_windows(&[(1.0, 5.0, 48)], 4..=14, Some((3.0, 48)));
        assert_eq!(out.len(), 1, "a re-onset inside the sustain must not start a second note: {out:?}");
        assert!((out[0].dur - 4.0).abs() <= 1.0 / crate::sep::FPS + 1e-9, "full length: {}", out[0].dur);
    }

    #[test]
    fn window_notes_random_held_notes_come_out_whole() {
        let mut s = 11u64;
        let mut lcg = || {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((s >> 33) as f64) / (1u64 << 31) as f64
        };
        let mut notes: Vec<(f64, f64, usize)> = Vec::new();
        while notes.len() < 5 {
            let p = 30 + (lcg() * 40.0) as usize;
            if notes.iter().any(|x| x.2 == p) {
                continue;
            }
            let on = 0.5 + lcg() * 2.5;
            let dur = 0.3 + lcg() * 3.7;
            notes.push((on, on + dur, p));
        }
        // from k = 1: a note starting before 1.0 s is emitted by window k = 3 and must not be skipped
        let (_, out, _) = run_windows(&notes, 1..=20, None);
        assert_eq!(out.len(), 5, "{out:?}");
        let tol = 1.0 / crate::sep::FPS + 1e-9;
        for &(on, off, p) in &notes {
            let m = out.iter().find(|m| m.pitch as usize == p + 21).unwrap_or_else(|| panic!("pitch {} missing", p + 21));
            assert!((m.t - on).abs() <= tol, "onset of pitch {}: {} vs {on}", p + 21, m.t);
            assert!((m.dur - (off - on)).abs() <= tol, "length of pitch {}: {} vs {}", p + 21, m.dur, off - on);
        }
    }
}
