//! Breaker round 1 (frontend + invariants). Test-only, never production code.
use crate::config::{flat_eq, Config, Input, InputKind, Rack};
use crate::notes::{midi_bytes, Note};
use crate::stemcfg::{ensure_channel, sources, CHANNEL_ID};

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

/// Break 1 (high): a user channel named "Stems" must not take the Stems channel id. Config::new_id is the
/// real path (ui.rs add-input). The user's channel must stay in the source picker.
#[test]
fn breaker_user_channel_named_stems_stays_a_source() {
    let mut cfg = Config::default_config();
    let user_id = cfg.new_id("Stems");
    assert_ne!(user_id, CHANNEL_ID, "new_id gave the user channel the Stems channel id");
    cfg.inputs.push(user_input(&user_id, "Stems", "easyeffects_source"));
    let srcs = sources(&cfg);
    assert!(
        srcs.iter().any(|(id, name)| *id == user_id && name == "Stems"),
        "user channel named Stems is missing from the source picker: {srcs:?}"
    );
}

/// Break 1b (high): with a user channel named "Stems" (made through new_id), ensure_channel must still
/// create the Stems channel and leave the user's channel alone.
#[test]
fn breaker_ensure_channel_does_not_reuse_user_channel() {
    let mut cfg = Config::default_config();
    let user_id = cfg.new_id("Stems");
    cfg.inputs.push(user_input(&user_id, "Stems", "easyeffects_source"));
    let before = cfg.inputs.iter().find(|i| i.id == user_id).unwrap().clone();
    let added = ensure_channel(&mut cfg, "spotify");
    assert!(added, "ensure_channel did not create the Stems channel next to the user channel");
    assert_eq!(cfg.inputs.iter().filter(|i| i.id == CHANNEL_ID).count(), 1, "the Stems channel id should be unique");
    let after = cfg.inputs.iter().find(|i| i.id == user_id).unwrap();
    assert_eq!(after.kind, before.kind, "the user channel was changed");
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
