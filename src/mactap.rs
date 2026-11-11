//! Mac capture for #77: the system output through one Core Audio process tap, as a child with the pw-cat stdout
//! contract (raw interleaved f32 LE, 44100 Hz, 2 channels, no header). The tap and its private aggregate device are
//! not visible in Sound settings or Audio MIDI Setup; the only permission is System Audio Recording.
#![allow(dead_code)]
use std::io::Write;
use std::mem::ManuallyDrop;
use std::os::unix::io::FromRawFd;
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
use std::ffi::c_void;
#[cfg(target_os = "macos")]
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender};

/// The stdout contract: every capture child writes this rate.
pub const RATE_OUT: u32 = 44100;

/// Linear resampler to 44100 Hz, stereo in and out. Output does not depend on how the input is split into blocks.
pub struct Resampler {
    step: f64,
    pos: f64,
    prev: [f32; 2],
    primed: bool,
}

impl Resampler {
    pub fn new(in_rate: f64) -> Self {
        Resampler { step: in_rate / RATE_OUT as f64, pos: 0.0, prev: [0.0; 2], primed: false }
    }

    /// `inp` is interleaved stereo at the input rate; the output is appended to `out`, interleaved stereo at 44100.
    pub fn push(&mut self, inp: &[f32], out: &mut Vec<f32>) {
        if self.step == 1.0 {
            out.extend_from_slice(inp);
            return;
        }
        for f in inp.chunks_exact(2) {
            let cur = [f[0], f[1]];
            if !self.primed {
                self.prev = cur;
                self.primed = true;
                continue;
            }
            while self.pos < 1.0 {
                let t = self.pos as f32;
                for c in 0..2 {
                    out.push(self.prev[c] + (cur[c] - self.prev[c]) * t);
                }
                self.pos += self.step;
            }
            self.pos -= 1.0;
            self.prev = cur;
        }
    }
}

/// Reads a RIFF/WAVE file: PCM 16-bit or IEEE float 32-bit, mono or stereo. Returns (rate, interleaved stereo).
pub fn wav_pcm(bytes: &[u8]) -> Result<(u32, Vec<f32>), String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".into());
    }
    let mut fmt: Option<(u16, u16, u32, u16)> = None;
    let mut data: Option<&[u8]> = None;
    let mut at = 12usize;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let size = u32::from_le_bytes([bytes[at + 4], bytes[at + 5], bytes[at + 6], bytes[at + 7]]) as usize;
        let start = at + 8;
        let end = start.saturating_add(size).min(bytes.len());
        let body = &bytes[start..end];
        if id == b"fmt " {
            if body.len() < 16 {
                return Err("short fmt chunk".into());
            }
            let tag = u16::from_le_bytes([body[0], body[1]]);
            let ch = u16::from_le_bytes([body[2], body[3]]);
            let rate = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
            let bits = u16::from_le_bytes([body[14], body[15]]);
            fmt = Some((tag, ch, rate, bits));
        } else if id == b"data" {
            data = Some(body);
        }
        at = start.saturating_add(size).saturating_add(size & 1);
    }
    let (tag, ch, rate, bits) = fmt.ok_or("no fmt chunk")?;
    let data = data.ok_or("no data chunk")?;
    if !matches!((tag, bits), (1, 16) | (3, 32)) {
        return Err(format!("unsupported WAV: format {tag}, {bits} bits"));
    }
    if ch != 1 && ch != 2 {
        return Err(format!("unsupported WAV: {ch} channels"));
    }
    let samples: Vec<f32> = if bits == 16 {
        data.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0).collect()
    } else {
        data.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect()
    };
    let stereo = if ch == 1 { samples.iter().flat_map(|&x| [x, x]).collect() } else { samples };
    Ok((rate, stereo))
}

