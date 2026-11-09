//! `wavelink-mixer piano --eval song.wav [--sections a-b,c-d,...]`: runs the LIVE note path (the
//! default core in 10 ms chunks of the mono mix, then the pianist layer: what the port plays) over a
//! recording and scores it without a reference:
//! - high-note rate: notes per second above C6;
//! - chroma agreement: per 100 ms, cosine between the notes' pitch-class profile and the audio's
//!   (FFT magnitudes 60 Hz-4 kHz folded to 12 classes), averaged over frames with sound;
//! - resynthesis distance: the notes rendered with `piano::synth_piano`, log-mel (40 bands) of both,
//!   each frame mean-removed (spectral shape, not level), mean L2 distance;
//! - note count and mean length; all per section.
use crate::notes::Note;
use crate::piano::{self, RATE};
use crate::subtract::Fft;

/// Per 100 ms: the audio chroma, and its prominent peaks (Hz, energy).
fn chroma_audio(x: &[f32]) -> (Vec<[f32; 12]>, Vec<Vec<(f32, f32)>>) {
    let n = 4096;
    let hop = (RATE as usize) / 10;
    let fft = Fft::new(n);
    let win: Vec<f32> = (0..n).map(|i| (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / n as f64).cos()) as f32).collect();
    let df = RATE as f32 / n as f32;
    let pc: Vec<Option<usize>> = (0..n / 2)
        .map(|k| {
            let f = k as f32 * df;
            if !(60.0..4000.0).contains(&f) {
                None
            } else {
                Some(((69.0 + 12.0 * (f / 440.0).log2()).round() as i32).rem_euclid(12) as usize)
            }
        })
        .collect();
    let (mut re, mut im) = (vec![0f32; n], vec![0f32; n]);
    let mut out = Vec::new();
    let mut peaks = Vec::new();
    let mut mag = vec![0f32; n / 2];
    let mut c = 0usize;
    while c < x.len() {
        for i in 0..n {
            let s = c as isize + i as isize - (n / 2) as isize;
            re[i] = if s >= 0 && (s as usize) < x.len() { x[s as usize] * win[i] } else { 0.0 };
            im[i] = 0.0;
        }
        fft.run(&mut re, &mut im, false);
        let mut ch = [0f32; 12];
        for k in 1..n / 2 {
            mag[k] = (re[k] * re[k] + im[k] * im[k]).sqrt();
            if let Some(p) = pc[k] {
                ch[p] += mag[k].sqrt();
            }
        }
        out.push(ch);
        // prominent peaks: local maxima >= 6 dB over the local median (+-20 bins), within 40 dB of
        // the frame max, 55 Hz - 4.5 kHz
        // from 55 Hz: below that the peaks are mostly kick drum and sub rumble (lofi: 100 % unexplained)
        let (k0, k1) = ((55.0 / df) as usize + 1, (4500.0 / df) as usize);
        let fmax = mag[k0..k1].iter().fold(0f32, |m, &v| m.max(v));
        let mut pk = Vec::new();
        let mut loc: Vec<f32> = Vec::with_capacity(41);
        for k in k0.max(2)..k1 {
            let m = mag[k];
            if m < fmax * 0.01 || m < mag[k - 1] || m < mag[k + 1] || m < mag[k - 2] || m < mag[k + 2] {
                continue;
            }
            loc.clear();
            loc.extend_from_slice(&mag[k.saturating_sub(20)..(k + 21).min(n / 2)]);
            loc.sort_by(|a, b| a.total_cmp(b));
            if m >= 2.0 * loc[loc.len() / 2] {
                pk.push((k as f32 * df, m * m));
            }
        }
        peaks.push(pk);
        c += hop;
    }
    (out, peaks)
}

/// Share of the peak energy within +-35 cents of a partial (1..16) of a note sounding then.
fn coverage(peaks: &[(f32, f32)], sounding: &[u8]) -> Option<(f32, f32)> {
    let tot: f32 = peaks.iter().map(|p| p.1).sum();
    if tot <= 0.0 {
        return None;
    }
    let ex: f32 = peaks
        .iter()
        .filter(|(f, _)| {
            sounding.iter().any(|&p| {
                let f0 = 440.0 * 2f32.powf((p as f32 - 69.0) / 12.0);
                let h = (f / f0).round();
                (1.0..=16.0).contains(&h) && (1200.0 * (f / (h * f0)).log2()).abs() <= 35.0
            })
        })
        .map(|p| p.1)
        .sum();
    Some((ex, tot))
}

