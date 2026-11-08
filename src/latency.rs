//! `wavelink-mixer latency`: measures the delay the mixer adds to game audio, by playing a click
//! straight into the hardware sink and again through the game channel, recording the hardware
//! sink's monitor each time. Pure helpers live here; the subcommand (`run`) touches PipeWire.
use crate::config::{names, BusKind, Config};
use crate::pw;
use serde_json::Value;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const RATE: usize = 48000;

/// Interleaved stereo click: `pre_s` seconds of silence, 1 ms at 0.9 on both channels, `post_s` of silence.
pub fn click_signal(rate: usize, pre_s: f64, post_s: f64) -> Vec<f32> {
    let pre = (pre_s * rate as f64).round() as usize;
    let post = (post_s * rate as f64).round() as usize;
    let click = rate / 1000;
    let mut v = vec![0.0f32; (pre + click + post) * 2];
    for f in pre..pre + click {
        v[f * 2] = 0.9;
        v[f * 2 + 1] = 0.9;
    }
    v
}

/// Index (in frames, not samples) of the first frame where |L| or |R| reaches `thresh`.
pub fn click_index(pcm: &[f32], thresh: f32) -> Option<usize> {
    pcm.chunks_exact(2).position(|f| f[0].abs() >= thresh || f[1].abs() >= thresh)
}

/// Device output latency in frames as PipeWire reports it: the `Latency` param of the node's
/// input port (`minQuantum * quantum + minRate`). `None` if the node or port is missing.
pub fn device_latency_frames(dump: &Value, node_name: &str, quantum: u64) -> Option<u64> {
    let objs = dump.as_array()?;
    let node = objs.iter().find(|o| o["type"] == "PipeWire:Interface:Node" && o["info"]["props"]["node.name"] == node_name)?;
    let node_id = node["id"].as_u64()?;
    objs.iter()
        .filter(|o| {
            o["type"] == "PipeWire:Interface:Port"
                && o["info"]["props"]["node.id"].as_u64() == Some(node_id)
                && o["info"]["props"]["port.direction"] == "in"
        })
        .find_map(|p| {
            let lat = p["info"]["params"]["Latency"].as_array()?;
            let e = lat.iter().find(|e| e["direction"] == "Input")?;
            let q = e["minQuantum"].as_f64()?;
            let r = e["minRate"].as_f64()?;
            Some((q * quantum as f64 + r).round() as u64)
        })
}

pub fn median(xs: &[f64]) -> f64 {
    let mut v = xs.to_vec();
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n == 0 {
        f64::NAN
    } else if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// Medians, mins and the delay split for one latency run. `added_ms` is clamped at zero: a
/// via-mixer median below the direct one is measurement noise, not a negative delay.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub direct_median_ms: f64,
    pub direct_min_ms: f64,
    pub via_median_ms: f64,
    pub via_min_ms: f64,
    pub device_ms: f64,
    pub added_ms: f64,
    pub total_ms: f64,
    pub below_noise: bool,
}

pub fn summarize(direct_ms: &[f64], via_ms: &[f64], device_ms: f64) -> Summary {
    let direct_median_ms = median(direct_ms);
    let via_median_ms = median(via_ms);
    let added_ms = (via_median_ms - direct_median_ms).max(0.0);
    Summary {
        direct_median_ms,
        direct_min_ms: direct_ms.iter().copied().fold(f64::INFINITY, f64::min),
        via_median_ms,
        via_min_ms: via_ms.iter().copied().fold(f64::INFINITY, f64::min),
        device_ms,
        added_ms,
        total_ms: device_ms + added_ms,
        below_noise: via_median_ms < direct_median_ms,
    }
}

/// Human summary of `summarize`: medians, the added delay (via − direct) and the total game → ears estimate.
pub fn summary(direct_ms: &[f64], via_ms: &[f64], device_ms: f64) -> String {
    let s = summarize(direct_ms, via_ms, device_ms);
    let added = if s.below_noise && format!("{:.1}", s.via_median_ms) != format!("{:.1}", s.direct_median_ms) {
        format!(
            "0.0 ms (below measurement noise: via mixer median {:.1} ms < direct median {:.1} ms)",
            s.via_median_ms, s.direct_median_ms
        )
    } else {
        format!("{:.1} ms", s.added_ms)
    };
    format!(
        "direct: median {:.1} ms (min {:.1})\n\
         via mixer: median {:.1} ms (min {:.1})\n\
         added by mixer: {added}\n\
         device (PipeWire-reported): {:.1} ms\n\
         total game → ears ≈ {:.1} ms",
        s.direct_median_ms,
        s.direct_min_ms,
        s.via_median_ms,
        s.via_min_ms,
        s.device_ms,
        s.total_ms
    )
}

