//! Breaker round 1, backend (PR #16 head 7a97a0f). Keeps only the tests that fail on the PR head.

use crate::notes::Note;
use crate::stemcfg::{Plays, SepEngine};
use crate::stems::{Opts, Session, Shared};
use std::time::Duration;

/// Stop, then start again, with notes left in the shared timeline from the first run.
/// The UI keeps one `Shared` per panel (stems_view.rs `State`), and `Session::start` resets only the clock.
/// HOME is pointed at a path that cannot be created, so the new session stops at model setup and never downloads.
#[test]
fn breaker_stale_notes_survive_new_session() {
    let old_home = std::env::var_os("HOME");
    std::env::set_var("HOME", "/dev/null/breaker-home");
    let shared = Shared::new();
    shared.timeline.lock().unwrap().add(1, &[Note { t: 12.0, dur: 0.5, pitch: 40, vel: 100 }]);
    let opts = Opts {
        tap: "ch_nothing".into(),
        tap_sink: false,
        engine: SepEngine::Live,
        plays: Plays::Nothing,
        out: "ch_stems".into(),
        buf_ms: 0,
        tier: None,
    };
    let session = Session::start(opts, shared.clone(), None);
    std::thread::sleep(Duration::from_millis(200));
    let clock = shared.now();
    let left = shared.timeline.lock().unwrap().drain_all();
    drop(session);
    if let Some(h) = old_home {
        std::env::set_var("HOME", h);
    }
    let n: usize = left.iter().map(|v| v.len()).sum();
    assert_eq!(clock, 0.0, "clock should restart at 0 for the new session");
    assert_eq!(n, 0, "new session inherited {n} note(s) from the previous run: {:?}", left[1]);
}
