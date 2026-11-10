use std::collections::BTreeMap;
use wavelink_proto::safety::{Reject, RaiseLimiter, MAX_RAISE_PER_WINDOW_DB, MAX_STEP_DB, RAISE_WINDOW_S};
use wavelink_proto::*;

fn check(c: &Command, s: &State) -> Result<wavelink_proto::safety::Checked, Reject> {
    wavelink_proto::safety::check(c, s, &|_| false)
}

fn state() -> State {
    let mut routes = BTreeMap::new();
    routes.insert("a1".to_string(), Route { on: true, level_db: -3.0 });
    State {
        rev: 7,
        server: "bazzite".into(),
        inputs: vec![
            Input {
                id: "spotify".into(),
                name: "Spotify".into(),
                color: "#1ed760".into(),
                source: InputSource::App,
                gain_db: -4.0,
                mute: false,
                normalize: true,
                norm_target_db: Some(-22.0),
                routes: routes.clone(),
            },
            Input {
                id: "mic".into(),
                name: "Mic".into(),
                color: "#bf5af2".into(),
                source: InputSource::Device { target: "easyeffects_source".into() },
                gain_db: 0.0,
                mute: true,
                normalize: false,
                norm_target_db: None,
                routes: BTreeMap::new(),
            },
        ],
        buses: vec![
            Bus {
                id: "a1".into(),
                name: "Headphones".into(),
                color: "#64d2ff".into(),
                kind: BusKind::Physical,
                gain_db: -10.0,
                mute: false,
                outputs: vec![Output { node: "alsa_output.x".into(), label: "Scarlett".into(), host: None }],
                max_db: None,
                ceiling_db: None,
            },
            Bus {
                id: "a3".into(),
                name: "Sonos".into(),
                color: "#ff9f0a".into(),
                kind: BusKind::Physical,
                gain_db: -30.0,
                mute: false,
                outputs: vec![Output { node: "raop_sonos".into(), label: "Sonos".into(), host: None }],
                max_db: Some(-20.0),
                ceiling_db: Some(-10.0),
            },
            Bus {
                id: "b1".into(),
                name: "Stream".into(),
                color: "#ff453a".into(),
                kind: BusKind::Virtual,
                gain_db: 0.0,
                mute: false,
                outputs: vec![],
                max_db: None,
                ceiling_db: None,
            },
        ],
        zones: vec![],
        hosts: vec![],
    }
}

fn rt_client(m: ClientMsg) {
    let s = m.to_json();
    assert_eq!(ClientMsg::from_json(&s).unwrap(), m, "{s}");
}
fn rt_server(m: ServerMsg) {
    let s = m.to_json();
    assert_eq!(ServerMsg::from_json(&s).unwrap(), m, "{s}");
}

#[test]
fn client_messages_round_trip() {
    rt_client(ClientMsg::Hello(Hello { protocol: PROTOCOL_VERSION, token: "abc".into(), client: "test".into(), meters: true, host: None }));
    rt_client(ClientMsg::Subscribe { meters: false });
    rt_client(ClientMsg::Ping { t: 42 });
    for cmd in [
        Command::SetInputGain { input: "spotify".into(), db: -6.5 },
        Command::SetInputMute { input: "mic".into(), mute: true },
        Command::SetRoute { input: "spotify".into(), bus: "a1".into(), on: Some(false), level_db: None },
        Command::SetRoute { input: "spotify".into(), bus: "a1".into(), on: None, level_db: Some(-12.0) },
        Command::SetNormalize { input: "spotify".into(), on: Some(true), target_db: Some(-18.0) },
        Command::SetBusGain { bus: "a3".into(), db: -25.0, unlock: false },
        Command::SetBusGain { bus: "a3".into(), db: -10.0, unlock: true },
        Command::SetBusMute { bus: "a1".into(), mute: true },
    ] {
        rt_client(ClientMsg::Command { id: 9, cmd });
    }
}