/// `latency [--runs N] [--channel game]`. Returns the process exit code.
pub fn run(args: Vec<String>) -> i32 {
    let mut runs = 5usize;
    let mut channel = "game".to_string();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--runs" => match it.next().and_then(|v| v.parse().ok()) {
                Some(n) if n > 0 => runs = n,
                _ => {
                    eprintln!("--runs needs a number above 0");
                    return 2;
                }
            },
            "--channel" => match it.next() {
                Some(c) => channel = c.clone(),
                None => {
                    eprintln!("--channel needs a name");
                    return 2;
                }
            },
            other => {
                eprintln!("unknown argument '{other}'");
                return 2;
            }
        }
    }
    let cfg = Config::load();
    let Some(hw) = cfg.buses.iter().find_map(|b| match &b.kind {
        BusKind::Hardware { target } => Some(target.clone()),
        _ => None,
    }) else {
        eprintln!("no hardware output bus in the config");
        return 1;
    };
    let game = names::ch(&channel);
    let meta = text("pw-metadata", &["-n", "settings"]);
    let quantum = pw::meta_value(&meta, "clock.force-quantum")
        .filter(|q| *q > 0)
        .or(pw::meta_value(&meta, "clock.quantum"))
        .unwrap_or(1024);
    let dump: Value = pw::output_timeout("pw-dump", &[], Duration::from_secs(3))
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    let Some(dev) = device_latency_frames(&dump, &hw, quantum) else {
        eprintln!("device {hw} has no PipeWire latency report (is it in the graph?)");
        return 1;
    };
    let device_ms = dev as f64 * 1000.0 / RATE as f64;
    println!(
        "graph quantum {quantum} frames ({:.1} ms per cycle), channel {channel}, device {hw}",
        quantum as f64 * 1000.0 / RATE as f64
    );

    let (mut direct, mut via) = (Vec::new(), Vec::new());
    for i in 1..=runs {
        for (tag, target, out) in [("direct", hw.as_str(), &mut direct), ("via mixer", game.as_str(), &mut via)] {
            match measure(&hw, target, quantum) {
                Some(ms) => {
                    println!("{tag} run {i}: {ms:.1} ms");
                    out.push(ms);
                }
                None => println!("{tag} run {i}: no click heard"),
            }
        }
    }
    let need = runs.min(3);
    if direct.len() < need || via.len() < need {
        println!("heard {} direct and {} via-mixer clicks; need {need} of each", direct.len(), via.len());
        return 1;
    }
    println!("{}", summary(&direct, &via, device_ms));
    0
}

