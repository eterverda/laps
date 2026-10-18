//! Keyboard driver: platform-independent contract, platform backends in
//! submodules. Backends deliver key presses to the application in one
//! shape regardless of the source (the focus keyboard wrapped from egui,
//! USB macro pads, any HID keyboard) — see `docs/keyboard.md`.
//!
//! Currently only the Linux backend (`linux`, evdev/sysfs discovery)
//! exists; macOS (IOKit) will join as a sibling module. The discovery
//! CLI (`list-keyboards`) stays Linux-only — it is specific to evdev.

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "linux")]
pub use linux::scan;

/// Platform stub until a real backend lands: an empty device list.
#[cfg(not(target_os = "linux"))]
pub fn scan() -> std::io::Result<Vec<KeyboardInfo>> {
    Ok(Vec::new())
}

/// A discovered keyboard. Identity fields mirror the matcher of the
/// future `KeyboardConfig` (name substring, exact vid/pid/serial); every
/// platform can supply them, only the collection method differs.
///
/// `listenable`/`grabbable` are the attach capabilities, the probed
/// access truth, not policy. On Linux they come from an open-probe of
/// the node (`linux::probe`); a platform with no per-device mechanism
/// fills them from its own rules — there is no grab API on Windows, so
/// a Windows backend answers `grabbable: false` for every device. The
/// ban on grabbing the system keyboard lives in the driver/config
/// validation, not here.
///
/// No Eq/Ord: matching a config entry is a filter, not equality, and
/// the only ordering need is the deterministic listing (a plain sort
/// key in `scan`).
#[derive(Debug, Clone)]
pub struct KeyboardInfo {
    pub name: String,
    pub vid: u16,
    pub pid: u16,
    pub serial: Option<String>,
    pub listenable: bool,
    pub grabbable: bool,
}
