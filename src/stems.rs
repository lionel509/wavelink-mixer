//! Stems session: taps a channel, separates it, plays and/or shows the stems, and writes notes to the MIDI channel.
//! Unit 1 skeleton: bodies are `todo!()` until Unit 4.
#![allow(dead_code, unused_imports, unused_variables)]
use crate::config::Config;
use crate::dash::{Fft, BANDS};
use crate::notes::{Note, Synth, Timeline, MIDI_LAG, STEMS};
use crate::sep::{Separator, Streamer, Transcriber, BLOCK, BLOCK_MS, PARALLEL};
use crate::stemcfg::{Plays, SepEngine};
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;
use std::sync::{mpsc, Arc, Mutex};

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

/// Returns (ladder index, hop+pad+1.5 t + 0.03) for the cheapest tier this machine keeps up with.
pub fn pick_tier(ms_per_s: f64) -> (usize, f64) {
    todo!()
}

/// Lindley-style cushion for a live buffer, from per-call wall times and the hop; clamped to 10..=200 ms.
pub fn cushion_ms(call_ms: &[f64], hop_ms: f64) -> u32 {
    todo!()
}

/// `pw-cat --record --target <target> -P "{ node.name=<name> node.passive=true media.class=Stream/Input/Audio [stream.capture.sink=true] node.dont-fallback=true }" ...`
pub fn tap_args(target: &str, capture_sink: bool, name: &str) -> Vec<String> {
    todo!()
}

/// `pw-cat --playback --target <node> ...`
pub fn play_args(node: &str) -> Vec<String> {
    todo!()
}

pub struct Chunker {}

impl Chunker {
    /// Windows are [id*hop - left, id*hop + hop + pad) in frames, zeros before 0.
    pub fn new(left: usize, hop: usize, pad: usize) -> Self {
        todo!()
    }

    /// Interleaved stereo.
    pub fn push(&mut self, pcm: &[f32]) {
        todo!()
    }

    pub fn next_window(&mut self) -> Option<Window> {
        todo!()
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

pub struct Scheduler {}

impl Scheduler {
    pub fn new() -> Self {
        todo!()
    }

    pub fn put(&mut self, r: HopResult) {
        todo!()
    }

    pub fn take(&mut self, id: u64) -> Slot {
        todo!()
    }

    pub fn pending(&self) -> usize {
        todo!()
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
    todo!()
}

/// Shared with the UI: bands are 3 x BANDS; played is in frames at 44.1 kHz.
#[derive(Clone)]
pub struct Shared {
    pub bands: Vec<Arc<Mutex<Vec<f32>>>>,
    pub timeline: Arc<Mutex<Timeline>>,
    pub played: Arc<AtomicU64>,
    pub status: Arc<Mutex<String>>,
}

impl Shared {
    pub fn new() -> Shared {
        todo!()
    }

    /// played / 44100.
    pub fn now(&self) -> f64 {
        todo!()
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

pub struct Session {}

impl Session {
    pub fn start(o: Opts, shared: Shared, chunks: Option<mpsc::Sender<(u64, [Vec<f32>; 3])>>) -> Session {
        todo!()
    }
}

impl Drop for Session {
    /// Stop flag; kill and wait every child; never blocks the GTK thread on a join.
    fn drop(&mut self) {
        todo!()
    }
}

/// Two passive taps of `secs` each, in memory; returns (heard delay ms, r).
pub fn measure_delay(a: (&str, bool), b: (&str, bool), secs: f64) -> Result<(f64, f32), String> {
    todo!()
}

/// `stems [--source ID] [--engine live|quality] [--buf-ms N] [--tier K] [--measure-delay]`
pub fn cli(args: Vec<String>) -> i32 {
    todo!()
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
}
