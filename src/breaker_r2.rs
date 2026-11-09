//! Breaker round 2 (PR #16, fix round F1 to F5, head 91b9df3). Test-only, never production code.
//! Only public items are reachable from the crate root, so F3 (`create_midi`, `pub(super)`) and F4
//! (`apply_edit`, private) get no test here; see the comment for what was read instead.

use crate::config::{flat_eq, Config, Input, InputKind, Rack};
use crate::notes::{midi_bytes, Note};
use crate::stemcfg::sources;

fn user_input(id: &str, name: &str) -> Input {
    Input {
        id: id.into(),
        name: name.into(),
        color: "#ffffff".into(),
        kind: InputKind::Source { target: "easyeffects_source".into() },
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

/// Reads a SMF produced by midi_bytes: returns each track as (absolute tick, raw event bytes).
fn parse_tracks(b: &[u8]) -> Vec<Vec<(u64, Vec<u8>)>> {
    assert_eq!(&b[0..4], b"MThd");
    let hdr = u32::from_be_bytes(b[4..8].try_into().unwrap()) as usize;
    let mut p = 8 + hdr;
    let mut tracks = Vec::new();
    while p + 8 <= b.len() {
        assert_eq!(&b[p..p + 4], b"MTrk");
        let len = u32::from_be_bytes(b[p + 4..p + 8].try_into().unwrap()) as usize;
        let body = &b[p + 8..p + 8 + len];
        p += 8 + len;
        let (mut i, mut tick, mut evs) = (0usize, 0u64, Vec::new());
        while i < body.len() {
            let (mut dt, mut n) = (0u64, 0usize);
            loop {
                let c = body[i + n];
                n += 1;
                dt = (dt << 7) | (c & 0x7f) as u64;
                if c & 0x80 == 0 {
                    break;
                }
            }
            i += n;
            tick += dt;
            let (len_ev, ev) = if body[i] == 0xff {
                let l = body[i + 2] as usize;
                (3 + l, body[i..i + 3 + l].to_vec())
            } else {
                let k = if body[i] & 0xf0 == 0xc0 { 2 } else { 3 };
                (k, body[i..i + k].to_vec())
            };
            i += len_ev;
            evs.push((tick, ev));
        }
        tracks.push(evs);
    }
    tracks
}

/// Whether `key` is sounding after every event with tick <= `upto`, replayed the way a synth does
/// (note-on with velocity > 0 starts it, note-off or velocity 0 stops it).
fn sounding_at(evs: &[(u64, Vec<u8>)], key: u8, upto: u64) -> bool {
    let mut on = false;
    for (tick, m) in evs {
        if *tick > upto {
            break;
        }
        if m.len() == 3 && m[1] == key {
            match m[0] & 0xf0 {
                0x90 if m[2] > 0 => on = true,
                0x90 | 0x80 => on = false,
                _ => {}
            }
        }
    }
    on
}

/// Breaker F2 (round 1 open item, NEEDS 3): a config that already holds a user channel with id "stems"
/// (made on main, before new_id reserved CHANNEL_ID) must still show it in the source picker.
/// The fix only protects new ids. This test is expected to fail on the head.
#[test]
fn breaker_r2_legacy_stems_id_user_channel_hidden() {
    let mut cfg = Config::default_config();
    cfg.inputs.push(user_input("stems", "Stems"));
    let srcs = sources(&cfg);
    assert!(
        srcs.iter().any(|(id, name)| id == "stems" && name == "Stems"),
        "a user channel named Stems with id \"stems\" is hidden from the source picker: {srcs:?}"
    );
}

/// Breaker F5 (fix-round regression): a zero-length note at tick T has its note-off forced to T+1.
/// A same-pitch note that starts in that same tick (t offset 0.2 ms) then gets cut at T+1. Expected:
/// the second note still sounds at tick 1. This test is expected to fail on the head.
#[test]
fn breaker_r2_zero_length_note_cuts_same_tick_neighbour() {
    let bass = vec![
        Note { t: 0.0, dur: 0.0, pitch: 60, vel: 100 },
        Note { t: 0.0002, dur: 0.5, pitch: 60, vel: 100 },
    ];
    let bytes = midi_bytes(&[vec![], bass, vec![]]);
    let evs = &parse_tracks(&bytes)[2];
    assert!(
        sounding_at(evs, 60, 1),
        "the second note (pitch 60, starts at tick 0) is already off at tick 1: {evs:?}"
    );
}
