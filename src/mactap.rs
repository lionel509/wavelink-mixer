//! Mac capture for #77: the system output through one Core Audio process tap, as a child with the pw-cat stdout
//! contract (raw interleaved f32 LE, 44100 Hz, 2 channels, no header). Compile-only skeleton with the tests first.
#![allow(dead_code)]
use std::io::Write;

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
        let _ = in_rate;
        unimplemented!("#77 U2: Resampler::new")
    }

    /// `inp` is interleaved stereo at the input rate; the output is appended to `out`, interleaved stereo at 44100.
    pub fn push(&mut self, inp: &[f32], out: &mut Vec<f32>) {
        let _ = (inp, out, &self.step, &self.pos, &self.prev, &self.primed);
        unimplemented!("#77 U2: Resampler::push")
    }
}

/// Reads a RIFF/WAVE file: PCM 16-bit or IEEE float 32-bit, mono or stereo. Returns (rate, interleaved stereo).
pub fn wav_pcm(bytes: &[u8]) -> Result<(u32, Vec<f32>), String> {
    let _ = bytes;
    unimplemented!("#77 U2: wav_pcm")
}

/// Shapes the buffers of one capture callback into interleaved stereo. `channels` is the channel count of each
/// buffer; one buffer is taken as interleaved, two or more mono buffers are interleaved as left and right.
pub fn to_stereo(bufs: &[&[f32]], channels: usize) -> Vec<f32> {
    let _ = (bufs, channels);
    unimplemented!("#77 U2: to_stereo")
}

/// Writes `pcm` (interleaved stereo at 44100) in 10 ms slices, then silence, until a write fails. Returns the number
/// of slices written. With `pace`, slice k is due at k * 10 ms from the start.
pub fn feed(pcm: &[f32], out: &mut impl Write, pace: bool) -> usize {
    let _ = (pcm, out, pace);
    unimplemented!("#77 U2: feed")
}

/// `wav-tap FILE`: the pw-cat contract fed from a WAV at real-time pace. Exit code.
pub fn wav_run(args: &[String]) -> i32 {
    let _ = args;
    unimplemented!("#77 U2: wav_run")
}

/// `mac-tap [--mute]`: the Core Audio process tap. Exit code. Not macOS: prints a note and returns 2.
pub fn run(args: &[String]) -> i32 {
    let _ = args;
    unimplemented!("#77 U2: run")
}

#[cfg(target_os = "macos")]
pub mod ffi {
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

    /// Whether CATapDescription exists and responds to the three selectors `open` sends.
    pub fn tap_selectors_present() -> bool {
        unimplemented!("#77 U2: tap_selectors_present")
    }
}

/// A live Core Audio tap with its private aggregate device. Dropping it stops and destroys both.
#[cfg(target_os = "macos")]
pub struct Tap {
    pub rate: f64,
}

#[cfg(target_os = "macos")]
pub fn open(mute: bool) -> Result<Tap, String> {
    let _ = mute;
    unimplemented!("#77 U2: open")
}

#[cfg(target_os = "macos")]
impl Drop for Tap {
    fn drop(&mut self) {}
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
        v.windows(2).step_by(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count()
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
