// Breaker round 5 tests for PR #45 (head 1344624). Child module of remote::tests so it reuses its rig helpers.
use super::*;

fn usb_dac(c: &mut Config) {
    if let Some(b) = c.buses.iter_mut().find(|b| b.id == "a3") {
        b.kind = BusKind::Hardware { target: "alsa_output.usb-dac".into() };
    }
}

fn set_gain(cfg: &Config, id: &str, db: f64) -> serde_json::Value {
    let mut m = cfg.clone();
    m.buses.iter_mut().find(|b| b.id == id).unwrap().gain_db = db;
    serde_json::to_value(&m).unwrap()
}

fn gain(cfg: &std::path::Path, id: &str) -> Option<f64> {
    Config::peek(cfg).unwrap().buses.iter().find(|b| b.id == id).map(|b| b.gain_db)
}

// R5-1: same target, new bus id, inside the window. Same DAC as a3 (raised to -20 by SetBusGain).
// One gain rule per target: the new bus must not reach 0 dB where a3 could only reach -18 dB.
#[test]
fn r5_new_id_on_same_target_inside_window_is_raise_limited() {
    let r = rig_with("r5new", usb_dac);
    let (mut a, _) = join(&r);
    let _ = cmd(&mut a, 1, Command::SetBusGain { bus: "a3".into(), db: -20.0, unlock: false });
    let base = Config::peek(&r.cfg).unwrap();
    let mut gone = base.clone();
    gone.buses.retain(|b| b.id != "a3");
    send(&mut a, &ClientMsg::SaveConfig { id: 2, base: serde_json::to_value(&base).unwrap(), mine: serde_json::to_value(&gone).unwrap() });
    let _ = ack_for(&mut a, 2);
    let base2 = Config::peek(&r.cfg).unwrap();
    let mut add = base2.clone();
    let mut nb = base.buses.iter().find(|b| b.id == "a3").unwrap().clone();
    nb.id = "a9".into();
    nb.gain_db = 0.0;
    add.buses.push(nb);
    send(&mut a, &ClientMsg::SaveConfig { id: 3, base: serde_json::to_value(&base2).unwrap(), mine: serde_json::to_value(&add).unwrap() });
    let _ = ack_for(&mut a, 3);
    let g = gain(&r.cfg, "a9").expect("a9 saved");
    assert!(g <= -18.0 + 1e-9, "new id on the same DAC re-added at {g} dB, raise limit bypassed");
}

// R5-2: same id, re-added after the 1 s window has passed. Last known gain was -20 dB on this DAC.
// The re-add must not jump to 0 dB; the rule is last level plus the 6 dB step at most (-14).
#[test]
fn r5_readd_after_window_keeps_last_known_gain() {
    let r = rig_with("r5wait", usb_dac);
    let (mut a, _) = join(&r);
    let _ = cmd(&mut a, 1, Command::SetBusGain { bus: "a3".into(), db: -20.0, unlock: false });
    let base = Config::peek(&r.cfg).unwrap();
    let last = base.buses.iter().find(|b| b.id == "a3").unwrap().gain_db;
    let mut gone = base.clone();
    gone.buses.retain(|b| b.id != "a3");
    send(&mut a, &ClientMsg::SaveConfig { id: 2, base: serde_json::to_value(&base).unwrap(), mine: serde_json::to_value(&gone).unwrap() });
    let _ = ack_for(&mut a, 2);
    std::thread::sleep(Duration::from_millis(1200));
    let base2 = Config::peek(&r.cfg).unwrap();
    let mut back = base2.clone();
    let mut nb = base.buses.iter().find(|b| b.id == "a3").unwrap().clone();
    nb.gain_db = 0.0;
    back.buses.push(nb);
    send(&mut a, &ClientMsg::SaveConfig { id: 3, base: serde_json::to_value(&base2).unwrap(), mine: serde_json::to_value(&back).unwrap() });
    let _ = ack_for(&mut a, 3);
    let g = gain(&r.cfg, "a3").expect("a3 saved");
    assert!(g <= last + 6.0 + 1e-9, "re-added after window at {g} dB; last known {last} dB");
}

// R5-3: a brand-new bus on the same DAC as a live bus at -30 dB, asked for +12 dB, saved in one step.
// The raise-step rule (6 dB) that SetBusGain applies to this target must apply to a new bus too.
#[test]
fn r5_brand_new_bus_on_live_target_is_step_limited() {
    let r = rig_with("r5step", usb_dac);
    let (mut a, _) = join(&r);
    let base = Config::peek(&r.cfg).unwrap();
    let mut add = base.clone();
    add.buses.push(Bus { id: "a9".into(), name: "Extra".into(), color: "#ffffff".into(), kind: BusKind::Hardware { target: "alsa_output.usb-dac".into() }, gain_db: 12.0, mute: false, max_db: None });
    send(&mut a, &ClientMsg::SaveConfig { id: 1, base: serde_json::to_value(&base).unwrap(), mine: serde_json::to_value(&add).unwrap() });
    let _ = ack_for(&mut a, 1);
    let g = gain(&r.cfg, "a9").expect("a9 saved");
    assert!(g <= 6.0 + 1e-9, "new bus on a live DAC saved at {g} dB in one step");
}

// Sanity control: the same-id, in-window case the PR pins. Expected to pass.
#[test]
fn r5_control_same_id_in_window() {
    let g = super::readd_case("r5ctl", None);
    assert!(g <= -18.0 + 1e-9, "control {g}");
}
