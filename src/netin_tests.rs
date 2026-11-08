use super::*;
use crate::config::{names, Config, InputKind};
use std::io::{Read, Write};
use std::net::TcpListener;

const SEED: u64 = 0x9E3779B97F4A7C15;

fn rng(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

/// A RIFF/WAVE header up to and including the `data` chunk header (no payload).
/// Written here, not with swyh::wav_header, so U3 doesn't depend on U2.
fn wav(rate: u32, ch: u16, bits: u16, extra_chunk: bool) -> Vec<u8> {
    let mut v = b"RIFF".to_vec();
    v.extend(0u32.to_le_bytes());
    v.extend(b"WAVE");
    v.extend(b"fmt ");
    v.extend(16u32.to_le_bytes());
    v.extend(1u16.to_le_bytes());
    v.extend(ch.to_le_bytes());
    v.extend(rate.to_le_bytes());
    let block = ch as u32 * bits as u32 / 8;
    v.extend((rate * block).to_le_bytes());
    v.extend((block as u16).to_le_bytes());
    v.extend(bits.to_le_bytes());
    if extra_chunk {
        v.extend(b"LIST");
        v.extend(4u32.to_le_bytes());
        v.extend(b"INFO");
    }
    v.extend(b"data");
    v.extend(0u32.to_le_bytes());
    v
}

#[test]
fn sniff_wav_header() {
    let f = sniff(Some("audio/wav"), &wav(48000, 2, 16, false)).expect("16-bit WAV");
    assert_eq!(f, Format { rate: 48000, channels: 2, big_endian: false, header_len: 44 });
    let f = sniff(None, &wav(44100, 2, 16, true)).expect("WAV with a LIST chunk");
    assert_eq!(f.header_len, 44 + 12);
    assert!(sniff(None, &wav(48000, 2, 24, false)).is_err(), "24-bit must be refused");
}

#[test]
fn sniff_l16_content_type() {
    let head = [1u8, 2, 3, 4];
    let f = sniff(Some("audio/L16;rate=44100;channels=2"), &head).expect("L16");
    assert_eq!(f, Format { rate: 44100, channels: 2, big_endian: true, header_len: 0 });
    let f = sniff(Some("audio/l16; rate=48000; channels=1"), &head).expect("l16 mono");
    assert_eq!(f, Format { rate: 48000, channels: 1, big_endian: true, header_len: 0 });
}

#[test]
fn sniff_rejects_mp3() {
    let e = sniff(Some("audio/mpeg"), b"ID3\x03\x00").expect_err("MP3 is not supported");
    assert!(e.contains("unsupported"), "{e}");
    assert!(sniff(None, b"\xff\xfb\x90\x00").is_err());
}

#[test]
fn swap_l16_to_le() {
    let mut b = [0x12u8, 0x34, 0xAB, 0xCD];
    to_le(&mut b);
    assert_eq!(b, [0x34, 0x12, 0xCD, 0xAB]);
}

#[test]
fn play_args_pin_the_channel() {
    let f = Format { rate: 48000, channels: 1, big_endian: false, header_len: 44 };
    for id in ["s1", "x_y"] {
        let a = play_args(id, &f);
        let t = a.iter().position(|x| x == "--target").expect("--target");
        assert_eq!(a[t + 1], format!("ch_{id}"));
        let p = a.iter().position(|x| x == "-P").expect("-P");
        assert!(a[p + 1].contains("node.dont-fallback=true"), "{}", a[p + 1]);
        assert!(a[p + 1].contains(&format!("node.name=netin_{id}")), "{}", a[p + 1]);
        assert!(a.windows(2).any(|w| w[0] == "--rate" && w[1] == "48000"), "{a:?}");
        assert!(a.windows(2).any(|w| w[0] == "--channels" && w[1] == "1"), "{a:?}");
    }
}

#[test]
fn wanted_only_virtual_inputs_with_valid_urls() {
    let mut s = SEED;
    let urls = ["http://a.b/x", "ftp://x", "http://a b", "", "http://h/s.wav"];
    for _ in 0..300 {
        let mut cfg = Config::default_config();
        for _ in 0..3 {
            if rng(&mut s) % 2 == 0 {
                cfg.add_stream_input("Tone", "http://10.0.0.1:5959/stream/n1.wav");
            }
        }
        let mut src = cfg.inputs[0].clone();
        src.id = "srcx".into();
        src.kind = InputKind::Source { target: "alsa_input.x".into() };
        cfg.inputs.push(src);
        let mut ids: Vec<String> = cfg.inputs.iter().map(|i| i.id.clone()).collect();
        ids.push("missing".to_string());
        for _ in 0..6 {
            let k = ids[(rng(&mut s) % ids.len() as u64) as usize].clone();
            let u = urls[(rng(&mut s) % urls.len() as u64) as usize].to_string();
            cfg.net_in.insert(k, u);
        }
        let present_names: Vec<String> = cfg
            .inputs
            .iter()
            .map(|i| names::ch(&i.id))
            .filter(|_| rng(&mut s) % 3 != 0)
            .collect();
        let present = |n: &str| present_names.iter().any(|p| p == n);
        for (id, url) in wanted(&cfg, &present) {
            let inp = cfg.inputs.iter().find(|i| i.id == id).expect("wanted id is an input");
            assert!(matches!(inp.kind, InputKind::Virtual), "{id} is not a Virtual input");
            assert!(valid_url(&url), "{id} has an invalid url {url:?}");
            assert_eq!(cfg.net_in.get(&id), Some(&url), "{id} url differs from net_in");
            assert!(present(&names::ch(&id)), "{id} is not present");
        }
    }
}

#[test]
fn valid_url_cases() {
    for ok in ["http://192.168.1.20:5901/stream/swyh.wav", "https://example.com/a", "http://ab"] {
        assert!(valid_url(ok), "{ok}");
    }
    let long = format!("http://{}", "a".repeat(2100));
    for bad in ["ftp://x", "http://a b", "", "http://a", "http://a\tb", "http://a\nb", long.as_str()] {
        assert!(!valid_url(bad), "{bad:?}");
    }
}

#[test]
fn open_reads_local_wav_stream() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let payload: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
    let mut body = wav(44100, 2, 16, false);
    body.extend(&payload);
    std::thread::spawn(move || {
        let (mut c, _) = l.accept().unwrap();
        let mut buf = [0u8; 1024];
        let _ = c.read(&mut buf);
        let head = "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nConnection: close\r\n\r\n";
        c.write_all(head.as_bytes()).unwrap();
        c.write_all(&body).unwrap();
    });
    let (fmt, mut left, mut reader) = open(&format!("http://127.0.0.1:{port}/s.wav")).expect("stream opens");
    assert_eq!((fmt.rate, fmt.channels, fmt.header_len), (44100, 2, 44));
    reader.read_to_end(&mut left).unwrap();
    assert_eq!(left, payload);
}

