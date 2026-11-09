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
    let auto = rc.bind.trim().is_empty() || rc.bind.trim() == "auto";
    let addrs = settings::bind_addrs(&rc.bind, rc.allow_meshnet, &settings::interfaces());
    let opts = server::Opts {
        bind: addrs.join(","),
        port: rc.port,
        token: rc.token.clone(),
        cfg_path: crate::config::Config::path(),
        name: name.clone(),
        caps: rc.caps.clone(),
    };
    match server::serve(opts, std::sync::Arc::new(backend::PwBackend::default())) {
        Ok(run) => {
            run.token_file(p.clone());
            eprintln!("wavelink remote: listening on {} port {} (token in {})", addrs.join(", "), rc.port, p.display());
            if rc.mdns {
                mdns::advertise(format!("Wavelink Mixer on {name}"), run.addr.port());
            }
            if auto {
                // LAN addresses that appear later (Wi-Fi up after login, a new DHCP lease) get a listener too
                let (meshnet, port) = (rc.allow_meshnet, rc.port);
                let mut have: std::collections::HashSet<String> = addrs.into_iter().collect();
                let _ = std::thread::Builder::new().name("remote-bind".into()).spawn(move || loop {
                    std::thread::sleep(std::time::Duration::from_secs(30));
                    for a in settings::bind_addrs("auto", meshnet, &settings::interfaces()) {
                        if !have.contains(&a) {
                            match run.listen(&a, port) {
                                Ok(_) => eprintln!("wavelink remote: now also listening on {a}:{port}"),
                                Err(e) => eprintln!("wavelink remote: can't listen on {a}:{port}: {e}"),
                            }
                            have.insert(a); // tried once per address; an address that goes away just stays idle
                        }
                    }
                });
            } else {
                std::mem::forget(run); // lives as long as the engine
            }
        }
        Err(e) => eprintln!("wavelink remote: can't listen on {}:{}: {e}", addrs.join(","), rc.port),
    }
}

pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname").map(|s| s.trim().to_string()).ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "wavelink".into())
}

/// `wavelink-mixer remote-token [--rotate]`: print what a client needs to connect (a new token with --rotate).
pub fn print_info(args: &[String]) -> i32 {
    let p = settings::path();
    if args.iter().any(|a| a == "--rotate") {
        match settings::rotate(&p) {
            Ok(_) => println!("new token written to {} (the engine uses it from the next connection; reconnect clients with it)", p.display()),
            Err(e) => {
                eprintln!("{e}");
                return 1;
            }
        }
    }
    match settings::load_or_create(&p) {
        Ok(rc) => {
            // avahi may have renamed the host on a name clash (bazzite-2.local), so show its real name and the LAN IPs
            let mdns = crate::pw::output_timeout("avahi-resolve", &["-a", "127.0.0.1"], std::time::Duration::from_secs(2))
                .and_then(|o| String::from_utf8(o).ok())
                .and_then(|s| s.split_whitespace().nth(1).map(String::from))
                .unwrap_or_else(|| format!("{}.local", hostname()));
            let ifs = settings::interfaces();
            println!("port:  {}", rc.port);
            println!("host:  {mdns}");
            for (dev, ip) in settings::lan_addrs(&ifs, rc.allow_meshnet) {
                println!("ip:    {ip}  ({dev})");
            }
            println!("bind:  {}", settings::bind_addrs(&rc.bind, rc.allow_meshnet, &ifs).join(", "));
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
#[cfg(test)]
#[path = "breaker36.rs"]
mod breaker36;

