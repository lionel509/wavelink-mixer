//! wavelink-mixer: a Voicemeeter-style mixer for PipeWire.
//!   wavelink-mixer           open the mixer window
//!   wavelink-mixer engine    run the routing engine (systemd user service) + LAN control server
//!   wavelink-mixer remote-token [--rotate]   print the host/port/token a remote client needs (new token with --rotate)
//!   wavelink-mixer remote [ADDR]             the mixer console for another computer (a MacBook), a client of the engine
//!   wavelink-mixer fake-engine [PORT]        a test engine (no PipeWire) for the remote console demo
//!   wavelink-mixer listen [--wav FILE | --mute] [--engine live|quality] [--speed 0..3]   headless listening session (#77)
//!   wavelink-mixer mac-tap [--mute]          Mac system-audio capture child, raw f32 stereo 44.1 kHz on stdout (#77)
//!   wavelink-mixer wav-tap FILE              the same stdout contract fed from a WAV at real-time pace (#77)
mod config;
mod dash;
mod dsp;
mod models;
mod notes;
mod sep;
mod stemcfg;
mod stems;
mod lat;
mod listen;
mod listen_view;
mod mactap;
#[cfg(test)]
#[path = "test_breaker_back.rs"]
mod test_breaker_back;
mod latency;
mod leak;
mod norm;
mod engine;
mod midi;
mod player;
mod spotify;
mod sysmon;
mod pw;
mod remote;
mod busvol;
mod ui;
mod link;
mod macvol;
mod console;
#[cfg(test)]
#[path = "breaker_front.rs"]
mod breaker_front;
#[cfg(test)]
#[path = "breaker_busvol.rs"]
mod breaker_busvol;
#[cfg(test)]
#[path = "demo_midi.rs"]
mod demo_midi;

use adw::prelude::*;
use gtk::glib;

fn main() -> glib::ExitCode {
    // wavelink-mixer preset <input-id> <eq preset|-> <rack preset|->
    if std::env::args().nth(1).as_deref() == Some("preset") {
        let a: Vec<String> = std::env::args().collect();
        let (id, eq, rack) = (a.get(2).cloned().unwrap_or_default(), a.get(3).cloned().unwrap_or("-".into()), a.get(4).cloned().unwrap_or("-".into()));
        // read-modify-write under config.lock: the engine's remote server and the window write too (#26)
        let lock = config::lock_config(&config::Config::path());
        let mut cfg = config::Config::load();
        let Some(inp) = cfg.inputs.iter_mut().find(|i| i.id == id) else {
            eprintln!("no input '{id}'");
            return glib::ExitCode::FAILURE;
        };
        if eq != "-" {
            let Some(p) = config::EQ_PRESETS.iter().find(|p| p.0 == eq) else {
                eprintln!("no EQ preset '{eq}'");
                return glib::ExitCode::FAILURE;
            };
            inp.eq = p.1.to_vec();
        }
        if rack != "-" {
            let Some(p) = config::rack_presets().into_iter().find(|p| p.0 == rack) else {
                eprintln!("no rack preset '{rack}'");
                return glib::ExitCode::FAILURE;
            };
            inp.rack = p.1;
        }
        let (eqv, rk) = (inp.eq.clone(), inp.rack.clone());
        cfg.save();
        drop(lock);
        if let Some(n) = pw::Graph::read().and_then(|g| g.node(&config::names::strip_in(&id)).map(|n| n.id)) {
            pw::set_eq(n, &eqv);
            pw::set_rack(n, &rk);
            std::thread::sleep(std::time::Duration::from_millis(300)); // let pw-cli finish
        }
        println!("{id}: eq={eq} rack={rack}");
        return glib::ExitCode::SUCCESS;
    }
    if std::env::args().nth(1).as_deref() == Some("engine-conf") {
        print!("{}", engine::dump_conf());
        return glib::ExitCode::SUCCESS;
    }
    if std::env::args().nth(1).as_deref() == Some("remote-token") {
        std::process::exit(remote::print_info(&std::env::args().skip(2).collect::<Vec<_>>()));
    }
    if std::env::args().nth(1).as_deref() == Some("engine") {
        engine::run();
        return glib::ExitCode::SUCCESS;
    }
    if std::env::args().nth(1).as_deref() == Some("latency") {
        std::process::exit(latency::run(std::env::args().skip(2).collect()));
    }
    if std::env::args().nth(1).as_deref() == Some("remote") {
        std::process::exit(console::run(std::env::args().skip(2).collect()));
    }
    if std::env::args().nth(1).as_deref() == Some("fake-engine") {
        std::process::exit(remote::fake_engine(&std::env::args().skip(2).collect::<Vec<_>>()));
    }
    if std::env::args().nth(1).as_deref() == Some("mac-tap") {
        std::process::exit(mactap::run(&std::env::args().skip(2).collect::<Vec<_>>()));
    }
    if std::env::args().nth(1).as_deref() == Some("wav-tap") {
        std::process::exit(mactap::wav_run(&std::env::args().skip(2).collect::<Vec<_>>()));
    }
    if std::env::args().nth(1).as_deref() == Some("listen") {
        std::process::exit(listen::run(&std::env::args().skip(2).collect::<Vec<_>>()));
    }
    if std::env::args().nth(1).as_deref() == Some("stems") {
        std::process::exit(stems::cli(std::env::args().skip(2).collect()));
    }
    let app = adw::Application::builder().application_id("local.wavelink.Mixer").build();
    app.connect_activate(ui::activate);
    app.run_with_args::<&str>(&[])
}