/// Shapes the buffers of one capture callback into interleaved stereo. `channels` is the channel count of each
/// buffer; one buffer is taken as interleaved, two or more mono buffers are interleaved as left and right.
pub fn to_stereo(bufs: &[&[f32]], channels: usize) -> Vec<f32> {
    match bufs {
        [] => Vec::new(),
        [one] if channels <= 1 => one.iter().flat_map(|&x| [x, x]).collect(),
        [one] if channels == 2 => one.to_vec(),
        [one] => one.chunks_exact(channels).flat_map(|f| [f[0], f[1]]).collect(),
        [l, r, ..] => l.iter().zip(r.iter()).flat_map(|(&a, &b)| [a, b]).collect(),
    }
}

/// Writes `pcm` (interleaved stereo at 44100) in 10 ms slices, then silence, until a write fails. Returns the number
/// of slices written. With `pace`, slice k is due at k * 10 ms from the start.
pub fn feed(pcm: &[f32], out: &mut impl Write, pace: bool) -> usize {
    const SLICE: usize = 441 * 2;
    let t0 = Instant::now();
    let mut bytes = Vec::with_capacity(SLICE * 4);
    let mut k = 0usize;
    loop {
        if pace {
            let due = t0 + Duration::from_millis(10 * k as u64);
            let now = Instant::now();
            if due > now {
                std::thread::sleep(due - now);
            }
        }
        bytes.clear();
        let start = k * SLICE;
        for i in 0..SLICE {
            let v = pcm.get(start + i).copied().unwrap_or(0.0);
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        if out.write_all(&bytes).is_err() {
            return k;
        }
        k += 1;
    }
}

/// `wav-tap FILE`: the pw-cat contract fed from a WAV at real-time pace. Exit code.
pub fn wav_run(args: &[String]) -> i32 {
    let Some(path) = args.first() else {
        eprintln!("wav-tap: needs a WAV file");
        return 1;
    };
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("wav-tap: {path}: {e}");
            return 1;
        }
    };
    let (rate, mut pcm) = match wav_pcm(&bytes) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("wav-tap: {path}: {e}");
            return 1;
        }
    };
    if rate != RATE_OUT {
        let mut out = Vec::new();
        Resampler::new(rate as f64).push(&pcm, &mut out);
        pcm = out;
    }
    // fd 1 belongs to the parent; never close it here.
    let mut stdout = ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(1) });
    feed(&pcm, &mut *stdout, true);
    0
}

/// `mac-tap [--mute]`: the Core Audio process tap. Exit code. Not macOS: prints a note and returns 2.
#[cfg(not(target_os = "macos"))]
pub fn run(args: &[String]) -> i32 {
    let _ = args;
    eprintln!("mac-tap: macOS only");
    2
}

/// `mac-tap [--mute]`: the Core Audio process tap. Exit code.
#[cfg(target_os = "macos")]
pub fn run(args: &[String]) -> i32 {
    let mute = args.iter().any(|a| a == "--mute");
    let mut tap = match open(mute) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("mac-tap: {e}");
            return 1;
        }
    };
    let (tx, rx) = sync_channel::<Vec<f32>>(64);
    if let Err(e) = tap.start(tx) {
        eprintln!("mac-tap: {e}");
        return 1;
    }
    let mut rs = Resampler::new(tap.rate);
    // fd 1 belongs to the parent; never close it here.
    let mut stdout = ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(1) });
    let mut pcm = Vec::new();
    let mut bytes = Vec::new();
    loop {
        // Parent gone (reparented to launchd): stop, and the Tap drop tears down the tap.
        if unsafe { ffi::getppid() } == 1 {
            return 0;
        }
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(chunk) => {
                pcm.clear();
                rs.push(&chunk, &mut pcm);
                bytes.clear();
                for v in &pcm {
                    bytes.extend_from_slice(&v.to_le_bytes());
                }
                if stdout.write_all(&bytes).is_err() {
                    return 0;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                eprintln!("mac-tap: capture stopped");
                return 1;
            }
        }
    }
}

#[cfg(target_os = "macos")]
pub mod ffi {
    use std::ffi::{c_char, c_void, CStr};

