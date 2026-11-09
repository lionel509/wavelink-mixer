//! LAN control server (#26): the engine serves the mixer state over WebSocket so other devices
//! (the Wavelink Remote app on a MacBook, scripts) can view and control it.
//!
//! - protocol: the `wavelink-proto` crate (versioned serde types, safety caps);
//! - auth: a random token in ~/.config/wavelink-mixer/remote.json (mode 600), required in Hello;
//! - discovery: `_wavelink._tcp` via avahi-publish;
//! - writes: the server is the single writer for remote edits (lock + read-modify-write of config.json),
//!   and applies them to the live graph with wpctl like the GUI does.
pub mod backend;
mod mdns;
pub mod model;
pub mod server;
pub mod settings;

/// The engine's hook: start the server on its own threads if remote.json allows it. Never fails the engine.
pub fn start() {
    let p = settings::path();
    let rc = match settings::load_or_create(&p) {
        Ok(rc) => rc,
        Err(e) => {
            eprintln!("wavelink remote: disabled, {e}");
            return;
        }
    };
    if !rc.enabled {
        eprintln!("wavelink remote: disabled in {}", p.display());
        return;
    }
    let name = hostname();
    let opts = server::Opts {
        bind: rc.bind.clone(),
        port: rc.port,
        token: rc.token.clone(),
        cfg_path: crate::config::Config::path(),
        name: name.clone(),
        caps: rc.caps.clone(),
    };
    match server::serve(opts, std::sync::Arc::new(backend::PwBackend::default())) {
        Ok(run) => {
            eprintln!("wavelink remote: listening on {} (token in {})", run.addr, p.display());
            if rc.mdns {
                mdns::advertise(format!("Wavelink Mixer on {name}"), run.addr.port());
            }
            std::mem::forget(run); // lives as long as the engine
        }
        Err(e) => eprintln!("wavelink remote: can't listen on {}:{}: {e}", rc.bind, rc.port),
    }
}

pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname").map(|s| s.trim().to_string()).ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "wavelink".into())
}

/// `wavelink-mixer remote-token`: print what a client needs to connect.
pub fn print_info() -> i32 {
    match settings::load_or_create(&settings::path()) {
        Ok(rc) => {
            println!("host:  {}.local  port {}", hostname(), rc.port);
            println!("token: {}", rc.token);
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

#[cfg(test)]
mod tests;