fn chroma_notes(notes: &[Note], frames: usize) -> Vec<[f32; 12]> {
    let mut out = vec![[0f32; 12]; frames];
    for n in notes {
        let (a, b) = ((n.t * 10.0).round().max(0.0) as usize, ((n.t + n.dur) * 10.0).round() as usize);
        for f in a..b.max(a + 1).min(frames) {
            out[f][(n.pitch % 12) as usize] += n.vel as f32 / 127.0;
        }
    }
    out
}

fn cos(a: &[f32; 12], b: &[f32; 12]) -> Option<f32> {
    let (na, nb) = (a.iter().map(|v| v * v).sum::<f32>().sqrt(), b.iter().map(|v| v * v).sum::<f32>().sqrt());
    if na < 1e-9 || nb < 1e-9 {
        return None;
    }
    Some(a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>() / (na * nb))
}

fn logmel(x: &[f32]) -> Vec<[f32; 40]> {
    let n = 2048;
    let hop = 512;
    let fft = Fft::new(n);
    let win: Vec<f32> = (0..n).map(|i| (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / n as f64).cos()) as f32).collect();
    let mel = |f: f32| 2595.0 * (1.0 + f / 700.0).log10();
    let (m0, m1) = (mel(40.0), mel(8000.0));
    let df = RATE as f32 / n as f32;
    let band: Vec<Option<usize>> = (0..n / 2)
        .map(|k| {
            let m = mel(k as f32 * df);
            if m < m0 || m >= m1 {
                None
            } else {
                Some((((m - m0) / (m1 - m0)) * 40.0) as usize)
            }
        })
        .collect();
    let (mut re, mut im) = (vec![0f32; n], vec![0f32; n]);
    let mut out = Vec::new();
    let mut c = 0usize;
    while c + n <= x.len() {
        for i in 0..n {
            re[i] = x[c + i] * win[i];
            im[i] = 0.0;
        }
        fft.run(&mut re, &mut im, false);
        let mut m = [0f32; 40];
        for k in 1..n / 2 {
            if let Some(b) = band[k] {
                m[b] += re[k] * re[k] + im[k] * im[k];
            }
        }
        let mut l = [0f32; 40];
        for b in 0..40 {
            l[b] = 10.0 * (m[b] + 1e-6).log10();
        }
        let mean = l.iter().sum::<f32>() / 40.0;
        l.iter_mut().for_each(|v| *v -= mean);
        out.push(l);
        c += hop;
    }
    out
}

/// The live path over `mono` (at RATE): the notes the port would play.
pub fn live_notes(mono: &[f32]) -> (Vec<Note>, f64) {
    let mut st = crate::subtract::SubTracker::new();
    let mut pn = crate::pianist::Pianist::new();
    let mut evs = Vec::new();
    let t = std::time::Instant::now();
    let bypass = std::env::var_os("PIANO_EVAL_RAW").is_some();
    for c in mono.chunks(220) {
        let raw = st.push(c);
        if bypass {
            evs.extend(raw);
        } else {
            pn.push(&raw, st.now(), &mut evs);
        }
    }
    let raw = st.flush();
    let end = st.now();
    pn.push(&raw, end, &mut evs);
    pn.finish(end, &mut evs);
    let ms = t.elapsed().as_secs_f64() * 1000.0 / (mono.len() as f64 / RATE as f64 / 0.5);
    (piano::events_to_notes(&piano::apply_pedal(&evs)), ms)
}

