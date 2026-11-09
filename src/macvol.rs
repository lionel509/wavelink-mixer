//! The Mac's own output volume (CoreAudio, #37): the default output device's virtual main volume and mute,
//! through hand-declared FFI (no new crate). Elsewhere every function is a stub that reports "no device".
#![allow(dead_code)]

/// The default output device's volume (0..=100) and mute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Volume {
    pub volume: u8,
    pub mute: bool,
}

/// Keeps the CoreAudio listeners alive; dropping it removes every one of them.
pub struct Watch {
    #[cfg(target_os = "macos")]
    inner: *mut std::ffi::c_void,
}

impl Drop for Watch {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        mac::unwatch(self.inner);
    }
}

#[cfg(target_os = "macos")]
#[allow(unused_imports)] // watch is consumed by the console (U4)
pub use mac::{read, watch, write};

/// Current default output volume and mute; None when there is no default output or no volume control.
#[cfg(not(target_os = "macos"))]
pub fn read() -> Option<Volume> {
    None
}

/// Set the default output's volume and mute. False when the device refuses.
#[cfg(not(target_os = "macos"))]
pub fn write(_v: Volume) -> bool {
    false
}

/// Call `f` when the default output device or its volume/mute changes (a volume key, a device switch).
/// `f` runs on a CoreAudio thread: it must not touch GTK or any socket.
#[cfg(not(target_os = "macos"))]
pub fn watch(_f: impl Fn() + Send + Sync + 'static) -> Option<Watch> {
    None
}

/// 0..=100 percent to the 0.0..=1.0 scalar CoreAudio uses.
pub fn pct_to_scalar(p: u8) -> f32 {
    p.min(100) as f32 / 100.0
}

/// The inverse of `pct_to_scalar`, rounded to the nearest percent and clamped to 0..=100.
pub fn scalar_to_pct(s: f32) -> u8 {
    (s.clamp(0.0, 1.0) * 100.0).round() as u8
}

/// A four-character code as CoreAudio spells property selectors (`vmvc`, `dOut`, ...).
pub const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

#[cfg(target_os = "macos")]
mod ffi {
    use std::ffi::c_void;

    pub type OSStatus = i32;
    pub type AudioObjectID = u32;

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Address {
        pub selector: u32,
        pub scope: u32,
        pub element: u32,
    }

    pub type Listener = unsafe extern "C" fn(AudioObjectID, u32, *const Address, *mut c_void) -> OSStatus;

