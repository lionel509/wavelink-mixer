//! Stems session: taps a channel, separates it, plays and/or shows the stems, and writes notes to the MIDI channel.
#![allow(dead_code)]
use crate::config::Config;
use crate::dash::{Fft, BANDS};
use crate::dsp::{envelope, onsets, xcorr_lag, BP_RATE};
use crate::models::{self, ModelSet, S_FILES, U_FILES};
use crate::notes::{synth_events, Note, Synth, Timeline, MIDI_LAG, STEMS};
use crate::pw::{die_with_parent, Graph};
use crate::sep::{calibrate, deinterleave, frames_to_notes_open, next_strike, walk_end, Grids, FRAME_THRESH, MIN_NOTE_FRAMES, Separator, Streamer, Transcriber, BLOCK, BLOCK_MS, BP_SAMPLES, FPS, PARALLEL};
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
/// #77: the Mac capture children. Same stdout contract as pw-cat: raw f32 LE, 44100 Hz, 2 ch.
pub const MAC_TAP: &str = "mac:system";
pub const MAC_TAP_MUTE: &str = "mac:system:mute";
pub const WAV_TAP: &str = "wav:";
/// The capture child for a tap target: (program, args). Compile-only skeleton for #77 U4.
pub fn tap_cmd(target: &str, capture_sink: bool, name: &str) -> (String, Vec<String>) {
    let me = || std::env::current_exe().map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|_| "wavelink-mixer".into());
    if let Some(file) = target.strip_prefix(WAV_TAP) {
        return (me(), vec!["wav-tap".to_string(), file.to_string()]);
    }
    if target == MAC_TAP {
        return (me(), vec!["mac-tap".to_string()]);
    }
    if target == MAC_TAP_MUTE {
        return (me(), vec!["mac-tap".to_string(), "--mute".to_string()]);
    }
    (TAP_BIN.to_string(), tap_args(target, capture_sink, name))
}
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
    /// Lengthened notes sent again: [t - t0, dur, pitch, vel]. Absent when empty.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub ext: BTreeMap<&'static str, Vec<[f64; 4]>>,
}