pub fn eval(mono: &[f32], sections: &[(String, f64, f64)]) -> String {
    let (notes, ms) = live_notes(mono);
    if std::env::var_os("PIANO_EVAL_DEBUG").is_some() {
        for n in &notes {
            if let Some(m) = notes.iter().filter(|m| m.pitch == n.pitch && m.t < n.t - 1e-6 && m.t + m.dur >= n.t - 0.3).last() {
                eprintln!("restrike p{} at {:.2}: prev {:.2}+{:.2} gap {:.3}", n.pitch, n.t, m.t, m.dur, n.t - m.t - m.dur);
            }
        }
    }
    if let Ok(p) = std::env::var("PIANO_EVAL_MID") {
        let _ = std::fs::write(p, crate::notes::piano_midi(&notes, false, Some(0.0)));
    }
    let secs = mono.len() as f64 / RATE as f64;
    let (ca, peaks) = chroma_audio(mono);
    let mut sounding: Vec<Vec<u8>> = vec![Vec::new(); ca.len()];
    for n in &notes {
        let (a, b) = ((n.t * 10.0).round().max(0.0) as usize, ((n.t + n.dur) * 10.0).round() as usize);
        for f in a..b.max(a + 1).min(ca.len()) {
            sounding[f].push(n.pitch);
        }
    }
    let cn = chroma_notes(&notes, ca.len());
    let synth = piano::synth_piano(&notes, secs, 7);
    let (ma, ms_) = (logmel(mono), logmel(&synth));
    if std::env::var_os("PIANO_EVAL_PEAKS").is_some() {
        // unexplained peak energy by octave band, and its strongest pitch classes
        let mut band = [0f32; 9];
        let mut tot = [0f32; 9];
        let mut pcs = [0f32; 12];
        for (f, pk) in peaks.iter().enumerate() {
            for &(hz, e) in pk {
                let b = ((hz / 27.5).log2().floor() as usize).min(8);
                tot[b] += e;
                let ok = sounding[f].iter().any(|&p| {
                    let f0 = 440.0 * 2f32.powf((p as f32 - 69.0) / 12.0);
                    let h = (hz / f0).round();
                    (1.0..=16.0).contains(&h) && (1200.0 * (hz / (h * f0)).log2()).abs() <= 35.0
                });
                if !ok {
                    band[b] += e;
                    pcs[((69.0 + 12.0 * (hz / 440.0).log2()).round() as i32).rem_euclid(12) as usize] += e;
                }
            }
        }
        for b in 0..9 {
            eprintln!("A{}..: unexplained {:.0}% of {:.3e}", b, 100.0 * band[b] / tot[b].max(1e-12), tot[b]);
        }
        // the strongest unexplained low peaks (55-110 Hz), by frequency
        let mut low: std::collections::BTreeMap<i32, f32> = Default::default();
        for (f, pk) in peaks.iter().enumerate() {
            for &(hz, e) in pk {
                if (55.0..110.0).contains(&hz) {
                    let ok = sounding[f].iter().any(|&p| {
                        let f0 = 440.0 * 2f32.powf((p as f32 - 69.0) / 12.0);
                        let h = (hz / f0).round();
                        (1.0..=16.0).contains(&h) && (1200.0 * (hz / (h * f0)).log2()).abs() <= 35.0
                    });
                    if !ok {
                        *low.entry((69.0 + 12.0 * (hz / 440.0).log2()).round() as i32).or_default() += e;
                    }
                }
            }
        }
        eprintln!("unexplained 55-110 Hz by midi: {:?}", low.iter().map(|(k, v)| (*k, (*v / 1e3).round())).collect::<Vec<_>>());
        let mut bass: std::collections::BTreeMap<u8, usize> = Default::default();
        for n in &notes {
            if n.pitch < 52 {
                *bass.entry(n.pitch).or_default() += 1;
            }
        }
        eprintln!("bass notes played: {bass:?}");
        eprintln!("unexplained by pitch class: {:?}", pcs.iter().map(|v| (v / pcs.iter().sum::<f32>().max(1e-12) * 100.0).round()).collect::<Vec<_>>());
    }
    let mut gt_line = String::new();
    if let Ok(r) = std::env::var("PIANO_EVAL_REF") {
        if let Ok(gt) = std::fs::read(&r).map_err(|e| e.to_string()).and_then(|b| crate::notes::parse_smf(&b)) {
            let gt: Vec<Note> = gt.into_iter().filter(|p| p.ch != 9).map(|p| p.note).collect();
            let (p1, r1, f1) = piano::note_f1(&notes, &gt, 0.05);
            let (p2, r2, f2) = piano::note_f1(&notes, &gt, 0.10);
            // frame level, 50 ms: active pitch sets
            let nf = (secs * 20.0) as usize;
            let act = |ns: &[Note]| {
                let mut a = vec![[false; 128]; nf];
                for n in ns {
                    for f in ((n.t * 20.0) as usize)..(((n.t + n.dur) * 20.0) as usize).min(nf) {
                        a[f][n.pitch as usize] = true;
                    }
                }
                a
            };
            let (ea, ga) = (act(&notes), act(&gt));
            let (mut tp, mut fp, mut fneg) = (0f64, 0f64, 0f64);
            for f in 0..nf {
                for p in 0..128 {
                    match (ea[f][p], ga[f][p]) {
                        (true, true) => tp += 1.0,
                        (true, false) => fp += 1.0,
                        (false, true) => fneg += 1.0,
                        _ => {}
                    }
                }
            }
            let ff1 = 2.0 * tp / (2.0 * tp + fp + fneg).max(1.0);
            if std::env::var_os("PIANO_EVAL_FP").is_some() {
                // unmatched estimates: interval to the nearest reference note sounding at their onset
                let mut hist: std::collections::BTreeMap<i32, usize> = Default::default();
                let mut late = Vec::new();
                for n in &notes {
                    if gt.iter().any(|g| g.pitch == n.pitch && (g.t - n.t).abs() <= 0.05) {
                        continue;
                    }
                    if let Some(g) = gt.iter().filter(|g| g.pitch == n.pitch).min_by(|a, b| (a.t - n.t).abs().total_cmp(&(b.t - n.t).abs())) {
                        if (g.t - n.t).abs() < 0.3 {
                            late.push(((n.t - g.t) * 1000.0) as i32);
                            continue;
                        }
                    }
                    let near = gt.iter().filter(|g| g.t <= n.t + 0.05 && g.t + g.dur >= n.t).map(|g| n.pitch as i32 - g.pitch as i32).min_by_key(|d| d.abs());
                    *hist.entry(near.unwrap_or(999)).or_default() += 1;
                }
                if let Ok(pp) = std::env::var("PIANO_EVAL_PITCH") {
                    let pp: u8 = pp.parse().unwrap_or(63);
                    for g in gt.iter().filter(|g| g.pitch == pp) {
                        eprintln!("ref {pp}: {:.2}+{:.2}", g.t, g.dur);
                    }
                    for n in notes.iter().filter(|n| n.pitch == pp) {
                        eprintln!("est {pp}: {:.2}+{:.2} v{}", n.t, n.dur, n.vel);
                    }
                }
                // missed reference onsets: was an estimate of that pitch already sounding (merged into
                // a held note), did one start late (> 50 ms), or nothing at all
                let (mut merged, mut late_on, mut absent) = (0, 0, 0);
                for g in &gt {
                    if notes.iter().any(|n| n.pitch == g.pitch && (n.t - g.t).abs() <= 0.05) {
                        continue;
                    }
                    if notes.iter().any(|n| n.pitch == g.pitch && n.t < g.t && n.t + n.dur > g.t + 0.05) {
                        merged += 1;
                    } else if notes.iter().any(|n| n.pitch == g.pitch && n.t > g.t && n.t < g.t + g.dur) {
                        late_on += 1;
                    } else {
                        absent += 1;
                    }
                }
                eprintln!("FN: merged into a held note {merged}, started late {late_on}, absent {absent}");
                late.sort();
                eprintln!("FP: same pitch, onset off by (ms): {late:?}");
                eprintln!("FP: interval to the nearest sounding reference note (999 = none): {hist:?}");
            }
            gt_line = format!(
                "vs reference ({} notes): onset+-50ms P {p1:.2} R {r1:.2} F1 {f1:.2} | +-100ms P {p2:.2} R {r2:.2} F1 {f2:.2} | frame F1 {ff1:.2} (P {:.2} R {:.2})\n",
                gt.len(),
                tp / (tp + fp).max(1.0),
                tp / (tp + fneg).max(1.0)
            );
        }
    }
    let mut rep = format!("live path: {} notes over {secs:.0} s, {ms:.1} ms CPU per 0.5 s\n", notes.len());
    rep += &gt_line;
    rep += "section          notes  len_s  high/s  chroma  meldist  peakcov  extra/s  poly  restrk/s  oct/s\n";
    let mut all = vec![("all".to_string(), 0.0, secs)];
    all.extend(sections.iter().cloned());
    // frames with sound: audio chroma energy above 1 % of the 95th percentile
    let mut en: Vec<f32> = ca.iter().map(|c| c.iter().sum()).collect();
    en.sort_by(|a, b| a.total_cmp(b));
    let floor = 0.01 * en.get(en.len() * 95 / 100).copied().unwrap_or(1.0);
    for (name, a, b) in all {
        let ns: Vec<&Note> = notes.iter().filter(|n| n.t >= a && n.t < b).collect();
        let len = if ns.is_empty() { 0.0 } else { ns.iter().map(|n| n.dur).sum::<f64>() / ns.len() as f64 };
        let high = ns.iter().filter(|n| n.pitch > 84).count() as f64 / (b - a).max(1e-9);
        let (fa, fb) = ((a * 10.0) as usize, ((b * 10.0) as usize).min(ca.len()));
        let mut cs = Vec::new();
        let (mut ex, mut tot) = (0f32, 0f32);
        for f in fa..fb {
            if let Some((e, t)) = coverage(&peaks[f], &sounding[f]) {
                ex += e;
                tot += t;
            }
            if ca[f].iter().sum::<f32>() < floor {
                continue;
            }
            cs.push(cos(&ca[f], &cn[f]).unwrap_or(0.0));
        }
        let chroma = if cs.is_empty() { 0.0 } else { cs.iter().sum::<f32>() / cs.len() as f32 };
        let (ma0, ma1) = ((a * RATE as f64 / 512.0) as usize, ((b * RATE as f64 / 512.0) as usize).min(ma.len()).min(ms_.len()));
        let mut d = 0f32;
        let mut nd = 0;
        for f in ma0..ma1 {
            d += ma[f].iter().zip(&ms_[f]).map(|(x, y)| (x - y) * (x - y)).sum::<f32>().sqrt();
            nd += 1;
        }
        let meld = if nd > 0 { d / nd as f32 } else { 0.0 };
        let cov = if tot > 0.0 { ex / tot } else { 0.0 };
        // extra notes: pitch class not among the audio's 4 strongest chroma bins around the onset
        let extra = ns
            .iter()
            .filter(|n| {
                let f0 = ((n.t * 10.0) as usize).min(ca.len().saturating_sub(1));
                let mut c = [0f32; 12];
                for f in f0..(f0 + 3).min(ca.len()) {
                    for i in 0..12 {
                        c[i] += ca[f][i];
                    }
                }
                let mut idx: Vec<usize> = (0..12).collect();
                idx.sort_by(|&a, &b| c[b].total_cmp(&c[a]));
                !idx[..4].contains(&((n.pitch % 12) as usize))
            })
            .count() as f64
            / (b - a).max(1e-9);
        let poly = sounding[fa.min(sounding.len())..fb.min(sounding.len())].iter().map(|v| v.len()).sum::<usize>() as f64 / (fb.saturating_sub(fa)).max(1) as f64;
        // re-strikes: a note-on on a key that sounded within the last 300 ms
        let restrk = ns.iter().filter(|n| notes.iter().any(|m| m.pitch == n.pitch && m.t < n.t - 1e-6 && m.t + m.dur >= n.t - 0.3)).count() as f64 / (b - a).max(1e-9);
        // octave / 12th / two-octave pairs that start together (within 40 ms) and overlap
        let mut pairs = 0usize;
        for (i, x) in ns.iter().enumerate() {
            for y in ns[i + 1..].iter() {
                if y.t - x.t > 0.04 {
                    break;
                }
                let d = (x.pitch as i32 - y.pitch as i32).abs();
                if (d == 12 || d == 19 || d == 24) && (y.t - x.t).abs() <= 0.04 {
                    pairs += 1;
                }
            }
        }
        let octs = pairs as f64 / (b - a).max(1e-9);
        rep += &format!("{name:<16} {:>5}  {len:>5.2}  {high:>6.2}  {chroma:>6.3}  {meld:>7.2}  {:>6.1}%  {extra:>7.2}  {poly:>4.1}  {restrk:>8.2}  {octs:>5.2}\n", ns.len(), cov * 100.0);
    }
    rep
}