    /// AudioStreamBasicDescription: 40 bytes.
    #[repr(C)]
    pub struct Asbd {
        pub sample_rate: f64,
        pub format_id: u32,
        pub format_flags: u32,
        pub bytes_per_packet: u32,
        pub frames_per_packet: u32,
        pub bytes_per_frame: u32,
        pub channels_per_frame: u32,
        pub bits_per_channel: u32,
        pub reserved: u32,
    }

    /// AudioBuffer: 16 bytes, the data pointer at offset 8.
    #[repr(C)]
    pub struct AudioBuffer {
        pub channels: u32,
        pub byte_size: u32,
        pub data: *mut std::ffi::c_void,
    }

    /// AudioObjectPropertyAddress.
    #[repr(C)]
    pub struct Addr {
        pub selector: u32,
        pub scope: u32,
        pub element: u32,
    }

    pub type Id = *mut c_void;
    pub type Sel = *mut c_void;
    pub type IOProc = unsafe extern "C" fn(
        u32,
        *const c_void,
        *const c_void,
        *const c_void,
        *mut c_void,
        *const c_void,
        *mut c_void,
    ) -> i32;

    /// A four-character code, as the CoreAudio headers spell them.
    pub const fn fourcc(s: &[u8; 4]) -> u32 {
        u32::from_be_bytes(*s)
    }

    #[link(name = "objc")]
    unsafe extern "C" {
        fn objc_getClass(name: *const c_char) -> Id;
        fn sel_registerName(name: *const c_char) -> Sel;
        fn class_getInstanceMethod(cls: Id, sel: Sel) -> *mut c_void;
        fn objc_msgSend();
    }

    #[link(name = "Foundation", kind = "framework")]
    unsafe extern "C" {}

    #[link(name = "CoreAudio", kind = "framework")]
    unsafe extern "C" {
        pub fn AudioHardwareCreateProcessTap(desc: Id, out: *mut u32) -> i32;
        pub fn AudioHardwareDestroyProcessTap(tap: u32) -> i32;
        pub fn AudioHardwareCreateAggregateDevice(desc: *const c_void, out: *mut u32) -> i32;
        pub fn AudioHardwareDestroyAggregateDevice(dev: u32) -> i32;
        pub fn AudioObjectGetPropertyData(
            id: u32,
            addr: *const Addr,
            qsize: u32,
            qual: *const c_void,
            size: *mut u32,
            data: *mut c_void,
        ) -> i32;
        pub fn AudioObjectSetPropertyData(
            id: u32,
            addr: *const Addr,
            qsize: u32,
            qual: *const c_void,
            size: u32,
            data: *const c_void,
        ) -> i32;
        pub fn AudioDeviceCreateIOProcID(dev: u32, proc: IOProc, client: *mut c_void, out: *mut *mut c_void) -> i32;
        pub fn AudioDeviceDestroyIOProcID(dev: u32, proc: *mut c_void) -> i32;
        pub fn AudioDeviceStart(dev: u32, proc: *mut c_void) -> i32;
        pub fn AudioDeviceStop(dev: u32, proc: *mut c_void) -> i32;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        pub static kCFTypeArrayCallBacks: u8;
        pub static kCFTypeDictionaryKeyCallBacks: u8;
        pub static kCFTypeDictionaryValueCallBacks: u8;
        pub static kCFBooleanTrue: *const c_void;
        pub static kCFBooleanFalse: *const c_void;
        pub fn CFStringCreateWithCString(alloc: *const c_void, s: *const c_char, enc: u32) -> *const c_void;
        pub fn CFArrayCreate(alloc: *const c_void, vals: *const *const c_void, n: isize, cb: *const c_void) -> *const c_void;
        pub fn CFDictionaryCreate(
            alloc: *const c_void,
            keys: *const *const c_void,
            vals: *const *const c_void,
            n: isize,
            kcb: *const c_void,
            vcb: *const c_void,
        ) -> *const c_void;
        pub fn CFRelease(p: *const c_void);
    }

    unsafe extern "C" {
        pub fn getppid() -> i32;
    }

