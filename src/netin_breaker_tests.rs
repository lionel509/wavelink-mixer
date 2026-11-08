// Breaker round 1 (input, #15). Each test here is written to FAIL on the PR head.

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

/// A valid 16-bit stereo 44.1 kHz WAV header whose LIST chunk is `list_len` bytes long.
fn wav_with_list(list_len: usize) -> Vec<u8> {
    let mut v = b"RIFF".to_vec();
    v.extend(0u32.to_le_bytes());
    v.extend(b"WAVE");
    v.extend(b"fmt ");
    v.extend(16u32.to_le_bytes());
    v.extend(1u16.to_le_bytes());
    v.extend(2u16.to_le_bytes());
    v.extend(44100u32.to_le_bytes());
    v.extend((44100u32 * 4).to_le_bytes());
    v.extend(4u16.to_le_bytes());
    v.extend(16u16.to_le_bytes());
    v.extend(b"LIST");
    v.extend((list_len as u32).to_le_bytes());
    v.extend(vec![b'x'; list_len]);
    v.extend(b"data");
    v.extend(0u32.to_le_bytes());
    v
}

/// A reader that blocks for 3 s on every read, like a stalled HTTP body.
struct Stall;
impl Read for Stall {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        std::thread::sleep(Duration::from_secs(3));
        buf.fill(0);
        Ok(buf.len())
    }
}

/// A reader that returns `data` and then EOF.
struct Short {
    data: Vec<u8>,
    pos: usize,
}
impl Read for Short {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = buf.len().min(self.data.len() - self.pos);
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

fn fmt_stereo() -> Format {
    Format { rate: 44100, channels: 2, big_endian: false, header_len: 0 }
}

// BREAK 1: a valid WAV whose header is longer than the 256-byte read window is refused.
#[test]
fn breaker_wav_with_300_byte_metadata_chunk_opens() {
    let hdr = wav_with_list(300);
    let mut body = hdr.clone();
    body.extend(vec![7u8; 1000]);
    let port = serve_once("HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nConnection: close\r\n\r\n", body);
    let (fmt, left, mut reader) =
        open(&format!("http://127.0.0.1:{port}/s.wav")).expect("a valid WAV with a 300-byte LIST chunk must open");
    assert_eq!((fmt.rate, fmt.channels, fmt.header_len), (44100, 2, hdr.len()));
    let mut rest = left;
    reader.read_to_end(&mut rest).unwrap();
    assert_eq!(rest, vec![7u8; 1000]);
}

// BREAK 2: stop is only checked between reads, so a stalled stream keeps its pw-play alive.
#[test]
fn breaker_stop_is_honoured_while_stream_stalls() {
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = stop.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        s2.store(true, Ordering::Relaxed);
    });
    let mut out: Vec<u8> = Vec::new();
    let mut r = Stall;
    let t = Instant::now();
    let _ = pump(&mut out, &mut r, &[], &fmt_stereo(), &stop);
    let took = t.elapsed();
    assert!(took < Duration::from_secs(1), "pump ran {took:?} after stop was set");
}

// BREAK 3: a zero-byte L16 body is accepted as playable, so pw-play is spawned and respawned.
#[test]
fn breaker_empty_l16_stream_is_refused() {
    let port = serve_once("HTTP/1.1 200 OK\r\nContent-Type: audio/L16;rate=44100;channels=2\r\nConnection: close\r\n\r\n", vec![]);
    let ok = open(&format!("http://127.0.0.1:{port}/s.raw")).is_ok();
    assert!(!ok, "an empty L16 stream opened as playable");
}

// BREAK 4: the -P props string is built by string interpolation, so an id with a brace escapes the pin.
#[test]
fn breaker_play_args_props_cannot_escape_the_pin() {
    let a = play_args("x}y", &fmt_stereo());
    let p = a.iter().position(|x| x == "-P").expect("-P");
    let props = &a[p + 1];
    let close = props.find('}').expect("closing brace");
    assert!(props[..close].contains("node.dont-fallback=true"), "dont-fallback is outside the pinned object: {props}");
}

// BREAK 5: URL schemes are case-sensitive, so a browser-style uppercase URL is refused.
#[test]
fn breaker_valid_url_accepts_uppercase_scheme() {
    assert!(valid_url("HTTP://127.0.0.1:5959/stream/n1.wav"));
}

// BREAK 6: the last partial chunk of a stream that ends mid-read is dropped.
#[test]
fn breaker_pump_writes_final_partial_chunk() {
    let data: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
    let mut r = Short { data: data.clone(), pos: 0 };
    let mut out: Vec<u8> = Vec::new();
    let stop = AtomicBool::new(false);
    let res = pump(&mut out, &mut r, &[], &fmt_stereo(), &stop);
    assert_eq!(out.len(), data.len(), "pump returned {res:?}: wrote {} of {} bytes", out.len(), data.len());
}



// BREAK 7: a redirect to file:// panics inside ureq. The panic kills the worker thread, and
// In::tick keeps a dead worker for an unchanged URL, so the input never reconnects.
#[test]
fn breaker_redirect_to_file_scheme_is_an_error_not_a_panic() {
    let port = serve_once("HTTP/1.1 302 Found
Location: file:///etc/hosts
Content-Length: 0
Connection: close

", vec![]);
    let url = format!("http://127.0.0.1:{port}/r");
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| open(&url).is_err()));
    assert!(matches!(r, Ok(true)), "open({url}) panicked or accepted a file:// redirect: {r:?}");
}