    #[link(name = "CoreAudio", kind = "framework")]
    extern "C" {
        pub fn AudioObjectGetPropertyData(object: AudioObjectID, addr: *const Address, qual_size: u32, qual: *const c_void, io_size: *mut u32, data: *mut c_void) -> OSStatus;
        pub fn AudioObjectSetPropertyData(object: AudioObjectID, addr: *const Address, qual_size: u32, qual: *const c_void, size: u32, data: *const c_void) -> OSStatus;
        /// CoreAudio's `Boolean` is one byte.
        pub fn AudioObjectHasProperty(object: AudioObjectID, addr: *const Address) -> u8;
        pub fn AudioObjectAddPropertyListener(object: AudioObjectID, addr: *const Address, listener: Listener, client: *mut c_void) -> OSStatus;
        pub fn AudioObjectRemovePropertyListener(object: AudioObjectID, addr: *const Address, listener: Listener, client: *mut c_void) -> OSStatus;
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use super::ffi::{self, Address, AudioObjectID, OSStatus};
    use super::{fourcc, pct_to_scalar, scalar_to_pct, Volume, Watch};
    use std::ffi::c_void;
    use std::sync::Mutex;

    /// kAudioObjectSystemObject
    const SYSTEM: AudioObjectID = 1;
    const DEFAULT_OUTPUT: u32 = fourcc(b"dOut");
    const GLOBAL: u32 = fourcc(b"glob");
    const OUTPUT: u32 = fourcc(b"outp");
    const VIRTUAL_MAIN_VOLUME: u32 = fourcc(b"vmvc");
    const VOLUME: u32 = fourcc(b"volm");
    const MUTE: u32 = fourcc(b"mute");

    fn addr(selector: u32, scope: u32, element: u32) -> Address {
        Address { selector, scope, element }
    }

    fn has(obj: AudioObjectID, a: &Address) -> bool {
        unsafe { ffi::AudioObjectHasProperty(obj, a) != 0 }
    }

    fn get<T: Copy + Default>(obj: AudioObjectID, a: &Address) -> Option<T> {
        let mut v = T::default();
        let mut size = std::mem::size_of::<T>() as u32;
        let st = unsafe { ffi::AudioObjectGetPropertyData(obj, a, 0, std::ptr::null(), &mut size, &mut v as *mut T as *mut c_void) };
        (st == 0).then_some(v)
    }

    fn set<T: Copy>(obj: AudioObjectID, a: &Address, v: &T) -> bool {
        let st = unsafe { ffi::AudioObjectSetPropertyData(obj, a, 0, std::ptr::null(), std::mem::size_of::<T>() as u32, v as *const T as *const c_void) };
        st == 0
    }

    fn default_device() -> Option<AudioObjectID> {
        let id: u32 = get(SYSTEM, &addr(DEFAULT_OUTPUT, GLOBAL, 0))?;
        (id != 0).then_some(id)
    }

    /// The volume controls a device has: the virtual main volume, else the per-channel `volm` on elements 1 and 2.
    fn volume_addrs(dev: AudioObjectID) -> Vec<Address> {
        let main = addr(VIRTUAL_MAIN_VOLUME, OUTPUT, 0);
        if has(dev, &main) {
            return vec![main];
        }
        [1, 2].into_iter().map(|e| addr(VOLUME, OUTPUT, e)).filter(|a| has(dev, a)).collect()
    }

    fn mute_addr() -> Address {
        addr(MUTE, OUTPUT, 0)
    }

    pub fn read() -> Option<Volume> {
        let dev = default_device()?;
        let addrs = volume_addrs(dev);
        if addrs.is_empty() {
            return None;
        }
        let loudest = addrs.iter().filter_map(|a| get::<f32>(dev, a)).reduce(f32::max)?;
        let mute = get::<u32>(dev, &mute_addr()).is_some_and(|m| m != 0);
        Some(Volume { volume: scalar_to_pct(loudest), mute })
    }

    pub fn write(v: Volume) -> bool {
        let Some(dev) = default_device() else { return false };
        let addrs = volume_addrs(dev);
        if addrs.is_empty() {
            return false;
        }
        let scalar = pct_to_scalar(v.volume);
        let volume_ok = addrs.iter().all(|a| set(dev, a, &scalar));
        let mute_ok = !has(dev, &mute_addr()) || set(dev, &mute_addr(), &(v.mute as u32));
        volume_ok && mute_ok
    }

    /// State behind the listeners: the device they sit on and the addresses added to it.
    #[derive(Default)]
    struct State {
        attached: Option<(AudioObjectID, Vec<Address>)>,
        closed: bool,
    }

    /// Passed to CoreAudio as client data; leaked by `watch`, freed by `unwatch`.
    pub struct Inner {
        f: Box<dyn Fn() + Send + Sync>,
        state: Mutex<State>,
    }

    fn lock(m: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Adds the volume and mute listeners to the current default device.
    fn attach(client: *mut c_void) -> Option<(AudioObjectID, Vec<Address>)> {
        let dev = default_device()?;
        let mut addrs = volume_addrs(dev);
        if has(dev, &mute_addr()) {
            addrs.push(mute_addr());
        }
        for a in &addrs {
            unsafe { ffi::AudioObjectAddPropertyListener(dev, a, on_change, client) };
        }
        Some((dev, addrs))
    }

    fn detach(client: *mut c_void, dev: AudioObjectID, addrs: &[Address]) {
        for a in addrs {
            unsafe { ffi::AudioObjectRemovePropertyListener(dev, a, on_change, client) };
        }
    }

    /// The default device changed: move the device listeners to the new one.
    fn retarget(client: *mut c_void, inner: &Inner) {
        let mut st = lock(&inner.state);
        if st.closed {
            return;
        }
        if let Some((dev, addrs)) = st.attached.take() {
            detach(client, dev, &addrs);
        }
        st.attached = attach(client);
    }

    unsafe extern "C" fn on_change(obj: AudioObjectID, n: u32, addrs: *const Address, client: *mut c_void) -> OSStatus {
        let inner = &*(client as *const Inner);
        let changed: &[Address] = if addrs.is_null() { &[] } else { std::slice::from_raw_parts(addrs, n as usize) };
        if obj == SYSTEM && changed.iter().any(|a| a.selector == DEFAULT_OUTPUT) {
            retarget(client, inner);
        }
        (inner.f)();
        0
    }

    pub fn watch(f: impl Fn() + Send + Sync + 'static) -> Option<Watch> {
        let inner = Box::into_raw(Box::new(Inner { f: Box::new(f), state: Mutex::new(State::default()) }));
        let client = inner as *mut c_void;
        let sys = addr(DEFAULT_OUTPUT, GLOBAL, 0);
        if unsafe { ffi::AudioObjectAddPropertyListener(SYSTEM, &sys, on_change, client) } != 0 {
            unsafe { drop(Box::from_raw(inner)) };
            return None;
        }
        let attached = attach(client);
        lock(unsafe { &(*inner).state }).attached = attached;
        Some(Watch { inner: client })
    }

    /// Removes every listener this watch added, then frees its state.
    pub fn unwatch(client: *mut c_void) {
        let inner = unsafe { &*(client as *const Inner) };
        {
            let mut st = lock(&inner.state);
            st.closed = true;
            if let Some((dev, addrs)) = st.attached.take() {
                detach(client, dev, &addrs);
            }
            let sys = addr(DEFAULT_OUTPUT, GLOBAL, 0);
            unsafe { ffi::AudioObjectRemovePropertyListener(SYSTEM, &sys, on_change, client) };
        }
        unsafe { drop(Box::from_raw(client as *mut Inner)) };
    }
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