/// One click: record `hw`'s monitor while a click plays into `play`. Returns the delay in ms
/// from the click's onset in the signal to its arrival at the monitor.
fn measure(hw: &str, play: &str, quantum: u64) -> Option<f64> {
    let lat = format!("{quantum}/48000");
    let mut rec = Command::new("pw-record")
        .args([
            "--target", hw,
            "-P", "{ node.name=lat_rec node.passive=true media.class=Stream/Input/Audio stream.capture.sink=true node.dont-fallback=true }",
            "--format", "f32", "--rate", "48000", "--channels", "2", "--latency", lat.as_str(), "--raw", "-",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let buf: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
    let reader = rec.stdout.take().map(|mut out| {
        let buf = buf.clone();
        std::thread::spawn(move || {
            let mut pending: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                match out.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        pending.extend_from_slice(&chunk[..n]);
                        let whole = pending.len() / 4 * 4;
                        let samples: Vec<f32> = pending[..whole]
                            .chunks_exact(4)
                            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                            .collect();
                        pending.drain(..whole);
                        buf.lock().unwrap().extend(samples);
                    }
                }
            }
        })
    });

    let result = (|| {
        if !wait_frames(&buf, 24_000, Duration::from_secs(5)) {
            return None;
        }
        let n0 = buf.lock().unwrap().len() / 2;
        let sig: Vec<u8> = click_signal(RATE, 0.2, 0.3).iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut player = Command::new("pw-play")
            .args([
                "--target", play,
                "-P", "{ node.name=lat_play node.dont-fallback=true }",
                "--format", "f32", "--rate", "48000", "--channels", "2", "--latency", lat.as_str(), "--raw", "-",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        if let Some(mut stdin) = player.stdin.take() {
            let _ = stdin.write_all(&sig);
        }
        let _ = player.wait();
        let t = Instant::now();
        let idx = loop {
            let found = click_index(&buf.lock().unwrap()[n0 * 2..], 0.3);
            if found.is_some() || t.elapsed() > Duration::from_secs(2) {
                break found;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        idx.map(|i| (i as f64 - 0.2 * RATE as f64) * 1000.0 / RATE as f64)
    })();

    let _ = rec.kill();
    let _ = rec.wait();
    if let Some(r) = reader {
        let _ = r.join();
    }
    result
}

fn wait_frames(buf: &Mutex<Vec<f32>>, frames: usize, limit: Duration) -> bool {
    let t = Instant::now();
    while buf.lock().unwrap().len() < frames * 2 {
        if t.elapsed() > limit {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    true
}

fn text(cmd: &str, args: &[&str]) -> String {
    pw::output_timeout(cmd, args, Duration::from_secs(3)).map(|b| String::from_utf8_lossy(&b).to_string()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn click_signal_layout() {
        let s = click_signal(48000, 0.2, 0.3);
        assert_eq!(s.len(), 24048 * 2);
        assert!(s[..9600 * 2].iter().all(|&x| x == 0.0));
        for f in 9600..9648 {
            assert_eq!(s[f * 2], 0.9);
            assert_eq!(s[f * 2 + 1], 0.9);
        }
        assert!(s[9648 * 2..].iter().all(|&x| x == 0.0));
    }

    #[test]
    fn click_index_finds_first_frame() {
        let s = click_signal(48000, 0.2, 0.3);
        assert_eq!(click_index(&s, 0.3), Some(9600));
        assert_eq!(click_index(&vec![0.0f32; 1000], 0.3), None);
        assert_eq!(click_index(&s, 0.95), None);
    }

    #[test]
    fn device_latency_from_dump() {
        let dump = json!([
            {"id": 7, "type": "PipeWire:Interface:Node", "info": {"props": {"node.name": "hw"}}},
            {"id": 8, "type": "PipeWire:Interface:Port", "info": {
                "props": {"node.id": 7, "port.direction": "in"},
                "params": {"Latency": [
                    {"direction": "Input", "minQuantum": 1.0, "minRate": 512},
                    {"direction": "Output", "minQuantum": 0.0, "minRate": 0}]}}},
            {"id": 9, "type": "PipeWire:Interface:Port", "info": {
                "props": {"node.id": 7, "port.direction": "out"},
                "params": {"Latency": [
                    {"direction": "Input", "minQuantum": 0.0, "minRate": 0},
                    {"direction": "Output", "minQuantum": 0.0, "minRate": 0}]}}}
        ]);
        assert_eq!(device_latency_frames(&dump, "hw", 1024), Some(1536));
        assert_eq!(device_latency_frames(&dump, "nope", 1024), None);
    }

    #[test]
    fn median_odd_even() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[1.0, 2.0, 3.0, 4.0]), 2.5);
    }

    #[test]
    fn summary_reports_difference() {
        let s = summary(&[10.0, 11.0, 12.0], &[40.0, 41.0, 42.0], 8.0);
        assert!(s.contains("added by mixer: 30.0 ms"), "{s}");
        assert!(s.contains("total game → ears ≈ 38.0 ms"), "{s}");
    }

    #[test]
    fn summarize_clamps_negative_added_to_zero() {
        let direct = [37.3, 37.3, 37.3];
        let via = [33.5, 33.6, 33.7];
        let s = summarize(&direct, &via, 10.7);
        assert_eq!(s.added_ms, 0.0, "{s:?}");
        assert!((s.total_ms - 10.7).abs() < 0.01, "{s:?}");
        assert!(s.below_noise, "{s:?}");
        let printed = summary(&direct, &via, 10.7);
        assert!(
            printed.contains("added by mixer: 0.0 ms (below measurement noise: via mixer median 33.6 ms < direct median 37.3 ms)"),
            "{printed}"
        );
    }

    #[test]
    fn summarize_reports_real_delay() {
        let s = summarize(&[42.7; 5], &[44.2; 5], 32.0);
        assert!((s.added_ms - 1.5).abs() < 0.01, "{s:?}");
        assert!((s.total_ms - 33.5).abs() < 0.01, "{s:?}");
        assert!(!s.below_noise, "{s:?}");
    }

    #[test]
    fn summary_noise_line_hidden_when_rounded_medians_equal() {
        let p = summary(&[37.34; 3], &[37.31; 3], 10.0);
        assert!(p.contains("added by mixer: 0.0 ms\n"), "{p}");
        assert!(!p.contains("below measurement noise"), "{p}");
        assert!(p.contains("total game → ears ≈ 10.0 ms"), "{p}");
    }

    #[test]
    fn run_rejects_zero_runs() {
        assert_eq!(run(vec!["--runs".into(), "0".into()]), 2);
    }
}
