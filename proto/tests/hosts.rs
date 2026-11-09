//! Host (MacBook system volume) protocol rules: wire shape, deltas, commands and the safety step.
use wavelink_proto::safety::{self, Reject, MAX_HOST_STEP};
use wavelink_proto::*;

fn host(id: &str, volume: u8, online: bool) -> Host {
    Host { id: id.into(), label: "MacBook".into(), volume, mute: false, online }
}

fn state_with(h: Host) -> State {
    State { rev: 1, server: "s".into(), hosts: vec![h], ..Default::default() }
}

#[test]
fn state_without_hosts_field_still_parses_and_hello_host_is_optional() {
    let old_state = r#"{"rev":3,"server":"box","inputs":[],"buses":[]}"#;
    let st: State = serde_json::from_str(old_state).expect("old State must still parse");
    assert!(st.hosts.is_empty());

    let old_hello = r#"{"type":"hello","protocol":1,"token":"t","client":"c","meters":false}"#;
    match ClientMsg::from_json(old_hello).expect("old Hello must still parse") {
        ClientMsg::Hello(h) => assert!(h.host.is_none()),
        m => panic!("expected Hello, got {m:?}"),
    }
}

#[test]
fn host_hello_round_trips_and_is_absent_when_none() {
    let plain = ClientMsg::Hello(Hello { protocol: PROTOCOL_VERSION, token: "t".into(), client: "c".into(), meters: false, host: None });
    assert!(!plain.to_json().contains("\"host\""), "a Hello without a host must not carry the key");

    let hh = HostHello { id: "macbook".into(), label: "MacBook".into(), volume: 42, mute: true };
    let with = ClientMsg::Hello(Hello { protocol: PROTOCOL_VERSION, token: "t".into(), client: "c".into(), meters: false, host: Some(hh.clone()) });
    let back = ClientMsg::from_json(&with.to_json()).unwrap();
    assert_eq!(back, with);
    match back {
        ClientMsg::Hello(h) => assert_eq!(h.host, Some(hh)),
        m => panic!("expected Hello, got {m:?}"),
    }
}

#[test]
fn diff_emits_host_volume_mute_online_and_relabel_is_structural() {
    let old = state_with(host("macbook", 50, true));

    let mut quiet = old.clone();
    quiet.rev = 2;
    quiet.hosts[0].volume = 60;
    quiet.hosts[0].mute = true;
    quiet.hosts[0].online = false;
    let d = old.diff(&quiet).expect("level changes are not structural");
    assert!(d.contains(&Delta::HostVolume { host: "macbook".into(), volume: 60 }), "{d:?}");
    assert!(d.contains(&Delta::HostMute { host: "macbook".into(), mute: true }), "{d:?}");
    assert!(d.contains(&Delta::HostOnline { host: "macbook".into(), online: false }), "{d:?}");

    let mut relabel = old.clone();
    relabel.hosts[0].label = "Studio Mac".into();
    assert!(old.diff(&relabel).is_none(), "a host relabel must send a full State");
}

#[test]
fn apply_host_deltas_and_unknown_host_is_false() {
    let mut st = state_with(host("macbook", 50, true));
    assert!(st.apply(&Delta::HostVolume { host: "macbook".into(), volume: 70 }));
    assert_eq!(st.host("macbook").unwrap().volume, 70);
    assert!(st.apply(&Delta::HostMute { host: "macbook".into(), mute: true }));
    assert!(st.host("macbook").unwrap().mute);
    assert!(st.apply(&Delta::HostOnline { host: "macbook".into(), online: false }));
    assert!(!st.host("macbook").unwrap().online);
    assert!(!st.apply(&Delta::HostVolume { host: "nope".into(), volume: 1 }));
    assert!(!st.apply(&Delta::HostOnline { host: "nope".into(), online: true }));
}

#[test]
fn set_host_volume_steps_at_most_25_lowering_unlimited_offline_and_unknown_rejected() {
    assert_eq!(MAX_HOST_STEP, 25);
    let st = state_with(host("macbook", 50, true));
    let none = |_: &str| false;

    // a raise past the step is cut to 25 points above the current volume, with a note
    let c = safety::check(&Command::SetHostVolume { host: "macbook".into(), volume: 90 }, &st, &none).unwrap();
    assert_eq!(c.cmds, vec![Command::SetHostVolume { host: "macbook".into(), volume: 75 }]);
    assert_eq!(c.note.as_deref(), Some("raised at most 25 per step"));

    // lowering is never limited
    let low = state_with(host("macbook", 90, true));
    let c = safety::check(&Command::SetHostVolume { host: "macbook".into(), volume: 0 }, &low, &none).unwrap();
    assert_eq!(c.cmds, vec![Command::SetHostVolume { host: "macbook".into(), volume: 0 }]);
    assert!(c.note.is_none());

    // out-of-range input is clamped to 100 before the step applies
    let c = safety::check(&Command::SetHostVolume { host: "macbook".into(), volume: 200 }, &st, &none).unwrap();
    assert_eq!(c.cmds, vec![Command::SetHostVolume { host: "macbook".into(), volume: 75 }]);

    // offline and unknown hosts are refused
    let off = state_with(host("macbook", 50, false));
    assert_eq!(safety::check(&Command::SetHostVolume { host: "macbook".into(), volume: 10 }, &off, &none), Err(Reject::HostOffline("macbook".into())));
    assert_eq!(safety::check(&Command::SetHostMute { host: "macbook".into(), mute: true }, &off, &none), Err(Reject::HostOffline("macbook".into())));
    assert_eq!(safety::check(&Command::SetHostVolume { host: "nope".into(), volume: 10 }, &st, &none), Err(Reject::UnknownHost("nope".into())));
    assert_eq!(safety::check(&Command::SetHostMute { host: "nope".into(), mute: false }, &st, &none), Err(Reject::UnknownHost("nope".into())));
}

/// A command as the delta it produces (volume and mute only; the test driver applies these).
fn as_delta(c: &Command) -> Option<Delta> {
    match c {
        Command::SetHostVolume { host, volume } => Some(Delta::HostVolume { host: host.clone(), volume: *volume }),
        Command::SetHostMute { host, mute } => Some(Delta::HostMute { host: host.clone(), mute: *mute }),
        _ => None,
    }
}

#[test]
fn host_volume_never_leaves_0_100_or_jumps_more_than_the_step() {
    let mut st = state_with(host("macbook", 50, true));
    let none = |_: &str| false;
    // deterministic LCG so a failure replays
    let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = move || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (seed >> 33) as u32
    };
    for step in 0..2000 {
        let old = st.host("macbook").unwrap().volume;
        match next() % 4 {
            0 | 1 => {
                let cmd = Command::SetHostVolume { host: "macbook".into(), volume: (next() % 256) as u8 };
                if let Ok(c) = safety::check(&cmd, &st, &none) {
                    for d in c.cmds.iter().filter_map(as_delta) {
                        assert!(st.apply(&d));
                    }
                }
                let new = st.host("macbook").unwrap().volume;
                assert!(new <= 100, "step {step}: volume {new} above 100");
                if new > old {
                    assert!(new - old <= MAX_HOST_STEP, "step {step}: raised {old} -> {new}");
                }
            }
            2 => {
                // a host's own report (volume keys): clamped on the engine side, may move freely
                st.hosts[0].volume = ((next() % 256) as u8).min(100);
                assert!(st.host("macbook").unwrap().volume <= 100, "step {step}");
            }
            _ => {
                st.hosts[0].online = !st.hosts[0].online;
            }
        }
    }
}
