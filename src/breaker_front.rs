//! Breaker round 1 (frontend + invariants). Test-only, never production code.
use crate::config::{flat_eq, Config, Input, InputKind, Rack};
use crate::notes::{midi_bytes, Note};
use crate::stemcfg::{ensure_channel, sources};

fn user_input(id: &str, name: &str, target: &str) -> Input {
    Input {
        id: id.into(),
        name: name.into(),
        color: "#ffffff".into(),
        kind: InputKind::Source { target: target.into() },
        fx: "clean".into(),
        eq: flat_eq(),
        gain_db: 0.0,
        mute: false,
        routes: Default::default(),
        default_sink: false,
        rack: Rack::default(),
        normalize: false,
        norm_base: None,
        norm_out: None,
    }
}

/// Break 1 (high): Config::new_id("Stems") yields the id "stems" when the user names a channel
/// "Stems" before the feature is first used (ui.rs add-input path). stemcfg::sources() then drops
/// every input whose id is CHANNEL_ID, so the user's own channel vanishes from the source picker.
#[test]
fn breaker_user_channel_named_stems_stays_a_source() {
    let mut cfg = Config::default_config();
    assert!(!cfg.inputs.iter().any(|i| i.id == "stems"), "precondition: default config has no stems id");
    let user_id = cfg.new_id("Stems");
    assert_eq!(user_id, "stems", "precondition: new_id maps the name Stems to id stems");
    cfg.inputs.push(user_input(&user_id, "Stems", "easyeffects_source"));
    let srcs = sources(&cfg);
    assert!(
        srcs.iter().any(|(id, name)| id == "stems" && name == "Stems"),
        "user channel named Stems is missing from the source picker: {srcs:?}"
    );
}

/// Break 1b (high): with a user input already holding id "stems", ensure_channel returns false and
/// creates nothing. start() then writes the session's MIDI/audio into that user channel (out
/// "ch_stems"), so the user's own channel is altered by starting Stems.
#[test]
fn breaker_ensure_channel_does_not_reuse_user_channel() {
    let mut cfg = Config::default_config();
    cfg.inputs.push(user_input("stems", "Stems", "easyeffects_source"));
    let before = cfg.inputs.clone();
    let added = ensure_channel(&mut cfg, "spotify");
    assert!(added, "ensure_channel reused the user's 'stems' channel instead of creating the Stems channel");
    assert_eq!(cfg.inputs.iter().filter(|i| i.id == "stems").count(), 1, "stems id should stay unique");
    assert_eq!(
        cfg.inputs.iter().find(|i| i.id == "stems").map(|i| i.kind.clone()),
        Some(before.iter().find(|i| i.id == "stems").unwrap().kind.clone()),
        "the user's stems channel was changed"
    );
}

/// Break 2 (low): midi_bytes sorts (tick, kind) with note-off (kind 0) before note-on (kind 1).
/// A note whose length rounds to zero ticks (dur below ~0.5 ms) therefore writes its note-off
/// before its note-on, and the note sounds forever in a MIDI player.
#[test]
fn breaker_zero_length_note_starts_before_it_stops() {
    let bass = vec![Note { t: 0.0, dur: 0.0, pitch: 50, vel: 100 }];
    let bytes = midi_bytes(&[vec![], bass, vec![]]);
    let on = bytes.windows(3).position(|w| (w[0] & 0xf0) == 0x90 && w[1] == 50 && w[2] > 0);
    let off = bytes.windows(3).position(|w| (w[0] & 0xf0) == 0x80 && w[1] == 50);
    let on = on.expect("no note-on written for pitch 50");
    let off = off.expect("no note-off written for pitch 50");
    assert!(on < off, "note-off (byte {off}) is written before its note-on (byte {on}): the note never stops");
}

/// Break 3 (medium, static): save_midi names files to the second and calls fs::write, which
/// silently replaces an existing file. Two Save clicks in one second drain different notes, so
/// the first saved file is overwritten and both status lines still say "saved". Static check on
/// the source because save_midi is GTK code and cannot run headless.
#[test]
fn breaker_save_midi_does_not_overwrite_same_second() {
    let src = include_str!("stems_view.rs");
    let subsecond = src.contains("%3f") || src.contains("%.3f") || src.contains("%f");
    let guarded = src.contains("exists()") || src.contains("create_new");
    assert!(
        subsecond || guarded,
        "save_midi uses a whole-second stamp with no existence check, so a second save in the same second overwrites the first file"
    );
}