    /// Registers (or looks up) a selector.
    pub unsafe fn sel(name: &CStr) -> Sel {
        unsafe { sel_registerName(name.as_ptr()) }
    }

    /// The class named `name`, or null when the framework is not loaded.
    pub unsafe fn class(name: &CStr) -> Id {
        unsafe { objc_getClass(name.as_ptr()) }
    }

    /// Whether CATapDescription exists and responds to the three selectors `open` sends.
    pub fn tap_selectors_present() -> bool {
        unsafe {
            let cls = class(c"CATapDescription");
            if cls.is_null() {
                return false;
            }
            [c"initStereoGlobalTapButExcludeProcesses:", c"setPrivate:", c"setMuteBehavior:"]
                .iter()
                .all(|s| !class_getInstanceMethod(cls, sel(s)).is_null())
        }
    }

    // objc_msgSend, cast per call site to the exact signature (arm64 has no variadic message send).
    fn msg_ptr() -> *const c_void {
        objc_msgSend as unsafe extern "C" fn() as *const c_void
    }

    pub unsafe fn send0(o: Id, s: Sel) -> Id {
        let f: unsafe extern "C" fn(Id, Sel) -> Id = unsafe { std::mem::transmute(msg_ptr()) };
        unsafe { f(o, s) }
    }

    pub unsafe fn send1(o: Id, s: Sel, a: Id) -> Id {
        let f: unsafe extern "C" fn(Id, Sel, Id) -> Id = unsafe { std::mem::transmute(msg_ptr()) };
        unsafe { f(o, s, a) }
    }

    pub unsafe fn send_bool(o: Id, s: Sel, v: u8) {
        let f: unsafe extern "C" fn(Id, Sel, u8) = unsafe { std::mem::transmute(msg_ptr()) };
        unsafe { f(o, s, v) }
    }

    pub unsafe fn send_isize(o: Id, s: Sel, v: isize) {
        let f: unsafe extern "C" fn(Id, Sel, isize) = unsafe { std::mem::transmute(msg_ptr()) };
        unsafe { f(o, s, v) }
    }

    /// A new CFString (caller releases it).
    pub unsafe fn cfstr(s: &str) -> *const c_void {
        let c = std::ffi::CString::new(s).expect("no NUL in a tap name");
        unsafe { CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), 0x0800_0100) }
    }
}

/// A live Core Audio tap with its private aggregate device. Dropping it stops and destroys both.
#[cfg(target_os = "macos")]
pub struct Tap {
    pub rate: f64,
    tap: u32,
    dev: u32,
    proc_id: *mut c_void,
    client: *mut c_void,
}

#[cfg(target_os = "macos")]
fn check(what: &str, st: i32) -> Result<(), String> {
    if st == 0 {
        Ok(())
    } else {
        Err(format!("{what} failed: OSStatus {st}"))
    }
}

#[cfg(target_os = "macos")]
fn addr(selector: &[u8; 4]) -> ffi::Addr {
    ffi::Addr { selector: ffi::fourcc(selector), scope: ffi::fourcc(b"glob"), element: 0 }
}

