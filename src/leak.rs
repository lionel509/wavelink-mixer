//! Leak watch: the mixer's own memory, files, threads and child processes, read from /proc.
//! Only `scan`, `read` and `orphan_taps` touch /proc; on other OSes they return empty/zero and the Watch stays quiet.

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::Duration;

    /// Deterministic uniform [0, 1) from an LCG (no `rand` crate).
    fn unit(x: &mut u64) -> f64 {
        *x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (*x >> 11) as f64 / (1u64 << 53) as f64
    }

    fn base_stats(rss_kb: u64, fds: u64, threads: u64) -> ProcStats {
        ProcStats { rss_kb, fds, threads, zombies: 0, taps: 0, unlinked: 0 }
    }

    #[test]
    fn parse_stat_handles_odd_comm() {
        assert_eq!(
            parse_stat("1234 (Web (x) y) Z 99 1 2"),
            Some(Proc { pid: 1234, ppid: 99, state: 'Z', comm: "Web (x) y".into() })
        );
        assert!(parse_stat("garbage").is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn counts_zombie_child() {
        let me = std::process::id();
        let mut c = Command::new("true").spawn().unwrap();
        let pid = c.id();
        std::thread::sleep(Duration::from_millis(300));
        let procs = scan();
        assert!(procs.iter().any(|p| p.pid == pid && p.state == 'Z' && p.ppid == me), "no zombie {pid}");
        assert!(zombies(&procs, me) >= 1);
        c.wait().unwrap();
        assert!(!scan().iter().any(|p| p.pid == pid), "{pid} still listed after wait");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn counts_named_children() {
        let me = std::process::id();
        let mut kids: Vec<_> = (0..3).map(|_| Command::new("sleep").arg("5").spawn().unwrap()).collect();
        let procs = scan();
        assert!(children_named(&procs, me, "sleep") >= 3);
        for k in &kids {
            assert!(procs.iter().any(|p| p.pid == k.id()), "child {} missing", k.id());
        }
        for k in kids.iter_mut() {
            let _ = k.kill();
            let _ = k.wait();
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn read_self() {
        let me = std::process::id();
        let s = read(me, &scan());
        assert!(s.rss_kb > 0 && s.fds >= 3 && s.threads >= 1, "{s:?}");
    }

    #[test]
    fn watch_rules() {
        let base = ProcStats { rss_kb: 200 * 1024, fds: 50, threads: 20, zombies: 0, taps: 0, unlinked: 0 };
        let big = ProcStats { rss_kb: base.rss_kb + 500 * 1024, ..base };
        // before warm-up nothing is judged, however large the growth
        let mut w = Watch::new("t");
        assert!(w.update(&big, None, 0.0).is_empty());
        // the baseline is set at uptime 60
        let fresh = || {
            let mut w = Watch::new("t");
            assert!(w.update(&base, None, 60.0).is_empty());
            w
        };
        let rss = |mb: u64| ProcStats { rss_kb: base.rss_kb + mb * 1024, ..base };
        assert!(!fresh().update(&rss(70), None, 61.0).is_empty(), "+70 MB should warn");
        assert!(fresh().update(&rss(40), None, 61.0).is_empty(), "+40 MB should not warn");
        let fds = |n: u64| ProcStats { fds: base.fds + n, ..base };
        assert!(!fresh().update(&fds(33), None, 61.0).is_empty(), "fds +33 should warn");
        assert!(fresh().update(&fds(32), None, 61.0).is_empty(), "fds +32 should not warn");
        let th = |n: u64| ProcStats { threads: base.threads + n, ..base };
        assert!(!fresh().update(&th(17), None, 61.0).is_empty(), "threads +17 should warn");
        assert!(fresh().update(&th(16), None, 61.0).is_empty(), "threads +16 should not warn");
        // zombies: one reading is noise, two in a row warn, zero clears
        let zb = ProcStats { zombies: 1, ..base };
        let mut w = fresh();
        assert!(w.update(&zb, None, 61.0).is_empty());
        assert!(w.update(&zb, None, 62.0).iter().any(|s| s.contains("zombie")));
        assert!(w.update(&base, None, 63.0).is_empty());
        // taps: 17 of 18 once is noise, twice warns; no expectation means never warn
        let t17 = ProcStats { taps: 17, ..base };
        let mut w = fresh();
        assert!(w.update(&t17, Some(18), 61.0).is_empty());
        assert!(w.update(&t17, Some(18), 62.0).iter().any(|s| s.contains("expected 18")));
        let mut w = fresh();
        assert!(w.update(&t17, None, 61.0).is_empty());
        assert!(w.update(&t17, None, 62.0).is_empty());
        // unlinked meters: once is noise, twice warns, zero clears
        let ul = ProcStats { unlinked: 1, ..base };
        let mut w = fresh();
        assert!(w.update(&ul, None, 61.0).is_empty());
        assert!(w.update(&ul, None, 62.0).iter().any(|s| s.contains("not linked")));
        assert!(w.update(&base, None, 63.0).is_empty());
    }

    #[test]
    fn watch_quiet_on_steady_state() {
        let mut x: u64 = 3;
        for case in 0..200 {
            let rss0 = 50 * 1024 + (unit(&mut x) * 550.0 * 1024.0) as u64;
            let fds0 = 5 + (unit(&mut x) * 195.0) as u64;
            let th0 = 2 + (unit(&mut x) * 58.0) as u64;
            let expected = (unit(&mut x) * 20.0) as u64;
            let mut w = Watch::new("q");
            for k in 0..300u64 {
                let rss = (rss0 as f64 + (unit(&mut x) * 2.0 - 1.0) * 16.0 * 1024.0) as u64;
                let fds = (fds0 as f64 + (unit(&mut x) * 2.0 - 1.0) * 8.0).max(0.0) as u64;
                let threads = (th0 as f64 + (unit(&mut x) * 2.0 - 1.0) * 4.0).max(0.0) as u64;
                let s = ProcStats { rss_kb: rss, fds, threads, zombies: 0, taps: expected, unlinked: 0 };
                let warns = w.update(&s, Some(expected), k as f64 * 10.0);
                assert!(warns.is_empty(), "case {case} step {k}: {warns:?}");
            }
        }
    }

    #[test]
    fn watch_flags_growth() {
        let mut x: u64 = 5;
        for case in 0..200 {
            let rss0 = 50 * 1024 + (unit(&mut x) * 550.0 * 1024.0) as u64;
            let fds0 = 5 + (unit(&mut x) * 195.0) as u64;
            let th0 = 2 + (unit(&mut x) * 58.0) as u64;
            let base = ProcStats { rss_kb: rss0, fds: fds0, threads: th0, zombies: 0, taps: 0, unlinked: 0 };
            // memory: +1 MB per update, first warning exactly where the rule first holds
            let lim = rss0 + RSS_GROW_KB.max(rss0 / 4);
            let want = (1..=400u64).find(|k| rss0 + k * 1024 > lim);
            let mut w = Watch::new("g");
            w.update(&base, None, WARMUP_S);
            let mut got = None;
            for k in 1..=400u64 {
                let s = ProcStats { rss_kb: rss0 + k * 1024, ..base };
                if w.update(&s, None, WARMUP_S + k as f64 * 10.0).iter().any(|m| m.contains("memory")) {
                    got = Some(k);
                    break;
                }
            }
            assert_eq!(got, want, "case {case}: memory rule");
            // open files: +1 per update
            let mut w = Watch::new("g");
            w.update(&base, None, WARMUP_S);
            let mut got = None;
            for k in 1..=100u64 {
                let s = ProcStats { fds: fds0 + k, ..base };
                if w.update(&s, None, WARMUP_S + k as f64 * 10.0).iter().any(|m| m.contains("open files")) {
                    got = Some(k);
                    break;
                }
            }
            assert_eq!(got, (1..=100u64).find(|k| *k > FD_GROW), "case {case}: fd rule");
            // threads: +1 per update
            let mut w = Watch::new("g");
            w.update(&base, None, WARMUP_S);
            let mut got = None;
            for k in 1..=100u64 {
                let s = ProcStats { threads: th0 + k, ..base };
                if w.update(&s, None, WARMUP_S + k as f64 * 10.0).iter().any(|m| m.contains("threads")) {
                    got = Some(k);
                    break;
                }
            }
            assert_eq!(got, (1..=100u64).find(|k| *k > THREAD_GROW), "case {case}: thread rule");
        }
    }

    #[test]
    fn meter_tap_cmdline() {
        assert!(is_meter_tap_cmdline(
            b"pw-record\0-P\0{ node.name=meter_spotify node.passive=true media.class=Stream/Input/Audio node.autoconnect=false }\0--format\0f32\0"
        ));
        assert!(is_meter_tap_cmdline(b"pw-record\0-P\0{ node.name=spectrum }\0"));
        assert!(!is_meter_tap_cmdline(b"pw-record\0--target\0x\0-\0"));
        assert!(!is_meter_tap_cmdline(b""));
    }
}
