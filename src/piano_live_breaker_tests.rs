//! Breaker round 1 for #23 / PR #30: the live session's shared state and threading.
//! Uses the WAV replay mode (MIXER_PIANO_WAV path), so no pw-cat, no ALSA port, nothing played.
use super::*;

fn wav16(pcm: &[f32], rate: u32) -> Vec<u8> {
    let mut b = Vec::new();
    let data = (pcm.len() * 2) as u32;
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * 2).to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data.to_le_bytes());
    for v in pcm {
        b.extend_from_slice(&((v.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    b
}

fn temp_wav(name: &str, notes: &[Note], secs: f64) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("wavelink-piano-breaker-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    std::fs::write(&p, wav16(&piano::synth_piano(notes, secs, 3), piano::RATE)).unwrap();
    p
}

fn opts(wav: &std::path::Path) -> Opts {
    Opts { tap: "breaker".into(), tap_sink: false, port: false, wav: Some(wav.to_path_buf()) }
}

fn wait_for(shared: &Shared, secs: f64, f: impl Fn(&Inner) -> bool) -> bool {
    let t0 = Instant::now();
    while t0.elapsed().as_secs_f64() < secs {
        if f(&shared.get()) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

/// RED (F3). `Session::drop` sets the stop flag and kills the tap but does not join the worker, so
/// the old worker keeps writing into the `Shared` after drop returns: its final `apply` overwrites
/// `now` (and closes notes), and replay writes "replay finished". In the UI a source change or a
/// Live-MIDI-port toggle is `stop(); start()` on the same `Shared`, so the new session's state gets
/// the old session's clock / notes / status. Expected: once drop returns, nothing writes any more.
#[test]
fn brk_session_drop_leaves_no_writer_behind() {
    let notes: Vec<Note> = (0..12).map(|i| Note { t: 0.2 + 0.25 * i as f64, dur: 0.2, pitch: 60 + (i % 5) as u8, vel: 90 }).collect();
    let wav = temp_wav("drop.wav", &notes, 3.5);
    let shared = Shared::new();
    let s = Session::start(opts(&wav), shared.clone());
    assert!(wait_for(&shared, 3.0, |g| g.now > 0.6), "replay never got going");
    drop(s);
    {
        let mut g = shared.get();
        g.status = "next session".into();
        g.now = -1.0;
    }
    std::thread::sleep(Duration::from_millis(150));
    let g = shared.get();
    assert!(g.status == "next session" && g.now == -1.0, "old worker wrote after drop: status {:?}, now {}", g.status, g.now);
}

/// RED (F3). Restarting on the same `Shared` (what the UI does on a source change or a
/// Live-MIDI-port toggle) wipes every note of the take that has not been saved yet:
/// `Session::start` resets `Inner`. Toggling the port mid-song silently loses the recording.
#[test]
fn brk_restart_keeps_unsaved_notes() {
    let notes: Vec<Note> = (0..4).map(|i| Note { t: 0.2 + 0.25 * i as f64, dur: 0.2, pitch: 60 + i as u8, vel: 90 }).collect();
    let wav = temp_wav("restart.wav", &notes, 1.4);
    let shared = Shared::new();
    let a = Session::start(opts(&wav), shared.clone());
    assert!(wait_for(&shared, 5.0, |g| g.status == "replay finished"), "replay A did not finish");
    let before = shared.get().all.len();
    assert!(before >= 3, "replay A found {before} notes");
    drop(a);
    let b = Session::start(opts(&wav), shared.clone());
    let after = shared.get().all.len();
    drop(b);
    assert!(after >= before, "restart dropped the unsaved take: {before} notes before, {after} after");
}

/// GREEN probe. Stop in the middle of a held note: the roll gets the note closed (nothing left in
/// `open`), and a sink sees an Off for every On.
#[test]
fn brk_stop_mid_note_closes_everything() {
    let wav = temp_wav("held.wav", &[Note { t: 0.2, dur: 2.5, pitch: 55, vel: 90 }], 3.0);
    let shared = Shared::new();
    let s = Session::start(opts(&wav), shared.clone());
    assert!(wait_for(&shared, 3.0, |g| !g.open.is_empty()), "note never opened");
    drop(s);
    assert!(wait_for(&shared, 1.0, |g| g.open.is_empty()), "note left open after stop");
    let notes = shared.get().take_notes();
    assert_eq!(notes.len(), 1, "{notes:?}");
}

/// Saving twice while a note is sounding writes that note into both files: the first save gets
/// it cut at `now`, the second gets it again from its real start. RED (F9, Low).
#[test]
fn brk_save_twice_does_not_duplicate_sounding_note() {
    let mut g = Inner::default();
    g.apply(&[Ev::On { t: 1.0, pitch: 60, vel: 90 }], 1.5);
    let first = g.take_notes();
    g.apply(&[Ev::Off { t: 3.0, pitch: 60 }], 3.2);
    let second = g.take_notes();
    let total: usize = [first.len(), second.len()].iter().sum();
    assert_eq!(total, 1, "first save {first:?}, second save {second:?}");
}

// ------------------------------------------------------------------------------------------------
// Round 2 (head 89b857b): joined drop, kept take, snapshot Save
// ------------------------------------------------------------------------------------------------

/// GREEN probe. A source change in the middle of a held note (drop + start on the same Shared):
/// the old session's note is closed into the take at the restart, the new session's notes continue
/// on the take clock after it, no same-pitch overlap, nothing left open after the last stop.
#[test]
fn brk2_restart_mid_note_keeps_a_consistent_take() {
    let wav = temp_wav("r2-held.wav", &[Note { t: 0.2, dur: 2.6, pitch: 55, vel: 90 }], 3.0);
    let shared = Shared::new();
    let a = Session::start(opts(&wav), shared.clone());
    assert!(wait_for(&shared, 3.0, |g| !g.open.is_empty() && g.now > 0.9), "A never sounded");
    drop(a);
    let restart_at = shared.get().now;
    assert!(shared.get().open.is_empty(), "A left a note open after its drop");
    let b = Session::start(opts(&wav), shared.clone());
    assert!(wait_for(&shared, 3.0, |g| !g.open.is_empty() && g.now > restart_at + 0.9), "B never sounded");
    drop(b);
    let g = shared.get();
    assert!(g.open.is_empty());
    let snap = g.snapshot();
    assert_eq!(snap.len(), 2, "{snap:?}");
    assert!((snap[0].t - 0.2).abs() < 0.06 && snap[0].t + snap[0].dur <= restart_at + 1e-6, "{snap:?} restart at {restart_at}");
    assert!(snap[1].t >= restart_at && (snap[1].t - (restart_at + 0.2)).abs() < 0.06, "{snap:?} restart at {restart_at}");
}

/// GREEN probe. Save while a note sounds is a copy: two saves in a row agree, the take is not
/// drained, and once the note ends the next save has it exactly once at full length.
#[test]
fn brk2_snapshot_while_sounding_is_a_copy() {
    let mut g = Inner::default();
    g.apply(&[Ev::On { t: 0.5, pitch: 60, vel: 90 }, Ev::On { t: 0.6, pitch: 64, vel: 80 }, Ev::Off { t: 0.9, pitch: 64 }], 1.0);
    let s1 = g.snapshot();
    let s2 = g.snapshot();
    assert_eq!(s1, s2);
    assert_eq!(s1.len(), 2);
    g.apply(&[Ev::Off { t: 2.0, pitch: 60 }], 2.1);
    let s3 = g.snapshot();
    assert_eq!(s3.iter().filter(|n| n.pitch == 60).count(), 1, "{s3:?}");
    assert!((s3.iter().find(|n| n.pitch == 60).unwrap().dur - 1.5).abs() < 1e-9);
}