#[test]
fn server_messages_round_trip() {
    rt_server(ServerMsg::Welcome { protocol: 1, server: "bazzite".into(), version: "0.1.0".into() });
    rt_server(ServerMsg::Error { code: ErrorCode::Auth, message: "bad token".into() });
    rt_server(ServerMsg::State { state: state() });
    rt_server(ServerMsg::Delta {
        rev: 8,
        changes: vec![
            Delta::InputGain { input: "spotify".into(), db: -1.0 },
            Delta::InputMute { input: "mic".into(), mute: false },
            Delta::Route { input: "spotify".into(), bus: "b1".into(), route: Route { on: true, level_db: -6.0 } },
            Delta::Normalize { input: "spotify".into(), on: false, target_db: None },
            Delta::BusGain { bus: "a1".into(), db: -8.0 },
            Delta::BusMute { bus: "b1".into(), mute: true },
        ],
    });
    rt_server(ServerMsg::Ack { id: 3, ok: true, error: None, note: Some("capped".into()) });
    rt_server(ServerMsg::Ack { id: 4, ok: false, error: Some("unknown bus 'zz'".into()), note: None });
    rt_server(ServerMsg::Meters {
        meters: Meters { inputs: vec![Level { id: "spotify".into(), peak_db: -3.5, rms_db: -18.0 }], buses: vec![] },
    });
    rt_server(ServerMsg::Pong { t: 5 });
}

