use super::*;

fn usb_dac_r6(c: &mut Config) {
    if let Some(b) = c.buses.iter_mut().find(|b| b.id == "a3") {
        b.kind = BusKind::Hardware { target: "alsa_output.usb-dac".into() };
    }
}

fn gain_r6(cfg: &std::path::Path, id: &str) -> f64 {
    Config::peek(cfg).unwrap().buses.iter().find(|b| b.id == id).unwrap().gain_db
}

// R6-1: a bus added on a live DAC (a3 at -30 dB), then asked for 0 dB again by repeated SaveConfig calls
// inside one 1 s window. One gain rule per target: the DAC may rise at most 12 dB in the window, so no
// save may leave a9 above -18 dB. The 6 dB per-save step alone must not be the only limit.
#[test]
fn r6_repeated_saves_of_new_bus_stay_window_limited() {
    let r = rig_with("r6rep", usb_dac_r6);
    let (mut a, _) = join(&r);
    let _ = cmd(&mut a, 1, Command::SetBusGain { bus: "a3".into(), db: -30.0, unlock: false });
    std::thread::sleep(Duration::from_millis(1200));
    let base = Config::peek(&r.cfg).unwrap();
    let mut add = base.clone();
    let mut nb = base.buses.iter().find(|b| b.id == "a3").unwrap().clone();
    nb.id = "a9".into();
    nb.name = "Extra".into();
    nb.gain_db = 0.0;
    add.buses.push(nb);
    send(&mut a, &ClientMsg::SaveConfig { id: 2, base: serde_json::to_value(&base).unwrap(), mine: serde_json::to_value(&add).unwrap() });
    let _ = ack_for(&mut a, 2);
    let mut seen = vec![gain_r6(&r.cfg, "a9")];
    for id in 3..=5u64 {
        let base_n = Config::peek(&r.cfg).unwrap();
        let mut again = base_n.clone();
        again.buses.iter_mut().find(|b| b.id == "a9").unwrap().gain_db = 0.0;
        send(&mut a, &ClientMsg::SaveConfig { id, base: serde_json::to_value(&base_n).unwrap(), mine: serde_json::to_value(&again).unwrap() });
        let _ = ack_for(&mut a, id);
        seen.push(gain_r6(&r.cfg, "a9"));
    }
    let last = *seen.last().unwrap();
    assert!(last <= -18.0 + 1e-9, "a9 gain per save {seen:?} dB; 4 saves in one window took the DAC past the 12 dB window limit (floor -30, max -18)");
}