/// Creates the process tap (system output, no processes excluded) and its private aggregate device. With `mute`,
/// the speakers are silent while the tap runs. Nothing is started; `Tap::start` starts the IO.
#[cfg(target_os = "macos")]
pub fn open(mute: bool) -> Result<Tap, String> {
    use ffi::*;
    use std::ptr::{null, null_mut};
    let mut t = Tap { rate: 0.0, tap: 0, dev: 0, proc_id: null_mut(), client: null_mut() };
    unsafe {
        let cls = class(c"CATapDescription");
        if cls.is_null() {
            return Err("CATapDescription is missing: needs macOS 14.2 or later".into());
        }
        let arr = send0(class(c"NSArray"), sel(c"array"));
        let desc = send1(send0(cls, sel(c"alloc")), sel(c"initStereoGlobalTapButExcludeProcesses:"), arr);
        if desc.is_null() {
            return Err("CATapDescription init failed".into());
        }
        send_bool(desc, sel(c"setPrivate:"), 1);
        if mute {
            // CATapMutedWhenTapped: the speakers are silent only while this tap runs.
            send_isize(desc, sel(c"setMuteBehavior:"), 2);
        }
        let st = AudioHardwareCreateProcessTap(desc, &mut t.tap);
        send0(desc, sel(c"release"));
        check("AudioHardwareCreateProcessTap", st)?;

        let mut asbd: Asbd = std::mem::zeroed();
        let mut size = std::mem::size_of::<Asbd>() as u32;
        check(
            "tap format",
            AudioObjectGetPropertyData(t.tap, &addr(b"tfmt"), 0, null(), &mut size, &mut asbd as *mut _ as *mut c_void),
        )?;
        if asbd.format_id != fourcc(b"lpcm") || asbd.bits_per_channel != 32 || asbd.format_flags & 1 == 0 {
            return Err(format!(
                "tap format is not 32-bit float PCM (id {:#x}, {} bits)",
                asbd.format_id, asbd.bits_per_channel
            ));
        }
        t.rate = asbd.sample_rate;

        let mut tuid: *const c_void = null();
        let mut tsize = std::mem::size_of::<*const c_void>() as u32;
        check(
            "tap uid",
            AudioObjectGetPropertyData(t.tap, &addr(b"tuid"), 0, null(), &mut tsize, &mut tuid as *mut _ as *mut c_void),
        )?;

        let name = cfstr("wavelink-tap");
        let uid = cfstr(&format!("wavelink-tap-{}", std::process::id()));
        let subs = CFArrayCreate(null(), [null::<c_void>(); 0].as_ptr(), 0, &kCFTypeArrayCallBacks as *const u8 as *const c_void);
        let keys = [cfstr("name"), cfstr("uid"), cfstr("subdevices"), cfstr("private"), cfstr("stacked")];
        let vals = [name, uid, subs, kCFBooleanTrue, kCFBooleanFalse];
        let dict = CFDictionaryCreate(
            null(),
            keys.as_ptr(),
            vals.as_ptr(),
            5,
            &kCFTypeDictionaryKeyCallBacks as *const u8 as *const c_void,
            &kCFTypeDictionaryValueCallBacks as *const u8 as *const c_void,
        );
        let st = AudioHardwareCreateAggregateDevice(dict, &mut t.dev);
        CFRelease(dict);
        for k in keys {
            CFRelease(k);
        }
        CFRelease(name);
        CFRelease(uid);
        CFRelease(subs);
        check("AudioHardwareCreateAggregateDevice", st)?;

        let list = CFArrayCreate(null(), [tuid].as_ptr(), 1, &kCFTypeArrayCallBacks as *const u8 as *const c_void);
        let st = AudioObjectSetPropertyData(
            t.dev,
            &addr(b"tap#"),
            0,
            null(),
            std::mem::size_of::<*const c_void>() as u32,
            &list as *const _ as *const c_void,
        );
        CFRelease(list);
        CFRelease(tuid);
        check("tap list on the aggregate device", st)?;
    }
    Ok(t)
}

