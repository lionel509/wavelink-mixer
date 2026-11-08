//! Stems settings (~/.config/wavelink-mixer/stems.json) and the rules for which channel is the source.
//! Unit 1 skeleton: bodies are `todo!()` until Unit 5.
#![allow(dead_code, unused_imports, unused_variables)]
use crate::config::{glib_home, names, flat_eq, BusKind, Config, Input, InputKind, Rack, Route};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// The id of the input the feature creates.
pub const CHANNEL_ID: &str = "stems";

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, PartialEq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum SepEngine {
    #[default]
    Live,
    Quality,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, PartialEq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum Plays {
    #[default]
    Midi,
    Stems,
    Nothing,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct StemsCfg {
    pub source: String,
    pub engine: SepEngine,
    pub plays: Plays,
    /// Crash marker: the source id whose live mute is applied and must be undone on restart.
    pub replaced: Option<String>,
}

/// source "spotify", engine Live, plays Midi, replaced None.
impl Default for StemsCfg {
    fn default() -> Self {
        todo!()
    }
}

impl StemsCfg {
    /// Writes to a temp file, then renames it into path().
    pub fn save(&self) {
        todo!()
    }
}

/// glib_home()/.config/wavelink-mixer/stems.json
pub fn path() -> PathBuf {
    todo!()
}

pub fn load() -> StemsCfg {
    todo!()
}

/// Missing or invalid file -> default.
pub fn load_from(p: &Path) -> StemsCfg {
    todo!()
}

/// (id, name) of every input except CHANNEL_ID, in config order.
pub fn sources(cfg: &Config) -> Vec<(String, String)> {
    todo!()
}

/// `wanted` if it is a source, else "spotify" if it is one, else the first source, else None.
pub fn resolve(cfg: &Config, wanted: &str) -> Option<String> {
    todo!()
}

/// Virtual -> (names::ch(id), true); Source { target } -> (target, false).
pub fn tap(inp: &Input) -> (String, bool) {
    todo!()
}

/// If an input with id CHANNEL_ID exists, returns false and leaves cfg alone. Otherwise appends the
/// "Stems" Virtual input (Hardware buses copied from `source` and Virtual buses off; at least one Hardware
/// bus on) and returns true. Never edits another input.
pub fn ensure_channel(cfg: &mut Config, source: &str) -> bool {
    todo!()
}

/// cfg_mute || (!solo.is_empty() && !solo.contains(id)) || replaced == Some(id)
pub fn effective_mute(cfg_mute: bool, id: &str, solo: &HashSet<String>, replaced: Option<&str>) -> bool {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Deterministic uniform [0, 1) from an LCG (no `rand` crate).
    fn rnd(x: &mut u64) -> f64 {
        *x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (*x >> 11) as f64 / (1u64 << 53) as f64
    }

    fn random_routes(cfg: &Config, x: &mut u64) -> BTreeMap<String, Route> {
        cfg.buses
            .iter()
            .map(|b| {
                let on = rnd(x) < 0.5;
                let level_db = -40.0 + 40.0 * rnd(x);
                (b.id.clone(), Route { on, level_db })
            })
            .collect()
    }

    fn input(id: &str, kind: InputKind, routes: BTreeMap<String, Route>, mute: bool) -> Input {
        Input {
            id: id.into(),
            name: id.into(),
            color: "#ffffff".into(),
            kind,
            fx: "clean".into(),
            eq: flat_eq(),
            gain_db: 0.0,
            mute,
            routes,
            default_sink: false,
            rack: Rack::default(),
            normalize: false,
            norm_base: None,
            norm_out: None,
        }
    }

    #[test]
    fn stems_never_touch_other_channels() {
        for seed in 1..=300u64 {
            let mut x = seed;
            let mut cfg = Config::default_config();
            let extra = (rnd(&mut x) * 5.0) as usize;
            for _ in 0..extra {
                let id = cfg.new_id("ch");
                let kind = if rnd(&mut x) < 0.5 {
                    InputKind::Virtual
                } else {
                    InputKind::Source { target: format!("src_{}", (rnd(&mut x) * 10.0) as u32) }
                };
                let routes = random_routes(&cfg, &mut x);
                let mute = rnd(&mut x) < 0.5;
                cfg.inputs.push(input(&id, kind, routes, mute));
            }
            if rnd(&mut x) < 0.25 {
                let routes = random_routes(&cfg, &mut x);
                let mute = rnd(&mut x) < 0.5;
                cfg.inputs.push(input(CHANNEL_ID, InputKind::Virtual, routes, mute));
            }
            let srcs = sources(&cfg);
            if srcs.is_empty() {
                continue;
            }
            let pick = (rnd(&mut x) * srcs.len() as f64) as usize % srcs.len();
            let source = srcs[pick].0.clone();

            let before = cfg.clone();
            let had = before.inputs.iter().any(|i| i.id == CHANNEL_ID);
            let added = ensure_channel(&mut cfg, &source);

            // (a) every other input is unchanged, in the same order
            let others = |c: &Config| c.inputs.iter().filter(|i| i.id != CHANNEL_ID).cloned().collect::<Vec<_>>();
            assert_eq!(others(&cfg), others(&before), "seed {seed}: a non-stems input changed");

            // (b) added exactly when there was no stems input; a pre-existing one is untouched
            assert_eq!(added, !had, "seed {seed}: wrong added flag");
            let stems_now = cfg.inputs.iter().find(|i| i.id == CHANNEL_ID);
            if had {
                let was = before.inputs.iter().find(|i| i.id == CHANNEL_ID).unwrap();
                assert_eq!(stems_now.unwrap(), was, "seed {seed}: existing stems input changed");
            }

            // (c) the new input: Virtual, unmuted, Virtual buses off, a Hardware bus on
            if added {
                let s = stems_now.expect("stems input was added");
                assert_eq!(s.id, CHANNEL_ID);
                assert_eq!(s.kind, InputKind::Virtual, "seed {seed}");
                assert!(!s.mute, "seed {seed}");
                for b in &cfg.buses {
                    if let BusKind::Virtual { .. } = b.kind {
                        assert!(!s.route(&b.id).on, "seed {seed}: virtual bus {} is on", b.id);
                    }
                }
                let hw: Vec<&str> = cfg
                    .buses
                    .iter()
                    .filter(|b| matches!(b.kind, BusKind::Hardware { .. }))
                    .map(|b| b.id.as_str())
                    .collect();
                if !hw.is_empty() {
                    assert!(hw.iter().any(|id| s.route(id).on), "seed {seed}: no hardware bus is on");
                }
            }

            // (d) a second call adds nothing and changes nothing
            let snap = cfg.clone();
            assert!(!ensure_channel(&mut cfg, &source), "seed {seed}: second call added again");
            assert_eq!(cfg, snap, "seed {seed}: second call changed cfg");

            // (e) the stems input is never a source
            assert!(!sources(&cfg).iter().any(|(id, _)| id == CHANNEL_ID), "seed {seed}");

            // (f) the mute rule, with and without the replaced source
            let solo: HashSet<String> = cfg.inputs.iter().filter(|_| rnd(&mut x) < 0.3).map(|i| i.id.clone()).collect();
            for inp in &cfg.inputs {
                let none = effective_mute(inp.mute, &inp.id, &solo, None);
                assert_eq!(none, inp.mute || (!solo.is_empty() && !solo.contains(&inp.id)), "seed {seed}: {}", inp.id);
                let with = effective_mute(inp.mute, &inp.id, &solo, Some(source.as_str()));
                if inp.id == source {
                    assert!(with == none || (!none && with), "seed {seed}: replaced source {} changed", inp.id);
                } else {
                    assert_eq!(with, none, "seed {seed}: replaced mute leaked to {}", inp.id);
                }
            }
        }
    }

    #[test]
    fn resolve_falls_back() {
        let cfg = Config::default_config();
        assert_eq!(resolve(&cfg, "gone"), Some("spotify".to_string()));
        assert_ne!(resolve(&cfg, CHANNEL_ID), Some(CHANNEL_ID.to_string()));

        let mut no_spotify = cfg.clone();
        no_spotify.inputs.retain(|i| i.id != "spotify");
        let first = sources(&no_spotify)[0].0.clone();
        assert_eq!(resolve(&no_spotify, "spotify"), Some(first));

        let mut empty = cfg.clone();
        empty.inputs.clear();
        assert_eq!(resolve(&empty, "spotify"), None);
    }

    #[test]
    fn tap_by_kind() {
        let cfg = Config::default_config();
        let spotify = cfg.inputs.iter().find(|i| i.id == "spotify").unwrap();
        assert_eq!(tap(spotify), ("ch_spotify".to_string(), true));
        let mic = cfg.inputs.iter().find(|i| i.id == "mic").unwrap();
        assert_eq!(tap(mic), ("easyeffects_source".to_string(), false));
    }

    #[test]
    fn stemscfg_defaults_and_keys() {
        let d = StemsCfg::default();
        assert_eq!(d.source, "spotify");
        assert_eq!(d.engine, SepEngine::Live);
        assert_eq!(d.plays, Plays::Midi);
        assert_eq!(d.replaced, None);

        let v = serde_json::to_value(&d).unwrap();
        let mut keys: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, vec!["engine", "plays", "replaced", "source"]);

        let g: StemsCfg = serde_json::from_str(r#"{"source":"game"}"#).unwrap();
        assert_eq!(g.source, "game");
        assert_eq!(g.engine, SepEngine::Live);
        assert_eq!(g.plays, Plays::Midi);
        assert_eq!(g.replaced, None);

        let p = std::env::temp_dir().join(format!("wl-stems-bad-{}.json", std::process::id()));
        std::fs::write(&p, "not json {").unwrap();
        assert_eq!(load_from(&p), StemsCfg::default());
        let _ = std::fs::remove_file(&p);
    }
}