/// t0 = id * 22016 / 44100; notes as [t - t0, dur, pitch, vel].
pub fn chunk_header(id: u64, notes: &[Vec<Note>; 3]) -> ChunkHeader {
    let t0 = id as f64 * CHUNK as f64 / RATE as f64;
    let mut map = BTreeMap::new();
    for (i, name) in STEMS.iter().enumerate() {
        let v = notes[i].iter().map(|n| [n.t - t0, n.dur, n.pitch as f64, n.vel as f64]).collect();
        map.insert(*name, v);
    }
    ChunkHeader { op: "chunk", id, t0, rate: RATE as u32, frames: CHUNK, stems: STEMS, notes: map, ext: BTreeMap::new() }
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

    fn spawn(&self, role: &'static str, bin: &str, args: &[String], stdin: bool, stdout: bool) -> Result<Pipes, String> {
        let mut g = lock(&self.0);
        if g.dead {
            return Err("stopped".into());
        }
        let mut cmd = Command::new(bin);
        cmd.args(args)
            .stdin(if stdin { Stdio::piped() } else { Stdio::null() })
            .stdout(if stdout { Stdio::piped() } else { Stdio::null() })
            // pw-cat keeps its quiet stderr; our own capture children (mac-tap, wav-tap) show why they died.
            .stderr(if bin == TAP_BIN { Stdio::null() } else { Stdio::inherit() });
        die_with_parent(&mut cmd);
        let mut child = cmd.spawn().map_err(|e| format!("{bin}: {e}"))?;
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
    current: Arc<AtomicU64>,
}

impl Session {
    pub fn start(o: Opts, ui: Shared, chunks: Option<mpsc::Sender<(u64, [Vec<f32>; 3])>>) -> Session {
        let stop = Arc::new(AtomicBool::new(false));
        let kids = Arc::new(Kids::new());
        let gen = ui.current.fetch_add(1, Ordering::SeqCst) + 1;
        let current = ui.current.clone();
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
        Session { stop, kids, current }
    }
}

impl Drop for Session {
    /// Stop flag; kill and wait every child; never blocks the GTK thread on a join.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // No later write from this session: a block still in separation is refused by `Shared::live`.
        self.current.fetch_add(1, Ordering::SeqCst);
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
    notes_rx: mpsc::Receiver<(usize, Vec<Note>, Vec<(f64, Note)>)>,
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
        while let Ok((stem, notes, ext)) = self.notes_rx.try_recv() {
            if self.plays == Plays::Midi {
                if let Some(s) = self.synth.as_mut() {
                    s.queue(synth_events(stem, &notes, MIDI_LAG));
                    s.retime(stem, &ext, MIDI_LAG);
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

    let (ntx, nrx) = mpsc::channel::<(usize, Vec<Note>, Vec<(f64, Note)>)>();
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
        let (i, _) = kids.spawn("play", TAP_BIN, &play_args(&o.out), true, false)?;
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
    let mut dead = 0u32;
    while !stop.load(Ordering::Relaxed) {
        if tap.read_exact(&mut raw).is_err() {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            dead += 1;
            if dead >= 3 {
                return Err(format!("the capture exited {dead} times in a row with no audio; see the message above"));
            }
            s.reset();
            std::thread::sleep(Duration::from_millis(500));
            kids.kill("tap");
            tap = open_tap(kids, o)?;
            set_status(shared, "tap restarted");
            continue;
        }
        dead = 0;
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
    let (_, so) = {
        let (bin, args) = tap_cmd(&o.tap, o.tap_sink, "stems_tap");
        kids.spawn("tap", &bin, &args, false, true)
    }?;
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
    let (_, so) = {
        let (bin, args) = tap_cmd(name, sink, "stems_tap");
        kids.spawn("tap", &bin, &args, false, true)
    }?;
    so.ok_or_else(|| "tap has no output".to_string())
}

/// Consumes finished blocks: keeps notes for the window ending at `end`, adds them to the timeline and the MIDI queue.
fn transcribe(mut tr: Transcriber, rx: mpsc::Receiver<(u64, [Vec<f32>; 3])>, tx: mpsc::Sender<(usize, Vec<Note>, Vec<(f64, Note)>)>, shared: &Shared, stop: &AtomicBool) {
    let mut open: [Vec<Note>; 3] = Default::default();
    while let Ok((end, rings)) = rx.recv() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let end_t = end as f64 / RATE as f64;
        for stem in 0..3 {
            let (kept, ext): (Vec<Note>, Vec<(f64, Note)>) = if stem == 0 {
                let mono = mono_tail(&rings[0], DRUM_WINDOW);
                let kept = onsets(&mono, RATE)
                    .into_iter()
                    .map(|(t, s)| Note {
                        t: end_t - DRUM_WINDOW as f64 / RATE as f64 + t,
                        dur: 0.1,
                        pitch: 0,
                        vel: (127.0 * s).round().clamp(0.0, 127.0) as u8,
                    })
                    .filter(|n| in_window(n.t, end_t))
                    .collect();
                (kept, Vec::new())
            } else {
                let mono = mono_tail(&rings[stem], BP_WINDOW);
                match tr.grids(&mono) {
                    Ok(mut g) => window_notes(g.as_mut(), end_t, &mut open[stem]),
                    Err(e) => {
                        set_status(shared, format!("notes off: {e}"));
                        release(shared);
                        return;
                    }
                }
            };
            commit(shared, stem, &kept, &ext, &open[stem]);
            let _ = tx.send((stem, kept, ext));
        }
    }
    release(shared);
}

/// Notes of one stem for the window ending at `end_t`, with the carry-over of held notes.
///
/// A note whose scan reaches the window edge is emitted where its onset lands and kept in `open`.
/// In the next window the open note is walked on the fresh grid from its last known end, with the
/// same ENERGY_TOL rule as Basic Pitch; its grid cells are zeroed so no duplicate note is made.
/// Returns the new notes and `(previous end, note with its new dur)` for each extension.
/// A silent window (`None`) closes every open note at its current end.
pub fn window_notes(g: Option<&mut Grids>, end_t: f64, open: &mut Vec<Note>) -> (Vec<Note>, Vec<(f64, Note)>) {
    let ws = end_t - BP_SAMPLES as f64 / BP_RATE as f64;
    let Some(g) = g else {
        open.clear();
        return (Vec::new(), Vec::new());
    };
    let frames = g.note.len();
    let mut ext: Vec<(f64, Note)> = Vec::new();
    let mut still: Vec<Note> = Vec::new();
    for n in open.drain(..) {
        let p = n.pitch as usize - 21;
        let old_end = n.t + n.dur;
        let mut f0 = ((old_end - ws) * FPS).floor();
        if f0 < 1.0 {
            // Windows were dropped and this one starts past the note's last known end: bridge the gap
            // when the pitch is still sounding at the window start, else the note is over.
            if g.note.len() < 2 || g.note[1][p] < FRAME_THRESH {
                continue;
            }
            f0 = 1.0;
        }
        if f0 as usize + 1 >= frames {
            continue;
        }
        let f0 = f0 as usize;
        let (mut end, mut at_edge) = walk_end(&g.note, f0 - 1, p); // starts by testing frame f0
        // A raw strike of this pitch (#59) ends the carried note where it goes quiet, and is left for a new note.
        // Only a strike this window keeps may close the note; a later one leaves the note open up to it.
        let own = ((n.t - ws) * FPS).round() as i64 + MIN_NOTE_FRAMES as i64 + 1;
        let keep0 = ((end_t - 1.0 - ws) * FPS).ceil() as i64;
        let from = own.max(keep0).max(1) as usize;
        let strike = next_strike(&g.note, &g.onset, p, from, end);
        if let Some(fr) = strike {
            end = (0..fr).rev().find(|&f| g.note[f][p] >= FRAME_THRESH).map_or(fr, |f| f + 1);
            at_edge = !in_window(ws + fr as f64 / FPS, end_t);
        }
        let new_end = ws + end as f64 / FPS;
        if strike.is_none() && new_end <= old_end {
            continue;
        }
        let u = Note { dur: new_end - n.t, ..n.clone() };
        if (new_end - old_end).abs() > 1e-9 {
            ext.push((old_end, u.clone()));
        }
        if at_edge {
            still.push(u);
        }
        for row in g.note.iter_mut().take(strike.unwrap_or(end)) {
            row[p] = 0.0;
        }
    }
    *open = still;
    let mut new: Vec<Note> = Vec::new();
    // A new note whose re-strike is past this window's keep slice stays open for the next window (review #69 f2).
    let keep_end = ((end_t - 0.5 - ws) * crate::sep::FPS).ceil() as usize;
    for (n, at_edge) in crate::sep::frames_to_notes_open_keep(&g.note, &g.onset, keep_end) {
        let t = ws + n.t;
        if !in_window(t, end_t) {
            continue;
        }
        let n = Note { t, ..n };
        if at_edge {
            open.push(n.clone());
        }
        new.push(n);
    }
    (new, ext)
}

/// Hands one window's notes to the timeline: `kept` are added, `ext` lengthens, `open` are the notes a later
/// window may still change (Save leaves them for the next Save).
pub(crate) fn commit(shared: &Shared, stem: usize, kept: &[Note], ext: &[(f64, Note)], open: &[Note]) {
    if let Some(mut tl) = shared.live() {
        tl.add(stem, kept);
        tl.extend(stem, ext);
        tl.set_open(stem, open);
    }
}

/// The stream of this session ends: every held note is closed, so the next Save or chunk writes it.
pub(crate) fn release(shared: &Shared) {
    if let Some(mut tl) = shared.live() {
        tl.close_all();
    }
}

/// Splits the closed and held notes of one chunk into the rows to send as notes and as ext. A note already sent
/// is sent again only when its length changed. `sent` becomes this chunk's held notes.
pub fn stream_split(sent: &mut [Vec<Note>; 3], closed: [Vec<Note>; 3], held: [Vec<Note>; 3]) -> ([Vec<Note>; 3], [Vec<Note>; 3]) {
    let mut notes: [Vec<Note>; 3] = Default::default();
    let mut ext: [Vec<Note>; 3] = Default::default();
    for stem in 0..3 {
        for n in closed[stem].iter().chain(held[stem].iter()) {
            match sent[stem].iter().find(|s| s.t == n.t && s.pitch == n.pitch) {
                Some(s) if s.dur != n.dur => ext[stem].push(n.clone()),
                Some(_) => {}
                None => notes[stem].push(n.clone()),
            }
        }
        sent[stem] = held[stem].clone();
    }
    (notes, ext)
}

/// The JSON line of one engine chunk: the notes that are new, and the ext rows of the held notes that grew.
pub(crate) fn chunk_line(shared: &Shared, sent: &mut [Vec<Note>; 3], id: u64) -> String {
    let (closed, held) = {
        let mut tl = lock(&shared.timeline);
        (tl.drain_closed(), tl.held())
    };
    let (notes, ext) = stream_split(sent, closed, held);
    let mut header = chunk_header(id, &notes);
    for (i, name) in STEMS.iter().enumerate() {
        if ext[i].is_empty() {
            continue;
        }
        let rows = ext[i].iter().map(|n| [n.t - header.t0, n.dur, n.pitch as f64, n.vel as f64]).collect();
        header.ext.insert(*name, rows);
    }
    serde_json::to_string(&header).unwrap_or_default()
}

pub(crate) fn in_window(t: f64, end_t: f64) -> bool {
    t >= end_t - 1.0 && t < end_t - 0.5
}

/// Mono of the last `frames` frames of an interleaved stereo ring, zero-padded at the front.
pub(crate) fn mono_tail(stereo: &[f32], frames: usize) -> Vec<f32> {
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
    let mut sent: [Vec<Note>; 3] = Default::default();
    loop {
        if eof.load(Ordering::Relaxed) {
            break;
        }
        let (id, blobs) = match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(v) => v,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        };
        let line = chunk_line(&shared, &mut sent, id);
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
    fn tap_cmd_is_pw_cat_for_every_pipewire_target() {
        // I2: the Linux capture child does not change. Generated targets over ids and names.
        let alphabet: Vec<char> = "abcdefghijklmnopqrstuvwxyz0123456789._-".chars().collect();
        let mut s: u64 = 77;
        for _ in 0..300 {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let len = 1 + (s >> 33) as usize % 12;
            let mut id = String::new();
            for _ in 0..len {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                id.push(alphabet[(s >> 33) as usize % alphabet.len()]);
            }
            for target in [crate::config::names::ch(&id), id.clone()] {
                for sink in [false, true] {
                    assert_eq!(tap_cmd(&target, sink, "stems_tap"), (TAP_BIN.to_string(), tap_args(&target, sink, "stems_tap")));
                }
            }
        }
        assert_eq!(tap_cmd("ch_spotify", true, "stems_tap").0, "pw-cat");
    }

    #[test]
    fn tap_cmd_wav_and_mac_run_this_binary() {
        let me = std::env::current_exe().unwrap().to_string_lossy().to_string();
        let file = ["/tmp/x", "wav"].join(".");
        assert_eq!(tap_cmd(&format!("wav:{file}"), false, "stems_tap"), (me.clone(), vec!["wav-tap".to_string(), file]));
        assert_eq!(tap_cmd(MAC_TAP, false, "stems_tap"), (me.clone(), vec!["mac-tap".to_string()]));
        assert_eq!(tap_cmd(MAC_TAP_MUTE, false, "stems_tap"), (me, vec!["mac-tap".to_string(), "--mute".to_string()]));
    }

    #[test]
    fn breaker_r7b_prune_drops_unsaved_final_note_after_20_minutes() {
        // #75 as written in the issue: a final note from a long session survives a 20-minute prune.
        let mut tl = Timeline::new();
        tl.add(1, &[Note { t: 0.0, dur: 0.5, pitch: 40, vel: 90 }]);
        tl.set_open(1, &[]);
        tl.set_open(1, &[]);
        tl.prune(1300.0 - 1200.0 + 1.0);
        assert_eq!(tl.drain_all()[1].len(), 1);
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
        let win_len = BP_SAMPLES as f64 / BP_RATE as f64;
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
        let win_len = BP_SAMPLES as f64 / BP_RATE as f64;
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
    fn window_notes_bridges_four_or_more_dropped_windows() {
        let ks: Vec<usize> = (4..=14).filter(|k| !(15..=18).contains(k)).chain(19..=24).collect();
        let (_, out, _) = run_windows(&[(1.0, 12.0, 48)], 4..=4, None);
        let _ = out;
        for drop in 1..=7usize {
            let ks: Vec<usize> = ks.iter().copied().filter(|k| !(15..15 + drop).contains(k)).chain(15 + drop..=24).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
            let step = CHUNK as f64 / RATE as f64;
            let win_len = BP_SAMPLES as f64 / BP_RATE as f64;
            let mut open = Vec::new();
            let mut tl = crate::notes::Timeline::new();
            for k in ks {
                let end_t = k as f64 * step;
                let mut g = held_win(end_t - win_len, &[(1.0, 12.0, 48)]);
                let (new, ext) = window_notes(Some(&mut g), end_t, &mut open);
                tl.add(1, &new);
                tl.extend(1, &ext);
            }
            let out = tl.window(1, 0.0, 100.0);
            let total: f64 = out.iter().map(|n| n.dur).sum();
            assert_eq!(out.len(), 1, "drop {drop}: {out:?}");
            assert!((total - 11.0).abs() < 0.1, "drop {drop}: dur {total:.3}");
        }
    }

    #[test]
    fn window_notes_restrike_inside_carry_tail_is_kept() {
        let ks: Vec<usize> = (4..=14).collect();
        let (_, out, _) = run_windows(&[(1.0, 3.0, 48), (3.1, 5.0, 48)], 4..=14, None);
        let _ = ks;
        assert_eq!(out.len(), 2, "re-strike swallowed: {out:?}");
    }

    /// Pin for the grid origin: frame 0 of a production window sits at `end_t - BP_SAMPLES / BP_RATE`.
    /// A note onset at frame 100 (inside the kept [end_t - 1, end_t - 0.5) range) must land at that origin plus 100 frames, not 2.0 s before `end_t`.
    #[test]
    fn window_notes_uses_production_grid_origin() {
        let end_t = 10.0;
        let ws = end_t - BP_SAMPLES as f64 / BP_RATE as f64;
        let mut g = crate::sep::Grids { note: vec![vec![0.0f32; 88]; 172], onset: vec![vec![0.0f32; 88]; 172] };
        for f in 100..120 {
            g.note[f][48] = 0.9;
        }
        g.onset[100][48] = 0.9;
        let mut open = Vec::new();
        let (new, _) = window_notes(Some(&mut g), end_t, &mut open);
        assert_eq!(new.len(), 1, "{new:?}");
        let want = ws + 100.0 / crate::sep::FPS;
        assert!((new[0].t - want).abs() < 1e-9, "onset {} vs production origin {want}", new[0].t);
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

    /// Uniform [0, 1) from an LCG, as f64.
    fn lcg_f(s: &mut u64) -> f64 {
        *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((*s >> 33) as f64) / (1u64 << 31) as f64
    }

    /// Back-to-back notes of columns 40 and 52 from one seed, as (on, off, column). A third of the gaps are zero.
    fn chain_notes(seed: u64) -> Vec<(f64, f64, usize)> {
        let mut s = seed;
        let mut out = Vec::new();
        for p in [40usize, 52] {
            let mut t = 0.6 + lcg_f(&mut s) * 0.9;
            while t < 9.0 {
                let dur = 0.25 + lcg_f(&mut s) * 2.75;
                out.push((t, t + dur, p));
                let g = lcg_f(&mut s);
                let gap = if g < 1.0 / 3.0 {
                    0.0
                } else if g < 2.0 / 3.0 {
                    0.03 + lcg_f(&mut s) * 0.07
                } else {
                    0.3 + lcg_f(&mut s) * 0.7
                };
                t += dur + gap;
            }
        }
        out
    }

    /// Feeds the windows `ks` (any subset, in order) through window_notes and returns the timeline's notes.
    fn carry_out(notes: &[(f64, f64, usize)], ks: &[usize]) -> Vec<crate::notes::Note> {
        let step = CHUNK as f64 / RATE as f64;
        let win_len = BP_SAMPLES as f64 / BP_RATE as f64;
        let mut open = Vec::new();
        let mut tl = crate::notes::Timeline::new();
        for &k in ks {
            let end_t = k as f64 * step;
            let mut g = held_win(end_t - win_len, notes);
            let (new, ext) = window_notes(Some(&mut g), end_t, &mut open);
            tl.add(1, &new);
            tl.extend(1, &ext);
        }
        tl.window(1, 0.0, 100.0)
    }

    /// The engine client of the chunk stream (bass rows only): adds each `notes` row, and lengthens the note
    /// that an `ext` row names by its t and pitch.
    fn apply_chunk(client: &mut Vec<Note>, line: &str) {
        let v: serde_json::Value = serde_json::from_str(line).expect("chunk line is JSON");
        let t0 = v["t0"].as_f64().expect("t0");
        for r in v["notes"]["bass"].as_array().into_iter().flatten() {
            let r: Vec<f64> = r.as_array().unwrap().iter().map(|x| x.as_f64().unwrap()).collect();
            client.push(Note { t: t0 + r[0], dur: r[1], pitch: r[2] as u8, vel: r[3] as u8 });
        }
        for r in v["ext"]["bass"].as_array().into_iter().flatten() {
            let r: Vec<f64> = r.as_array().unwrap().iter().map(|x| x.as_f64().unwrap()).collect();
            let t = t0 + r[0];
            if let Some(m) = client.iter_mut().find(|m| m.pitch == r[2] as u8 && (m.t - t).abs() < 1e-6) {
                m.dur = r[1];
            }
        }
    }

    #[test]
    fn breaker59_touching_same_pitch_notes_stay_two() {
        let (_, mut out, _) = run_windows(&[(1.0, 3.0, 48), (3.0, 5.0, 48)], 4..=14, None);
        out.sort_by(|a, b| a.t.total_cmp(&b.t));
        assert_eq!(out.len(), 2, "{out:?}");
        let tol = 1.5 / crate::sep::FPS;
        assert!((out[0].t + out[0].dur - 3.0).abs() <= tol, "first note must end at 3.0: {out:?}");
        assert!((out[1].t - 3.0).abs() <= tol && (out[1].dur - 2.0).abs() <= tol, "second note must be 3.0 + 2.0: {out:?}");
    }

    #[test]
    fn breaker59_restrike_onset_inside_sustain_starts_new_note() {
        let (_, mut out, _) = run_windows(&[(1.0, 5.0, 48)], 4..=14, Some((3.0, 48)));
        out.sort_by(|a, b| a.t.total_cmp(&b.t));
        assert_eq!(out.len(), 2, "a re-strike is a new note: {out:?}");
        let tol = 1.5 / crate::sep::FPS;
        assert!((out[0].t + out[0].dur - out[1].t).abs() <= tol, "the first ends where the re-strike starts: {out:?}");
        let total: f64 = out.iter().map(|n| n.dur).sum();
        assert!((total - 4.0).abs() <= 2.0 / crate::sep::FPS, "the two lengths sum to the sustain: {total}");
    }

    #[test]
    fn window_notes_restrike_within_one_keep_slice_is_kept() {
        let (_, out, _) = run_windows(&[(1.0, 1.3, 48), (1.3, 3.0, 48)], 4..=12, None);
        assert_eq!(out.len(), 2, "re-strike inside one keep slice swallowed: {out:?}");
    }

    #[test]
    fn window_notes_restrike_near_the_window_edge_shortens_the_carried_note() {
        let (_, mut out, _) = run_windows(&[(1.2, 1.95, 48), (1.95, 3.0, 48)], 4..=12, None);
        out.sort_by(|a, b| a.t.total_cmp(&b.t));
        assert_eq!(out.len(), 2, "{out:?}");
        assert!((out[0].t + out[0].dur - 1.95).abs() <= 1.5 / crate::sep::FPS, "carried note must end at the re-strike: {out:?}");
    }

    /// #62b: a carried note whose last known end is exactly the window start is bridged, not dropped.
    #[test]
    fn window_notes_bridge_at_the_exact_boundary() {
        let end_t = 10.0;
        let ws = end_t - BP_SAMPLES as f64 / BP_RATE as f64;
        let fresh = || crate::sep::Grids { note: vec![vec![0.9f32; 88]; 172], onset: vec![vec![0.0f32; 88]; 172] };
        for end in [ws + 0.5 / crate::sep::FPS, ws] {
            let mut open = vec![Note { t: ws - 0.5, dur: end - (ws - 0.5), pitch: 69, vel: 100 }];
            let mut g = fresh();
            let (_, ext) = window_notes(Some(&mut g), end_t, &mut open);
            assert_eq!(ext.len(), 1, "carried note with old end {end}: {ext:?}");
            assert_eq!(open.len(), 1, "the note still sounds, so it stays open");
        }
    }

    /// Pins #62a: the repeat's onset was never delivered, so the gap is bridged into one note.
    #[test]
    fn window_notes_gap_bridge_is_one_note_when_the_repeat_onset_was_dropped() {
        let ks: Vec<usize> = (4..=7).chain(16..=22).collect();
        let out = carry_out(&[(1.0, 5.6, 48), (5.8, 10.0, 48)], &ks);
        assert_eq!(out.len(), 1, "{out:?}");
        assert!((out[0].t - 1.0).abs() <= 1.5 / crate::sep::FPS, "{out:?}");
        assert!((out[0].dur - 9.0).abs() <= 0.1, "{out:?}");
    }

    /// Review finding 2 (#69): a new note whose re-strike is past the window's keep slice (2.3 s, keep [1.6, 2.1))
    /// is seen once, and gone in the next window. The note must stay open and come out whole.
    #[test]
    fn window_notes_new_note_restrike_past_keep_seen_once_stays_whole() {
        let win_len = BP_SAMPLES as f64 / BP_RATE as f64;
        let notes = [(1.6, 3.0, 48)];
        let mut open = Vec::new();
        let mut tl = crate::notes::Timeline::new();
        for end_t in [2.1, 2.6, 3.1, 3.6] {
            let ws = end_t - win_len;
            let mut g = held_win(ws, &notes);
            if (end_t - 2.6f64).abs() < 1e-9 {
                g.onset[((2.3 - ws) * crate::sep::FPS).round() as usize][48] = 0.6;
            }
            let (new, ext) = window_notes(Some(&mut g), end_t, &mut open);
            tl.add(1, &new);
            tl.extend(1, &ext);
        }
        let out = tl.window(1, 0.0, 100.0);
        assert_eq!(out.len(), 1, "{out:?}");
        assert!((out[0].t - 1.6).abs() <= 1.5 / crate::sep::FPS, "{out:?}");
        assert!((out[0].dur - 1.4).abs() <= 1.5 / crate::sep::FPS, "{out:?}");
    }

    /// I59 and I62 on generated chains: one note per strike, no same-pitch overlap, at most one open note per pitch.
    #[test]
    fn carry_invariant_one_note_per_strike() {
        let tol = 1.5 / crate::sep::FPS;
        let step = CHUNK as f64 / RATE as f64;
        let win_len = BP_SAMPLES as f64 / BP_RATE as f64;
        for seed in 1..=40u64 {
            let chain = chain_notes(seed);
            let mut open: Vec<Note> = Vec::new();
            let mut tl = crate::notes::Timeline::new();
            for k in 1..=28usize {
                let end_t = k as f64 * step;
                let mut g = held_win(end_t - win_len, &chain);
                let (new, ext) = window_notes(Some(&mut g), end_t, &mut open);
                tl.add(1, &new);
                tl.extend(1, &ext);
                let mut ps: Vec<u8> = open.iter().map(|n| n.pitch).collect();
                ps.sort();
                ps.dedup();
                assert_eq!(ps.len(), open.len(), "seed {seed} window {k}: two open notes share a pitch");
            }
            let out = tl.window(1, 0.0, 100.0);
            for p in [40usize, 52] {
                let mut want: Vec<(f64, f64)> = chain.iter().filter(|c| c.2 == p).map(|c| (c.0, c.1)).collect();
                let mut got: Vec<(f64, f64)> = out.iter().filter(|n| n.pitch as usize == p + 21).map(|n| (n.t, n.t + n.dur)).collect();
                want.sort_by(|a, b| a.0.total_cmp(&b.0));
                got.sort_by(|a, b| a.0.total_cmp(&b.0));
                assert_eq!(got.len(), want.len(), "seed {seed} pitch {}: {got:?} vs {want:?}", p + 21);
                for i in 0..got.len() {
                    assert!((got[i].0 - want[i].0).abs() <= tol, "seed {seed} pitch {} note {i}: onset {} vs {}", p + 21, got[i].0, want[i].0);
                    assert!((got[i].1 - want[i].1).abs() <= tol, "seed {seed} pitch {} note {i}: end {} vs {}", p + 21, got[i].1, want[i].1);
                    if i > 0 {
                        assert!(got[i].0 >= got[i - 1].1 - 1e-9, "seed {seed} pitch {}: notes {} and {} overlap", p + 21, i - 1, i);
                    }
                }
            }
        }
    }

    /// I62 on generated held notes: a run of dropped windows never splits or shortens the note.
    #[test]
    fn carry_invariant_held_note_survives_dropped_windows() {
        let tol = 1.5 / crate::sep::FPS;
        for seed in 1..=40u64 {
            let mut s = seed;
            let on = 0.6 + lcg_f(&mut s) * 1.0;
            let len = 8.0 + lcg_f(&mut s) * 4.0;
            let k0 = 8 + (lcg_f(&mut s) * 4.0) as usize;
            let d = 1 + (lcg_f(&mut s) * 7.0) as usize;
            let ks: Vec<usize> = (1..=30usize).filter(|k| !(k0..k0 + d).contains(k)).collect();
            let out = carry_out(&[(on, on + len, 48)], &ks);
            assert_eq!(out.len(), 1, "seed {seed}, dropped {k0}..{}: {out:?}", k0 + d);
            let m = &out[0];
            assert!((m.t - on).abs() <= tol, "seed {seed}: onset {} vs {on}", m.t);
            assert!((m.t + m.dur - (on + len)).abs() <= tol, "seed {seed}: end {} vs {}", m.t + m.dur, on + len);
        }
    }

    /// I60 with the Save path: mid-song take plus the final take equal one final save of the reference timeline,
    /// and a stopped session writes everything it has.
    #[test]
    fn save_invariant_mid_save_plus_final_is_the_single_save() {
        let step = CHUNK as f64 / RATE as f64;
        let win_len = BP_SAMPLES as f64 / BP_RATE as f64;
        for seed in 1..=40u64 {
            let chain = chain_notes(seed);
            let mut s = seed ^ 0x5555;
            let mid_k = 3 + (lcg_f(&mut s) * 23.0) as usize;
            // running: the session is live at the mid-song Save, then stops and saves again
            {
                let shared = Shared::new();
                let mut reference = crate::notes::Timeline::new();
                let mut open: Vec<Note> = Vec::new();
                let mut mid: Vec<Note> = Vec::new();
                for k in 1..=28usize {
                    let end_t = k as f64 * step;
                    let mut g = held_win(end_t - win_len, &chain);
                    let (new, ext) = window_notes(Some(&mut g), end_t, &mut open);
                    commit(&shared, 1, &new, &ext, &open);
                    reference.add(1, &new);
                    reference.extend(1, &ext);
                    if k == mid_k {
                        let (tracks, held) = crate::notes::take_for_save(&mut lock(&shared.timeline), true);
                        assert!(held >= open.len(), "seed {seed}: every open note is counted as held");
                        mid = tracks[1].clone();
                    }
                }
                release(&shared);
                let (fin, held_end) = crate::notes::take_for_save(&mut lock(&shared.timeline), true);
                assert_eq!(held_end, 0, "seed {seed}: nothing is held after release");
                let mut single: Vec<Note> = reference.drain_all().into_iter().nth(1).unwrap();
                let mut both: Vec<Note> = mid.iter().cloned().chain(fin[1].iter().cloned()).collect();
                let by_start = |a: &Note, b: &Note| a.t.total_cmp(&b.t).then(a.pitch.cmp(&b.pitch));
                single.sort_by(by_start);
                both.sort_by(by_start);
                assert_eq!(both, single, "seed {seed} mid save at window {mid_k}: mid + final differ from one final save");
            }
            // stopped at the mid-song Save: that Save writes every note so far, and nothing is held
            {
                let shared = Shared::new();
                let mut open: Vec<Note> = Vec::new();
                let mut added = 0usize;
                for k in 1..=mid_k {
                    let end_t = k as f64 * step;
                    let mut g = held_win(end_t - win_len, &chain);
                    let (new, ext) = window_notes(Some(&mut g), end_t, &mut open);
                    commit(&shared, 1, &new, &ext, &open);
                    added += new.len();
                }
                let (tracks, held) = crate::notes::take_for_save(&mut lock(&shared.timeline), false);
                assert_eq!(held, 0, "seed {seed}: a stopped session holds nothing");
                assert_eq!(tracks[1].len(), added, "seed {seed}: a stopped Save writes every note so far");
            }
        }
    }

    #[test]
    fn commit_marks_held_and_release_clears() {
        let shared = Shared::new();
        let a = Note { t: 1.0, dur: 0.5, pitch: 60, vel: 90 };
        commit(&shared, 1, &[a.clone()], &[], &[a.clone()]);
        let (tracks, held) = crate::notes::take_for_save(&mut lock(&shared.timeline), true);
        assert_eq!(held, 1, "the open note is held");
        assert!(tracks.iter().all(|v| v.is_empty()), "a held note is not written: {tracks:?}");
        release(&shared);
        let (tracks, held) = crate::notes::take_for_save(&mut lock(&shared.timeline), true);
        assert_eq!(held, 0);
        assert_eq!(tracks[1], vec![a], "after release the note is closed and written");
    }

    #[test]
    fn stream_split_sends_each_note_once_then_its_changes() {
        let a = Note { t: 1.0, dur: 0.5, pitch: 60, vel: 90 };
        let mut sent: [Vec<Note>; 3] = Default::default();
        let (n1, e1) = stream_split(&mut sent, [vec![], vec![], vec![]], [vec![], vec![a.clone()], vec![]]);
        assert_eq!(n1[1], vec![a.clone()], "a new held note is sent as a note");
        assert!(e1.iter().all(|v| v.is_empty()));
        let a2 = Note { dur: 1.0, ..a.clone() };
        let (n2, e2) = stream_split(&mut sent, [vec![], vec![], vec![]], [vec![], vec![a2.clone()], vec![]]);
        assert!(n2.iter().all(|v| v.is_empty()), "a lengthened held note is not sent again as a note: {n2:?}");
        assert_eq!(e2[1], vec![a2.clone()], "a lengthened held note goes out as ext");
        let (n3, e3) = stream_split(&mut sent, [vec![], vec![a2.clone()], vec![]], [vec![], vec![], vec![]]);
        assert!(n3.iter().all(|v| v.is_empty()) && e3.iter().all(|v| v.is_empty()), "an unchanged note is sent once: {n3:?} {e3:?}");
        let a3 = Note { dur: 1.4, ..a.clone() };
        let (_, _) = stream_split(&mut sent, [vec![], vec![], vec![]], [vec![], vec![a3.clone()], vec![]]);
        let (n4, e4) = stream_split(&mut sent, [vec![], vec![a3.clone()], vec![]], [vec![], vec![], vec![]]);
        assert!(n4.iter().all(|v| v.is_empty()), "{n4:?}");
        assert!(e4[1].is_empty(), "closed at the dur it was last sent with: {e4:?}");
    }

    #[test]
    fn chunk_line_has_no_ext_key_without_extensions() {
        let shared = Shared::new();
        let a = Note { t: 1.0, dur: 0.5, pitch: 60, vel: 90 };
        commit(&shared, 1, &[a.clone()], &[], &[a.clone()]);
        let mut sent: [Vec<Note>; 3] = Default::default();
        let line = chunk_line(&shared, &mut sent, 2);
        assert!(!line.contains("\"ext\""), "{line}");
        assert!(line.contains("\"notes\""), "{line}");
        assert_eq!(sent[1], vec![a], "the held note is recorded as sent");
    }

    #[test]
    fn chunk_line_carries_an_extension() {
        let shared = Shared::new();
        let a = Note { t: 1.0, dur: 0.5, pitch: 60, vel: 90 };
        commit(&shared, 1, &[a.clone()], &[], &[a.clone()]);
        let mut sent: [Vec<Note>; 3] = Default::default();
        let first = chunk_line(&shared, &mut sent, 2);
        assert!(first.contains("\"notes\""), "{first}");
        let a2 = Note { dur: 1.0, ..a.clone() };
        commit(&shared, 1, &[], &[(1.5, a2.clone())], &[a2.clone()]);
        let line = chunk_line(&shared, &mut sent, 3);
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        let t0 = v["t0"].as_f64().unwrap();
        let row: Vec<f64> = v["ext"]["bass"][0].as_array().expect("an ext row for bass").iter().map(|x| x.as_f64().unwrap()).collect();
        assert!((row[0] - (1.0 - t0)).abs() < 1e-9 && (row[1] - 1.0).abs() < 1e-9 && row[2] == 60.0 && row[3] == 90.0, "{row:?}");
    }

    #[test]
    fn stream_held_note_stops_at_a_restrike_already_sent() {
        let shared = Shared::new();
        let a = Note { t: 10.0, dur: 0.5, pitch: 40, vel: 90 };
        let s = Note { t: 10.125, dur: 0.3, pitch: 40, vel: 90 };
        let mut sent: [Vec<Note>; 3] = Default::default();
        let mut client: Vec<Note> = Vec::new();
        commit(&shared, 1, &[a.clone(), s.clone()], &[], &[a.clone()]);
        apply_chunk(&mut client, &chunk_line(&shared, &mut sent, 21));
        let a2 = Note { dur: 2.0, ..a.clone() };
        commit(&shared, 1, &[], &[(10.5, a2.clone())], &[a2.clone()]);
        apply_chunk(&mut client, &chunk_line(&shared, &mut sent, 22));
        let a3 = Note { dur: 2.5, ..a.clone() };
        commit(&shared, 1, &[], &[(12.0, a3.clone())], &[a3.clone()]);
        apply_chunk(&mut client, &chunk_line(&shared, &mut sent, 23));
        release(&shared);
        apply_chunk(&mut client, &chunk_line(&shared, &mut sent, 24));
        client.sort_by(|x, y| x.t.total_cmp(&y.t));
        assert_eq!(client.len(), 2, "{client:?}");
        assert!((client[0].dur - 0.125).abs() < 1e-6, "{client:?}");
        assert!((client[1].t - 10.125).abs() < 1e-6 && (client[1].dur - 0.3).abs() < 1e-6, "{client:?}");
    }

    /// I60 for the engine: a client that applies every chunk ends with the same whole notes as a final save.
    #[test]
    fn stream_invariant_client_ends_with_whole_notes() {
        let step = CHUNK as f64 / RATE as f64;
        let win_len = BP_SAMPLES as f64 / BP_RATE as f64;
        for seed in 1..=40u64 {
            let chain = chain_notes(seed);
            let shared = Shared::new();
            let mut reference = crate::notes::Timeline::new();
            let mut open: Vec<Note> = Vec::new();
            let mut sent: [Vec<Note>; 3] = Default::default();
            let mut client: Vec<Note> = Vec::new();
            for k in 1..=28usize {
                let end_t = k as f64 * step;
                let mut g = held_win(end_t - win_len, &chain);
                let (new, ext) = window_notes(Some(&mut g), end_t, &mut open);
                commit(&shared, 1, &new, &ext, &open);
                reference.add(1, &new);
                reference.extend(1, &ext);
                let line = chunk_line(&shared, &mut sent, k as u64);
                apply_chunk(&mut client, &line);
            }
            release(&shared);
            let line = chunk_line(&shared, &mut sent, 29);
            apply_chunk(&mut client, &line);
            let mut want: Vec<Note> = reference.drain_all().into_iter().nth(1).unwrap();
            let by_start = |a: &Note, b: &Note| a.t.total_cmp(&b.t).then(a.pitch.cmp(&b.pitch));
            want.sort_by(by_start);
            client.sort_by(by_start);
            assert_eq!(client.len(), want.len(), "seed {seed}: client {client:?} vs {want:?}");
            for (c, w) in client.iter().zip(want.iter()) {
                assert!(c.pitch == w.pitch && (c.t - w.t).abs() < 1e-6 && (c.dur - w.dur).abs() < 1e-6, "seed {seed}: client {c:?} vs {w:?}");
            }
        }
    }
}
