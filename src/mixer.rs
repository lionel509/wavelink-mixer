//! The seam between the mixer window and wherever its state and PipeWire live (#44).
//! Test-first: the `Mixer` trait, `Locals`, `LocalMixer` and `RemoteMixer` are written in the next commits.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Rack};
    use crate::pw::{GraphWire, Side};
    use wavelink_proto::{ClientMsg, PwCmd, ServerMsg};

    #[test]
    fn locals_for_remote_are_all_false_and_local_all_true() {
        // I3: a remote window starts no window-local engine
        assert_eq!(
            Locals::for_mixer(false),
            Locals { taps: false, sysmon: false, midi: false, norm: false, leak: false, restart: false }
        );
        assert_eq!(
            Locals::for_mixer(true),
            Locals { taps: true, sysmon: true, midi: true, norm: true, leak: true, restart: true }
        );
    }

    #[test]
    fn remote_save_sends_save_config_with_base_and_mine_and_advances_base() {
        let (m, _feed, out) = RemoteMixer::for_test();
        let mut base = Config::default_config();
        let mut mine = base.clone();
        mine.inputs[0].name = "Renamed".into();
        m.save_cfg(&mut mine, &mut base, &|_, _, _| {}).unwrap();
        match out.try_recv().unwrap() {
            ClientMsg::SaveConfig { base: b, mine: mi, .. } => {
                assert_eq!(b, serde_json::to_value(Config::default_config()).unwrap(), "base is the common ancestor");
                assert_eq!(mi, serde_json::to_value(&mine).unwrap());
            }
            m => panic!("{m:?}"),
        }
        assert_eq!(base, mine, "base advances to what was sent");
    }

    #[test]
    fn remote_pull_merges_the_cached_config_three_way() {
        let (m, feed, _out) = RemoteMixer::for_test();
        let mut hub = Config::default_config();
        hub.inputs[0].name = "Hub".into();
        feed.send(ServerMsg::Config { config: serde_json::to_value(&hub).unwrap() }).unwrap();
        m.events();
        let mut mine = Config::default_config();
        mine.inputs[0].gain_db = -3.0;
        let mut base = Config::default_config();
        let got = m.pull_cfg(&mut mine, &mut base, &|_, _, _| {}).unwrap();
        assert!(got.is_some(), "a new hub config is pulled");
        assert_eq!(mine.inputs[0].name, "Hub", "their rename");
        assert_eq!(mine.inputs[0].gain_db, -3.0, "my gain");
    }

    #[test]
    fn remote_writes_map_to_pw_cmds() {
        let (m, _feed, out) = RemoteMixer::for_test();
        m.set_volume(1, 0.5);
        m.set_volume_mute(2, 0.25, true);
        m.set_mute(3, false);
        m.set_eq(4, &[1.0, 2.0]);
        m.set_rack(5, &Rack::default());
        m.move_stream(6, "sink");
        m.link("o", "i");
        m.link_side("o", "i", Side::Left);
        let got: Vec<PwCmd> = (0..8)
            .map(|_| match out.try_recv().unwrap() {
                ClientMsg::Pw { cmd, .. } => cmd,
                m => panic!("{m:?}"),
            })
            .collect();
        assert_eq!(
            got,
            vec![
                PwCmd::SetVolume { id: 1, v: 0.5 },
                PwCmd::SetVolumeMute { id: 2, v: 0.25, mute: true },
                PwCmd::SetMute { id: 3, mute: false },
                PwCmd::SetEq { id: 4, eq: vec![1.0, 2.0] },
                PwCmd::SetRack { id: 5, rack: serde_json::to_value(Rack::default()).unwrap() },
                PwCmd::MoveStream { serial: 6, sink: "sink".into() },
                PwCmd::Link { out: "o".into(), inp: "i".into() },
                PwCmd::LinkSide { out: "o".into(), inp: "i".into(), side: "left".into() },
            ]
        );
    }

    #[test]
    fn remote_is_ready_only_after_config_and_graph() {
        let (m, feed, _out) = RemoteMixer::for_test();
        assert!(!m.ready());
        feed.send(ServerMsg::Config { config: serde_json::to_value(Config::default_config()).unwrap() }).unwrap();
        m.events();
        assert!(!m.ready(), "config alone is not enough");
        feed.send(ServerMsg::Graph { graph: serde_json::to_value(GraphWire::default()).unwrap() }).unwrap();
        m.events();
        assert!(m.ready());
    }
}