#[cfg(target_os = "macos")]
impl Tap {
    /// Starts the IO proc: every callback's buffers go through `to_stereo` into `tx`; a full channel drops the buffer.
    pub fn start(&mut self, tx: SyncSender<Vec<f32>>) -> Result<(), String> {
        let client = Box::into_raw(Box::new(tx)) as *mut c_void;
        self.client = client;
        let mut proc_id: *mut c_void = std::ptr::null_mut();
        check("AudioDeviceCreateIOProcID", unsafe { ffi::AudioDeviceCreateIOProcID(self.dev, io_proc, client, &mut proc_id) })?;
        self.proc_id = proc_id;
        check("AudioDeviceStart", unsafe { ffi::AudioDeviceStart(self.dev, proc_id) })
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn io_proc(
    _dev: u32,
    _now: *const c_void,
    input: *const c_void,
    _in_time: *const c_void,
    _out: *mut c_void,
    _out_time: *const c_void,
    client: *mut c_void,
) -> i32 {
    if input.is_null() || client.is_null() {
        return 0;
    }
    let tx = unsafe { &*(client as *const SyncSender<Vec<f32>>) };
    // AudioBufferList: mNumberBuffers, then the AudioBuffer array at offset 8.
    let n = unsafe { *(input as *const u32) } as usize;
    let list = unsafe { (input as *const u8).add(8) as *const ffi::AudioBuffer };
    let channels = unsafe { (*list).channels.max(1) } as usize;
    let mut slices: Vec<&[f32]> = Vec::with_capacity(n);
    for i in 0..n {
        let b = unsafe { &*list.add(i) };
        if b.data.is_null() || b.byte_size < 4 {
            continue;
        }
        slices.push(unsafe { std::slice::from_raw_parts(b.data as *const f32, b.byte_size as usize / 4) });
    }
    if !slices.is_empty() {
        let _ = tx.try_send(to_stereo(&slices, channels));
    }
    0
}

#[cfg(target_os = "macos")]
impl Drop for Tap {
    fn drop(&mut self) {
        unsafe {
            if !self.proc_id.is_null() {
                ffi::AudioDeviceStop(self.dev, self.proc_id);
                ffi::AudioDeviceDestroyIOProcID(self.dev, self.proc_id);
            }
            if self.dev != 0 {
                ffi::AudioHardwareDestroyAggregateDevice(self.dev);
            }
            if self.tap != 0 {
                ffi::AudioHardwareDestroyProcessTap(self.tap);
            }
            // The IO proc is gone, so nothing can reach the sender any more.
            if !self.client.is_null() {
                drop(Box::from_raw(self.client as *mut SyncSender<Vec<f32>>));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(frames: usize, rate: f64, hz: f64) -> Vec<f32> {
        let mut v = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            let x = (2.0 * std::f64::consts::PI * hz * i as f64 / rate).sin() as f32 * 0.5;
            v.push(x);
            v.push(x);
        }
        v
    }

    fn lcg(s: &mut u64) -> usize {
        *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (*s >> 33) as usize
    }

    fn crossings(v: &[f32]) -> usize {
        // Left channel only: consecutive frames, not the L/R pairs of one frame.
        let left: Vec<f32> = v.chunks_exact(2).map(|f| f[0]).collect();
        left.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count()
    }

    #[test]
    fn resampler_output_does_not_depend_on_block_split() {
        let input = sine(4800, 48000.0, 440.0);
        let mut whole = Resampler::new(48000.0);
        let mut a = Vec::new();
        whole.push(&input, &mut a);
        let mut s = 9u64;
        let mut split = Resampler::new(48000.0);
        let mut b = Vec::new();
        let mut at = 0;
        while at < input.len() {
            let n = (1 + lcg(&mut s) % 300) * 2;
            let end = (at + n).min(input.len());
            split.push(&input[at..end], &mut b);
            at = end;
        }
        assert_eq!(a, b, "block split changed the output");
    }

    #[test]
    fn resampler_48k_to_44k1_keeps_length_and_pitch() {
        let input = sine(48000, 48000.0, 440.0);
        let mut r = Resampler::new(48000.0);
        let mut out = Vec::new();
        r.push(&input, &mut out);
        let frames = out.len() / 2;
        assert!((frames as i64 - 44100).abs() <= 2, "{frames} frames");
        let zc = crossings(&out);
        assert!((zc as i64 - 880).abs() <= 2, "{zc} zero crossings");
    }

    #[test]
    fn resampler_same_rate_is_passthrough() {
        let input = sine(1000, 44100.0, 300.0);
        let mut r = Resampler::new(44100.0);
        let mut out = Vec::new();
        r.push(&input[..500], &mut out);
        r.push(&input[500..], &mut out);
        assert_eq!(out, input);
    }

    fn wav_bytes(tag: u16, bits: u16, ch: u16, rate: u32, data: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&((36 + data.len()) as u32).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&tag.to_le_bytes());
        b.extend_from_slice(&ch.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        let align = ch as u32 * (bits as u32 / 8);
        b.extend_from_slice(&(rate * align).to_le_bytes());
        b.extend_from_slice(&(align as u16).to_le_bytes());
        b.extend_from_slice(&bits.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&(data.len() as u32).to_le_bytes());
        b.extend_from_slice(data);
        b
    }

    #[test]
    fn wav_pcm_reads_pcm16_and_float32_mono_and_stereo() {
        let s16: Vec<u8> = [16384i16, -16384].iter().flat_map(|v| v.to_le_bytes()).collect();
        let (rate, v) = wav_pcm(&wav_bytes(1, 16, 1, 44100, &s16)).unwrap();
        assert_eq!(rate, 44100);
        assert_eq!(v, vec![0.5, 0.5, -0.5, -0.5], "mono is duplicated to stereo");
        let f32s: Vec<u8> = [0.25f32, -0.25, 0.75, 0.0].iter().flat_map(|v| v.to_le_bytes()).collect();
        let (rate, v) = wav_pcm(&wav_bytes(3, 32, 2, 48000, &f32s)).unwrap();
        assert_eq!(rate, 48000);
        assert_eq!(v, vec![0.25, -0.25, 0.75, 0.0]);
    }

    #[test]
    fn wav_pcm_rejects_other_formats() {
        assert!(wav_pcm(&wav_bytes(1, 24, 2, 44100, &[0u8; 12])).is_err(), "24-bit");
        assert!(wav_pcm(&wav_bytes(1, 16, 6, 44100, &[0u8; 24])).is_err(), "6 channels");
        assert!(wav_pcm(b"not a wav file at all").is_err());
    }

    #[test]
    fn to_stereo_shapes() {
        let a = [1.0f32, 2.0, 3.0, 4.0];
        assert_eq!(to_stereo(&[&a[..]], 2), a.to_vec(), "interleaved stereo is copied");
        assert_eq!(to_stereo(&[&[0.5f32, -0.5][..]], 1), vec![0.5, 0.5, -0.5, -0.5], "mono is duplicated");
        assert_eq!(to_stereo(&[&[1.0f32, 2.0][..], &[3.0, 4.0][..]], 1), vec![1.0, 3.0, 2.0, 4.0], "two mono buffers");
        assert_eq!(to_stereo(&[&[1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0][..]], 3), vec![1.0, 2.0, 4.0, 5.0], "first two of 3 ch");
    }

    #[test]
    fn feed_writes_the_pcm_then_silence_until_the_pipe_closes() {
        struct Limit {
            buf: Vec<u8>,
            left: usize,
        }
        impl Write for Limit {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                if self.left == 0 {
                    return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "closed"));
                }
                self.left -= 1;
                self.buf.extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let pcm: Vec<f32> = (0..3 * 441 * 2).map(|i| i as f32).collect();
        let mut w = Limit { buf: Vec::new(), left: 5 };
        let n = feed(&pcm, &mut w, false);
        assert_eq!(n, 5, "slices written before the pipe closed");
        let slice = 441 * 2 * 4;
        assert_eq!(w.buf.len(), 5 * slice);
        let want: Vec<u8> = pcm.iter().flat_map(|v| v.to_le_bytes()).collect();
        assert_eq!(&w.buf[..3 * slice], &want[..], "the pcm comes first");
        assert!(w.buf[3 * slice..].iter().all(|&b| b == 0), "then silence");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn ffi_struct_layouts() {
        use ffi::*;
        assert_eq!(std::mem::size_of::<Asbd>(), 40);
        assert_eq!(std::mem::size_of::<AudioBuffer>(), 16);
        assert_eq!(std::mem::offset_of!(AudioBuffer, data), 8);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn tap_class_and_selectors_exist() {
        assert!(ffi::tap_selectors_present(), "CATapDescription with its three selectors");
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "creates a real tap and private aggregate device; run once by hand"]
    fn mac_tap_objects_create_and_destroy() {
        let t = open(false).expect("tap and aggregate device");
        assert!(t.rate > 0.0);
        drop(t);
    }
}
