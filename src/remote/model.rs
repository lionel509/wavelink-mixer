//! config.json <-> protocol state, and applying a (checked) command to the config.
use crate::config::{BusKind, Config, InputKind, Route};
use std::collections::BTreeMap;
use wavelink_proto as p;

pub fn to_state(cfg: &Config, server: &str, rev: u64, caps: &BTreeMap<String, f64>) -> p::State {
    let inputs = cfg
        .inputs
        .iter()
        .map(|i| p::Input {
            id: i.id.clone(),
            name: i.name.clone(),
            color: i.color.clone(),
            source: match &i.kind {
                InputKind::Virtual => p::InputSource::App,
                InputKind::Source { target } => p::InputSource::Device { target: target.clone() },
            },
            gain_db: i.gain_db,
            mute: i.mute,
            normalize: i.normalize,
            norm_target_db: i.norm_out,
            routes: cfg.buses.iter().map(|b| (b.id.clone(), { let r = i.route(&b.id); p::Route { on: r.on, level_db: r.level_db } })).collect(),
        })
        .collect();
    let buses = cfg
        .buses
        .iter()
        .map(|b| {
            let (kind, outputs) = match &b.kind {
                BusKind::Hardware { target } => (p::BusKind::Physical, vec![p::Output { node: target.clone(), label: b.name.clone(), host: None }]),
                BusKind::Virtual { node, description } => (p::BusKind::Virtual, vec![p::Output { node: node.clone(), label: description.clone(), host: None }]),
            };
            p::Bus {
                id: b.id.clone(),
                name: b.name.clone(),
                color: b.color.clone(),
                kind,
                gain_db: b.gain_db,
                mute: b.mute,
                outputs,
                max_db: super::settings::cap_for(caps, b),
                ceiling_db: super::settings::ceiling_for(b),
            }
        })
        .collect();
    p::State { rev, server: server.to_string(), inputs, buses, zones: vec![], hosts: vec![] }
}

/// Write one already-checked command into the config. False if it names nothing (raced with a structure change).
pub fn apply(cfg: &mut Config, cmd: &p::Command) -> bool {
    match cmd {
        // scaffold (U2 todo): host commands never touch config.json
        p::Command::SetHostVolume { .. } | p::Command::SetHostMute { .. } => false,
        p::Command::SetInputGain { input, db } => match cfg.inputs.iter_mut().find(|i| &i.id == input) {
            Some(i) => {
                i.gain_db = *db;
                // a remote fader move is his move: it becomes his position for auto-normalize (the GUI re-anchors the level)
                if i.normalize || i.norm_base.is_some() {
                    i.norm_base = Some(*db);
                }
                true
            }
            None => false,
        },
        p::Command::SetInputMute { input, mute } => cfg.inputs.iter_mut().find(|i| &i.id == input).map(|i| i.mute = *mute).is_some(),
        p::Command::SetRoute { input, bus, on, level_db } => {
            if !cfg.buses.iter().any(|b| &b.id == bus) {
                return false;
            }
            match cfg.inputs.iter_mut().find(|i| &i.id == input) {
                Some(i) => {
                    let r = i.routes.entry(bus.clone()).or_insert(Route { on: false, level_db: 0.0 });
                    if let Some(on) = on {
                        r.on = *on;
                    }
                    if let Some(l) = level_db {
                        r.level_db = *l;
                    }
                    true
                }
                None => false,
            }
        }
        p::Command::SetNormalize { input, on, target_db } => match cfg.inputs.iter_mut().find(|i| &i.id == input) {
            Some(i) => {
                if let Some(on) = on {
                    i.normalize = *on;
                }
                if let Some(t) = target_db {
                    i.norm_out = Some(*t);
                    if i.norm_base.is_none() {
                        i.norm_base = Some(i.gain_db);
                    }
                }
                true
            }
            None => false,
        },
        // never above the bus's hard ceiling (#35), whatever the command says
        p::Command::SetBusGain { bus, db, .. } => cfg.buses.iter_mut().find(|b| &b.id == bus).map(|b| b.gain_db = b.clamp_db(*db)).is_some(),
        p::Command::SetBusMute { bus, mute } => cfg.buses.iter_mut().find(|b| &b.id == bus).map(|b| b.mute = *mute).is_some(),
    }
}
