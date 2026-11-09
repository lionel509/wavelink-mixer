//! The Mac's own output volume (CoreAudio, #37): the default output device's virtual main volume and mute,
//! through hand-declared FFI (no new crate). Elsewhere every function is a stub that reports "no device".
//! SCAFFOLD: signatures and types only (U3 fills them in).
#![allow(dead_code)]

/// The default output device's volume (0..=100) and mute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Volume {
    pub volume: u8,
    pub mute: bool,
}

/// Current default output volume and mute; None when there is no default output or no volume control.
pub fn read() -> Option<Volume> {
    todo!("U3: macvol read")
}

/// Set the default output's volume and mute. False when the device refuses.
pub fn write(_v: Volume) -> bool {
    todo!("U3: macvol write")
}

/// Keeps the CoreAudio listeners alive; dropping it removes every one of them.
pub struct Watch;

/// Call `f` when the default output device or its volume/mute changes (a volume key, a device switch).
/// `f` runs on a CoreAudio thread: it must not touch GTK or any socket.
pub fn watch(_f: impl Fn() + Send + Sync + 'static) -> Option<Watch> {
    todo!("U3: macvol watch")
}

/// 0..=100 percent to the 0.0..=1.0 scalar CoreAudio uses.
pub fn pct_to_scalar(_p: u8) -> f32 {
    todo!("U3: pct_to_scalar")
}

/// The inverse of `pct_to_scalar`, rounded to the nearest percent and clamped to 0..=100.
pub fn scalar_to_pct(_s: f32) -> u8 {
    todo!("U3: scalar_to_pct")
}

/// A four-character code as CoreAudio spells property selectors (`vmvc`, `dOut`, ...).
pub const fn fourcc(s: &[u8; 4]) -> u32 {
    let _ = s;
    0 // scaffold: U3 returns u32::from_be_bytes(*s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pct_scalar_round_trip_is_identity_for_0_to_100() {
        for p in 0..=100u8 {
            assert_eq!(scalar_to_pct(pct_to_scalar(p)), p, "percent {p}");
        }
    }

    #[test]
    fn fourcc_matches_apple_headers() {
        assert_eq!(fourcc(b"vmvc"), 0x766D_7663);
        assert_eq!(fourcc(b"dOut"), 0x644F_7574);
        assert_eq!(fourcc(b"mute"), 0x6D75_7465);
        assert_eq!(fourcc(b"outp"), 0x6F75_7470);
        assert_eq!(fourcc(b"volm"), 0x766F_6C6D);
        assert_eq!(fourcc(b"glob"), 0x676C_6F62);
    }

    /// Touches the real default output: reads, writes the same value back, re-reads. Run by hand only.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn real_device_read_write_restore() {
        let before = read().expect("a default output with a volume control");
        assert!(write(before));
        assert_eq!(read(), Some(before));
    }
}
