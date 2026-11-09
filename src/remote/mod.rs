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
            // avahi may have renamed the host on a name clash (bazzite-2.local), so show its real name and the LAN IPs
            let mdns = crate::pw::output_timeout("avahi-resolve", &["-a", "127.0.0.1"], std::time::Duration::from_secs(2))
                .and_then(|o| String::from_utf8(o).ok())
                .and_then(|s| s.split_whitespace().nth(1).map(String::from))
                .unwrap_or_else(|| format!("{}.local", hostname()));
            let ips = crate::pw::output_timeout("ip", &["-4", "-o", "addr", "show", "scope", "global"], std::time::Duration::from_secs(2))
                .and_then(|o| String::from_utf8(o).ok())
                .map(|s| lan_ips(&s))
                .unwrap_or_default();
            println!("port:  {}", rc.port);
            println!("host:  {mdns}");
            for (dev, ip) in ips {
                println!("ip:    {ip}  ({dev})");
            }
            println!("token: {}", rc.token);
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

/// (interface, IPv4) from `ip -4 -o addr`, skipping container bridges.
fn lan_ips(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let dev = f.get(1)?.to_string();
            let ip = f.iter().position(|x| *x == "inet").and_then(|i| f.get(i + 1))?.split('/').next()?.to_string();
            (!["podman", "docker", "veth", "virbr", "br-"].iter().any(|p| dev.starts_with(p))).then_some((dev, ip))
        })
        .collect()
}

#[cfg(test)]
mod tests;
#[cfg(test)]
#[path = "breaker36.rs"]
mod breaker36;

#[cfg(test)]
#[test]
fn lan_ips_skip_bridges() {
    let out = "2: enp14s0    inet 192.168.1.231/24 brd 192.168.1.255 scope global dynamic\n3: podman0    inet 10.88.0.1/16 scope global\n4: wlp15s0    inet 192.168.1.157/24 scope global\n";
    assert_eq!(lan_ips(out), vec![("enp14s0".into(), "192.168.1.231".into()), ("wlp15s0".into(), "192.168.1.157".into())]);
}
