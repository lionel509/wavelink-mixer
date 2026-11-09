//! Throwaway bench for issue #20: separation speed per model, called the way the app calls it.
//! `cargo test --release bench20 -- --ignored --nocapture`; `BENCH_N` overrides the sample count (default 100).
//! Not a PR. Run on the PC by bench/run-pc.sh (CPU, then `--features cuda`).
use crate::models::{self, S_FILES, U_FILES};
use crate::sep::{deinterleave, Separator, Streamer, Transcriber, BLOCK, PARALLEL};
use crate::stems::{pick_tier, BP_WINDOW, CHUNK, CHUNK_BLOCKS, LADDER};
use std::time::Instant;

const RATE: usize = 44100;
/// 60 s of stereo, in frames.
const TOTAL: usize = 60 * RATE;
const WARM: usize = 5;

/// Deterministic uniform [-1, 1) from an LCG (no `rand` crate).
fn lcg(s: &mut u64) -> f32 {
    *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    ((*s >> 33) as f32) / (1u64 << 31) as f32 * 2.0 - 1.0
}

/// Frames for `secs`, as stems::frames does.
fn frames(secs: f64) -> usize {
    (secs * RATE as f64).round() as usize
}

/// Interleaved stereo: sines, a click every 0.5 s and LCG noise.
fn signal() -> Vec<f32> {
    use std::f64::consts::PI;
    let mut seed = 20u64;
    let mut x = Vec::with_capacity(2 * TOTAL);
    for i in 0..TOTAL {
        let t = i as f64 / RATE as f64;
        let click = if i % 22050 < 44 { 0.8f32 } else { 0.0 };
        let left = (2.0 * PI * 110.0 * t).sin() * 0.3 + (2.0 * PI * 440.0 * t).sin() * 0.15;
        let right = (2.0 * PI * 165.0 * t).sin() * 0.3 + (2.0 * PI * 660.0 * t).sin() * 0.15;
        for tone in [left, right] {
            let noise = lcg(&mut seed) * 0.1;
            x.push((tone as f32 + click + noise).clamp(-1.0, 1.0));
        }
    }
    x
}

fn bench_n() -> usize {
    std::env::var("BENCH_N").ok().and_then(|v| v.parse().ok()).unwrap_or(100)
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

/// Prints the one BENCH line for a model. `ms` holds one entry per timed sample, already in ms per 0.5 s of audio.
fn report(ep: &str, model: &str, mut ms: Vec<f64>) {
    ms.sort_by(|a, b| a.total_cmp(b));
    let p50 = pct(&ms, 0.5);
    println!(
        "BENCH ep={ep} model={model} n={} p50_ms={:.3} p99_ms={:.3} max_ms={:.3} rtf={:.4}",
        ms.len(),
        p50,
        pct(&ms, 0.99),
        ms[ms.len() - 1],
        p50 / 500.0
    );
}

fn providers_line() -> String {
    #[cfg(feature = "cuda")]
    {
        use ort::execution_providers::{CUDAExecutionProvider, ExecutionProvider};
        let avail = CUDAExecutionProvider::default().is_available().unwrap_or(false);
        format!("BENCH ep=cuda providers=CUDAExecutionProvider(device=0,error_on_failure) cuda_available={avail}")
    }
    #[cfg(not(feature = "cuda"))]
    {
        "BENCH ep=cpu providers=CPUExecutionProvider".to_string()
    }
}

/// StemgenRT as Live runs it: one process() per 128-frame block, a chunk is CHUNK_BLOCKS blocks, summed.
fn bench_live(ep: &str, sig: &[f32], n: usize) {
    let m = models::ensure(&S_FILES, &|s: String| println!("{s}")).unwrap();
    let mut st = Streamer::load(&m, PARALLEL).unwrap();
    let nblocks = TOTAL / BLOCK;
    let blocks: Vec<Vec<f32>> = (0..nblocks).map(|b| deinterleave(&sig[2 * b * BLOCK..2 * (b + 1) * BLOCK])).collect();
    let mut chunk_ms = Vec::with_capacity(n);
    for c in 0..WARM + n {
        let mut total = 0f64;
        for k in 0..CHUNK_BLOCKS {
            let b = (c * CHUNK_BLOCKS + k) % nblocks;
            let t0 = Instant::now();
            st.process(&blocks[b]).expect("StemgenRT process");
            total += t0.elapsed().as_secs_f64() * 1000.0;
        }
        if c >= WARM {
            chunk_ms.push(total);
        }
    }
    report(ep, "live", chunk_ms);
}

/// UMX-HQ as Quality runs it: pick the tier from ms_per_s() like separate_loop does, then one separate() per window.
/// Each call is scaled to ms per 0.5 s of new audio (the hop is the new audio per call).
fn bench_quality(ep: &str, sig: &[f32], n: usize) {
    let m = models::ensure(&U_FILES, &|s: String| println!("{s}")).unwrap();
    let mut sep = Separator::load(&m).unwrap();
    let ms = sep.ms_per_s().unwrap();
    let (idx, delay) = pick_tier(ms);
    let (l, h, p) = LADDER[idx];
    let (win, hop) = (frames(l + h + p), frames(h));
    println!("tier ep={ep} model=quality idx={idx} l={l} h={h} p={p} ms_per_s={ms:.2} delay_s={delay:.3}");
    let mut calls = Vec::with_capacity(n);
    for k in 0..WARM + n {
        let start = (k * hop) % (TOTAL - win);
        let w = &sig[2 * start..2 * (start + win)];
        let t0 = Instant::now();
        sep.separate(w).expect("UMX separate");
        let call_ms = t0.elapsed().as_secs_f64() * 1000.0;
        if k >= WARM {
            calls.push(call_ms * 0.5 / h);
        }
    }
    report(ep, "quality", calls);
}

/// Basic Pitch as the transcriber runs it on one chunk: notes() on the two stems it transcribes (bass, other),
/// each on a BP_WINDOW mono window. The drum onset DSP is not the transcriber and is not timed.
fn bench_basicpitch(ep: &str, sig: &[f32], n: usize) {
    let m = models::ensure(&["basic-pitch-nmp.onnx"], &|s: String| println!("{s}")).unwrap();
    let mut tr = Transcriber::load(&m).unwrap();
    let span = TOTAL - BP_WINDOW;
    let mono = |start: usize| -> Vec<f32> {
        sig[2 * start..2 * (start + BP_WINDOW)].chunks_exact(2).map(|c| 0.5 * (c[0] + c[1])).collect()
    };
    let mut chunk_ms = Vec::with_capacity(n);
    for k in 0..WARM + n {
        let a = (k * CHUNK) % span;
        let b = (k * CHUNK + CHUNK / 2) % span;
        let (xa, xb) = (mono(a), mono(b));
        let mut total = 0f64;
        for x in [&xa, &xb] {
            let t0 = Instant::now();
            tr.notes(x).expect("Basic Pitch notes");
            total += t0.elapsed().as_secs_f64() * 1000.0;
        }
        if k >= WARM {
            chunk_ms.push(total);
        }
    }
    report(ep, "basicpitch", chunk_ms);
}

#[test]
#[ignore]
fn bench20() {
    let ep = if cfg!(feature = "cuda") { "cuda" } else { "cpu" };
    println!("{}", providers_line());
    println!("ort info: {}", ort::info());
    let n = bench_n();
    println!("samples per model: {n} (after {WARM} warm-up)");
    let sig = signal();
    bench_live(ep, &sig, n);
    bench_quality(ep, &sig, n);
    bench_basicpitch(ep, &sig, n);
}