#[test]
fn open_rejects_404() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut c, _) = l.accept().unwrap();
        let mut buf = [0u8; 1024];
        let _ = c.read(&mut buf);
        let _ = c.write_all(b"HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n");
    });
    assert!(open(&format!("http://127.0.0.1:{port}/missing.wav")).is_err());
}

/// Serves one connection: reads the request, writes `head` then `body`, then closes.
fn serve_once(head: &str, body: Vec<u8>) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let head = head.to_string();
    std::thread::spawn(move || {
        let (mut c, _) = l.accept().unwrap();
        let mut buf = [0u8; 1024];
        let _ = c.read(&mut buf);
        let _ = c.write_all(head.as_bytes());
        let _ = c.write_all(&body);
    });
    port
}

/// A redirect to file:// makes ureq panic. The panic kills the worker thread.
#[test]
fn breaker_redirect_to_file_scheme_is_an_error_not_a_panic() {
    let port = serve_once("HTTP/1.1 302 Found\r\nLocation: file:///etc/hosts\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", vec![]);
    let url = format!("http://127.0.0.1:{port}/r");
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| open(&url).is_err()));
    assert!(matches!(r, Ok(true)), "open({url}) panicked or accepted a file:// redirect: {r:?}");
}

/// A wanted input whose worker thread has ended must get a new worker, not stay silent.
#[test]
fn sync_restarts_a_worker_whose_thread_ended() {
    let mut inn = In::default();
    let url = "http://127.0.0.1:1/x.wav".to_string();
    let old_stop = Arc::new(AtomicBool::new(false));
    let dead = std::thread::spawn(|| {});
    while !dead.is_finished() {
        std::thread::sleep(Duration::from_millis(5));
    }
    inn.workers.insert("n1".into(), (url.clone(), old_stop.clone(), dead));
    inn.sync(HashMap::from([("n1".to_string(), url.clone())]));
    let (_, new_stop, th) = &inn.workers["n1"];
    let running = !th.is_finished();
    new_stop.store(true, Ordering::Relaxed);
    old_stop.store(true, Ordering::Relaxed);
    assert!(running, "worker for n1 ended and was not restarted while n1 is still wanted");
}
