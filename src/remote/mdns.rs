//! DNS-SD advertisement via `avahi-publish` (the system's avahi-daemon already owns port 5353).
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Keep `avahi-publish -s <name> _wavelink._tcp <port>` running on a dedicated thread, restarting it
/// with backoff if it exits (avahi-daemon restarted, network change). Missing avahi-publish: logs once.
pub fn advertise(name: String, port: u16) {
    let _ = std::thread::Builder::new().name("remote-mdns".into()).spawn(move || {
        let mut backoff = Duration::from_secs(2);
        loop {
            let started = Instant::now();
            let mut cmd = Command::new("avahi-publish");
            // the child dies with this (long-lived) thread's process
            crate::pw::die_with_parent(&mut cmd);
            let child = cmd
                .args(["-s", &name, wavelink_proto::MDNS_SERVICE, &port.to_string(), &format!("proto={}", wavelink_proto::PROTOCOL_VERSION)])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            match child {
                Ok(mut c) => {
                    eprintln!("wavelink remote: advertising '{name}' as {} on port {port}", wavelink_proto::MDNS_SERVICE);
                    let _ = c.wait();
                }
                Err(e) => {
                    eprintln!("wavelink remote: avahi-publish unavailable ({e}); clients must use host:port");
                    return;
                }
            }
            if started.elapsed() > Duration::from_secs(60) {
                backoff = Duration::from_secs(2);
            }
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(Duration::from_secs(60));
        }
    });
}
