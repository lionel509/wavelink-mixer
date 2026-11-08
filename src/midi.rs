//! Novation SL MkIII as a control surface (default template on its main MIDI port):
//! faders CC 41-48, knobs CC 21-28, buttons under faders CC 51-58, second row CC 71-78.
//! Reads via `aseqdump` and reconnects when the keyboard is unplugged/replugged.
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::time::Duration;

pub const PORT: &str = "Novation SL MkIII:0";

pub enum Midi {
    Connected(bool),
    Cc(u8, u8), // controller, value
}

pub fn spawn(tx: Sender<Midi>) {
    if crate::dash::no_audio() {
        return; // test runs never touch real devices
    }
    std::thread::spawn(move || loop {
        let mut cmd = Command::new("stdbuf");
        crate::pw::die_with_parent(&mut cmd);
        let child = cmd
            .args(["-oL", "aseqdump", "-p", PORT])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let Ok(mut child) = child else {
            std::thread::sleep(Duration::from_secs(3));
            continue;
        };
        let out = child.stdout.take().unwrap();
        let mut announced = false;
        for line in BufReader::new(out).lines() {
            let Ok(line) = line else { break };
            if line.contains("Waiting for data") {
                if tx.send(Midi::Connected(true)).is_err() {
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                }
                announced = true;
                continue;
            }
            // "  36:0   Control change          0, controller 41, value 79"
            if let Some(rest) = line.split("controller ").nth(1) {
                let mut it = rest.split(", value ");
                if let (Some(c), Some(v)) = (it.next(), it.next()) {
                    if let (Ok(c), Ok(v)) = (c.trim().parse::<u8>(), v.trim().parse::<u8>()) {
                        if tx.send(Midi::Cc(c, v)).is_err() {
                            let _ = child.kill();
                            return;
                        }
                    }
                }
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        if announced && tx.send(Midi::Connected(false)).is_err() {
            return;
        }
        std::thread::sleep(Duration::from_secs(3));
    });
}