#[test]
fn wire_format_is_stable() {
    // the documented shape other clients (scripts, a future web UI) rely on
    let s = ClientMsg::Command { id: 1, cmd: Command::SetBusGain { bus: "a3".into(), db: -25.0, unlock: false } }.to_json();
    assert_eq!(s, r#"{"type":"command","id":1,"cmd":{"op":"set_bus_gain","bus":"a3","db":-25.0}}"#);
    let h: ClientMsg = ClientMsg::from_json(r#"{"type":"hello","protocol":1,"token":"t"}"#).unwrap();
    assert_eq!(h, ClientMsg::Hello(Hello { protocol: 1, token: "t".into(), client: String::new(), meters: false, host: None }));
}

#[test]
fn unknown_fields_and_kinds_are_tolerated() {
    // a newer server may add fields and new kinds within the same protocol
    let mut v = serde_json::to_value(ServerMsg::State { state: state() }).unwrap();
    v["state"]["buses"][0]["kind"] = "network_speaker".into();
    v["state"]["buses"][0]["latency_ms"] = 12.into();
    v["state"]["inputs"][0]["source"] = serde_json::json!({"type": "remote_stream", "url": "x"});
    let m: ServerMsg = serde_json::from_value(v).unwrap();
    let ServerMsg::State { state } = m else { panic!() };
    assert_eq!(state.buses[0].kind, BusKind::Other);
    assert_eq!(state.inputs[0].source, InputSource::Other);
}

#[test]
fn diff_then_apply_reproduces_new_state() {
    let old = state();
    let mut new = old.clone();
    new.rev = 8;
    new.inputs[0].gain_db = -9.0;
    new.inputs[0].routes.insert("b1".into(), Route { on: true, level_db: -20.0 });
    new.inputs[0].routes.remove("a1");
    new.inputs[1].mute = false;
    new.inputs[1].norm_target_db = Some(-30.0);
    new.buses[1].gain_db = -24.0;
    new.buses[2].mute = true;
    let d = old.diff(&new).expect("same structure");
    assert_eq!(d.len(), 7, "{d:?}");
    let mut got = old.clone();
    for x in &d {
        assert!(got.apply(x));
    }
    got.rev = new.rev;
    // a removed route reads back as the default (off, 0 dB)
    assert_eq!(got.inputs[0].route("a1"), Route::default());
    got.inputs[0].routes.remove("a1");
    assert_eq!(got, new);
    assert_eq!(old.diff(&old), Some(vec![]));
}

#[test]
fn structural_change_has_no_diff() {
    let old = state();
    let mut renamed = old.clone();
    renamed.inputs[0].name = "Music".into();
    assert_eq!(old.diff(&renamed), None);
    let mut more = old.clone();
    more.buses.push(Bus { id: "a4".into(), ..old.buses[0].clone() });
    assert_eq!(old.diff(&more), None);
    let mut capped = old.clone();
    capped.buses[0].max_db = Some(-10.0);
    assert_eq!(old.diff(&capped), None);
}

#[test]
fn check_rejects_unknown_and_nan() {
    let s = state();
    assert_eq!(check(&Command::SetInputGain { input: "nope".into(), db: 0.0 }, &s), Err(Reject::UnknownInput("nope".into())));
    assert_eq!(check(&Command::SetBusMute { bus: "zz".into(), mute: true }, &s), Err(Reject::UnknownBus("zz".into())));
    assert_eq!(check(&Command::SetInputGain { input: "mic".into(), db: f64::NAN }, &s), Err(Reject::NotFinite));
    assert_eq!(check(&Command::SetBusGain { bus: "a1".into(), db: f64::INFINITY, unlock: false }, &s), Err(Reject::NotFinite));
    assert_eq!(check(&Command::SetRoute { input: "mic".into(), bus: "a1".into(), on: None, level_db: None }, &s), Err(Reject::Empty));
    assert_eq!(check(&Command::SetRoute { input: "mic".into(), bus: "nope".into(), on: Some(true), level_db: None }, &s), Err(Reject::UnknownBus("nope".into())));
    assert_eq!(check(&Command::SetNormalize { input: "mic".into(), on: None, target_db: None }, &s), Err(Reject::Empty));
}

/// `unlock` = this connection holds an unlock for the bus (the command's own flag is ignored).
fn bus_gain(s: &State, bus: &str, db: f64, unlock: bool) -> f64 {
    let b = bus.to_string();
    let c = wavelink_proto::safety::check(&Command::SetBusGain { bus: bus.into(), db, unlock: true }, s, &move |x| unlock && x == b).unwrap();
    match c.cmds.as_slice() {
        [Command::SetBusGain { db, .. }] => *db,
        x => panic!("{x:?}"),
    }
}

#[test]
fn faders_clamp_to_range() {
    let s = state();
    let c = check(&Command::SetInputGain { input: "mic".into(), db: 40.0 }, &s).unwrap();
    assert_eq!(c.cmds, vec![Command::SetInputGain { input: "mic".into(), db: MAX_DB }]);
    let c = check(&Command::SetRoute { input: "mic".into(), bus: "a1".into(), on: None, level_db: Some(-500.0) }, &s).unwrap();
    assert_eq!(c.cmds, vec![Command::SetRoute { input: "mic".into(), bus: "a1".into(), on: None, level_db: Some(MIN_DB) }]);
    let c = check(&Command::SetNormalize { input: "mic".into(), on: None, target_db: Some(3.0) }, &s).unwrap();
    assert_eq!(c.cmds, vec![Command::SetNormalize { input: "mic".into(), on: None, target_db: Some(NORM_TARGET_MAX_DB) }]);
    // virtual buses: no step limit, just the range
    assert_eq!(bus_gain(&s, "b1", 12.0, false), 12.0);
    assert_eq!(bus_gain(&s, "b1", -60.0, false), -60.0);
}

#[test]
fn physical_bus_rises_at_most_one_step() {
    let s = state(); // a1 at -10
    assert_eq!(bus_gain(&s, "a1", 0.0, false), -10.0 + MAX_STEP_DB);
    assert_eq!(bus_gain(&s, "a1", -7.0, false), -7.0);
    // lowering is never limited
    assert_eq!(bus_gain(&s, "a1", -60.0, false), -60.0);
    let c = wavelink_proto::safety::check(&Command::SetBusGain { bus: "a1".into(), db: 12.0, unlock: true }, &s, &|_| true).unwrap();
    assert!(c.note.unwrap().contains("per step"));
}

#[test]
fn speaker_cap_needs_unlock() {
    let mut s = state(); // a3 at -30, cap -20
    assert_eq!(bus_gain(&s, "a3", -26.0, false), -26.0);
    // a step of 6 lands at -24, under the cap
    assert_eq!(bus_gain(&s, "a3", 0.0, false), -24.0);
    s.buses[1].gain_db = -22.0;
    assert_eq!(bus_gain(&s, "a3", 0.0, false), -20.0);
    // the command's own unlock flag is ignored: still capped
    let c = check(&Command::SetBusGain { bus: "a3".into(), db: -10.0, unlock: true }, &s).unwrap();
    assert!(c.note.unwrap().contains("capped at -20"));
    assert_eq!(c.cmds, vec![Command::SetBusGain { bus: "a3".into(), db: -20.0, unlock: false }]);
    // a server-side unlock lifts the soft cap, the step still applies
    assert_eq!(bus_gain(&s, "a3", 0.0, true), -16.0);
    assert_eq!(bus_gain(&s, "a3", -18.0, true), -18.0);
    // ... but never past the hard ceiling (-10 here)
    s.buses[1].gain_db = -14.0;
    assert_eq!(bus_gain(&s, "a3", 0.0, true), -10.0);
    s.buses[1].gain_db = -22.0;
    // already above the cap (set locally): any locked command brings it to the cap or lower
    s.buses[1].gain_db = -5.0;
    assert_eq!(bus_gain(&s, "a3", -8.0, false), -20.0);
    assert_eq!(bus_gain(&s, "a3", -40.0, false), -40.0);
}

#[test]
fn unmuting_a_loud_capped_bus_lowers_it_first() {
    let mut s = state();
    s.buses[1].gain_db = -3.0;
    s.buses[1].mute = true;
    let c = check(&Command::SetBusMute { bus: "a3".into(), mute: false }, &s).unwrap();
    assert_eq!(
        c.cmds,
        vec![Command::SetBusGain { bus: "a3".into(), db: -20.0, unlock: false }, Command::SetBusMute { bus: "a3".into(), mute: false }]
    );
    // unlocked: the soft cap no longer applies, the hard ceiling (-10) still does
    let c = wavelink_proto::safety::check(&Command::SetBusMute { bus: "a3".into(), mute: false }, &s, &|_| true).unwrap();
    assert_eq!(c.cmds[0], Command::SetBusGain { bus: "a3".into(), db: -10.0, unlock: false });
    // under the cap: plain unmute
    s.buses[1].gain_db = -30.0;
    let c = check(&Command::SetBusMute { bus: "a3".into(), mute: false }, &s).unwrap();
    assert_eq!(c.cmds, vec![Command::SetBusMute { bus: "a3".into(), mute: false }]);
}

#[test]
fn ceiling_applies_without_any_soft_cap() {
    let mut s = state();
    s.buses[0].ceiling_db = Some(-8.0); // a1 at -10, no soft cap
    assert_eq!(bus_gain(&s, "a1", 12.0, true), -8.0);
    s.buses[2].ceiling_db = Some(-3.0); // virtual b1
    assert_eq!(bus_gain(&s, "b1", 12.0, false), -3.0);
}

#[test]
fn raise_limiter_is_time_based() {
    let mut l = RaiseLimiter::default();
    // from -60: the first second allows up to -48 in total, however many commands
    assert_eq!(l.allowed(0.0, -60.0), -60.0 + MAX_RAISE_PER_WINDOW_DB);
    l.record(0.0, -60.0, -54.0);
    l.record(0.01, -54.0, -48.0);
    assert_eq!(l.allowed(0.02, -48.0), -48.0, "budget spent within the window");
    // decreases are never refused, and the lowest level in the window sets the floor
    l.record(0.03, -48.0, -58.0);
    assert_eq!(l.allowed(0.04, -58.0), -48.0);
    // once the window has passed, the floor is the current level again
    assert_eq!(l.allowed(0.04 + RAISE_WINDOW_S, -58.0), -58.0 + MAX_RAISE_PER_WINDOW_DB);
    assert!(MAX_STEP_DB <= MAX_RAISE_PER_WINDOW_DB);
}

#[test]
fn unlock_messages_round_trip() {
    rt_client(ClientMsg::Unlock { bus: "a3".into(), on: true });
    rt_server(ServerMsg::Unlock { bus: "a3".into(), seconds: 60 });
}
