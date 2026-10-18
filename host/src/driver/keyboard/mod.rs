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

/// Key press in the canonical cross-platform shape (see
/// docs/keyboard.md): `keycode` is the HID usage id (page 0x07),
/// `modifiers` the HID boot-protocol bitmask of the same keyboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyEvent {
    /// Internal name: "main" or the key of `Setup.keyboards`.
    pub keyboard_id: String,
    pub modifiers: u32,
    pub keycode: u32,
    pub action: KeyAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    Press,
    Release,
}

impl KeyAction {
    pub fn as_str(self) -> &'static str {
        match self {
            KeyAction::Press => "press",
            KeyAction::Release => "release",
        }
    }
}

/// Boot-protocol modifier bits with their short names.
pub const MODIFIER_NAMES: [(u32, &str); 8] = [
    (0x01, "LCtrl"),
    (0x02, "LShift"),
    (0x04, "LAlt"),
    (0x08, "LGui"),
    (0x10, "RCtrl"),
    (0x20, "RShift"),
    (0x40, "RAlt"),
    (0x80, "RGui"),
];

/// "LCtrl+LShift" for a non-empty mask, empty string otherwise. Order
/// follows `MODIFIER_NAMES`; the caller joins the combo as Mod+Mod+Key.
pub fn modifier_names(mask: u32) -> String {
    let mut s = String::new();
    for (bit, name) in MODIFIER_NAMES {
        if mask & bit != 0 {
            if !s.is_empty() {
                s.push('+');
            }
            s.push_str(name);
        }
    }
    s
}

/// Boot-protocol bit of a modifier usage (0xE0..=0xE7), if it is one.
pub fn modifier_bit(hid_usage: u32) -> Option<u32> {
    (0xE0..=0xE7)
        .contains(&hid_usage)
        .then(|| 1 << (hid_usage - 0xE0))
}

/// Per-device HID modifier state: a Press of a modifier usage sets its
/// bit, a Release clears it. Modifier keys still arrive as regular
/// KeyEvents — the mask and the key events are independent, as in HID.
#[derive(Debug, Default, Clone, Copy)]
pub struct ModifierTracker {
    mask: u32,
}

impl ModifierTracker {
    /// Apply one mapped key action; returns the fresh mask.
    pub fn apply(&mut self, hid_usage: u32, action: KeyAction) -> u32 {
        if let Some(bit) = modifier_bit(hid_usage) {
            match action {
                KeyAction::Press => self.mask |= bit,
                KeyAction::Release => self.mask &= !bit,
            }
        }
        self.mask
    }
}

/// Canonical key name from the HID Usage Tables (page 0x07) with the
/// doc's abbreviations: the "Keyboard " prefix is dropped, "a and A"
/// forms collapse to the uppercase letter. Unknown codes render as
/// "0xNN".
pub fn key_name(keycode: u32) -> String {
    const X: Option<&str> = None;
    // 0x00..=0xE7, one entry per usage.
    const NAMES: [Option<&str>; 0xE8] = [
        X,
        X,
        X,
        X, // 0x00-0x03: NoEvent / error rollover usages
        Some("A"),
        Some("B"),
        Some("C"),
        Some("D"),
        Some("E"),
        Some("F"),
        Some("G"),
        Some("H"),
        Some("I"),
        Some("J"),
        Some("K"),
        Some("L"),
        Some("M"),
        Some("N"),
        Some("O"),
        Some("P"),
        Some("Q"),
        Some("R"),
        Some("S"),
        Some("T"),
        Some("U"),
        Some("V"),
        Some("W"),
        Some("X"),
        Some("Y"),
        Some("Z"),
        Some("1"),
        Some("2"),
        Some("3"),
        Some("4"),
        Some("5"),
        Some("6"),
        Some("7"),
        Some("8"),
        Some("9"),
        Some("0"),
        Some("Enter"),
        Some("Escape"),
        Some("Backspace"),
        Some("Tab"),
        Some("Spacebar"),
        Some("-"),
        Some("="),
        Some("["),
        Some("]"),
        Some("\\"),
        Some("NonUS#"),
        Some(";"),
        Some("'"),
        Some("`"),
        Some(","),
        Some("."),
        Some("/"),
        Some("CapsLock"),
        Some("F1"),
        Some("F2"),
        Some("F3"),
        Some("F4"),
        Some("F5"),
        Some("F6"),
        Some("F7"),
        Some("F8"),
        Some("F9"),
        Some("F10"),
        Some("F11"),
        Some("F12"),
        Some("PrintScreen"),
        Some("ScrollLock"),
        Some("Pause"),
        Some("Insert"),
        Some("Home"),
        Some("PageUp"),
        Some("Delete"),
        Some("End"),
        Some("PageDown"),
        Some("RightArrow"),
        Some("LeftArrow"),
        Some("DownArrow"),
        Some("UpArrow"),
        Some("NumLock"),
        Some("Keypad/"),
        Some("Keypad*"),
        Some("Keypad-"),
        Some("Keypad+"),
        Some("KeypadEnter"),
        Some("Keypad1"),
        Some("Keypad2"),
        Some("Keypad3"),
        Some("Keypad4"),
        Some("Keypad5"),
        Some("Keypad6"),
        Some("Keypad7"),
        Some("Keypad8"),
        Some("Keypad9"),
        Some("Keypad0"),
        Some("Keypad."),
        Some("NonUS\\"),
        Some("Application"),
        Some("Power"),
        Some("Keypad="),
        Some("F13"),
        Some("F14"),
        Some("F15"),
        Some("F16"),
        Some("F17"),
        Some("F18"),
        Some("F19"),
        Some("F20"),
        Some("F21"),
        Some("F22"),
        Some("F23"),
        Some("F24"),
        Some("Execute"),
        Some("Help"),
        Some("Menu"),
        Some("Select"),
        Some("Stop"),
        Some("Again"),
        Some("Undo"),
        Some("Cut"),
        Some("Copy"),
        Some("Paste"),
        Some("Find"),
        Some("Mute"),
        Some("VolumeUp"),
        Some("VolumeDown"),
        Some("LockingCapsLock"),
        Some("LockingNumLock"),
        Some("LockingScrollLock"),
        Some("KeypadComma"),
        Some("KeypadEqualSign"),
        Some("International1"),
        Some("International2"),
        Some("International3"),
        Some("International4"),
        Some("International5"),
        Some("International6"),
        Some("International7"),
        Some("International8"),
        Some("International9"),
        Some("LANG1"),
        Some("LANG2"),
        Some("LANG3"),
        Some("LANG4"),
        Some("LANG5"),
        Some("LANG6"),
        Some("LANG7"),
        Some("LANG8"),
        Some("LANG9"),
        Some("AlternateErase"),
        Some("SysReq"),
        Some("Cancel"),
        Some("Clear"),
        Some("Prior"),
        Some("Return"),
        Some("Separator"),
        Some("Out"),
        Some("Oper"),
        Some("ClearAgain"),
        Some("CrSel"),
        Some("ExSel"),
        // 0xA5..=0xDF reserved
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        X,
        Some("LeftControl"),
        Some("LeftShift"),
        Some("LeftAlt"),
        Some("LeftGui"),
        Some("RightControl"),
        Some("RightShift"),
        Some("RightAlt"),
        Some("RightGui"),
    ];
    debug_assert_eq!(NAMES.len(), 0xE8);
    match NAMES.get(keycode as usize).copied().flatten() {
        Some(name) => name.to_owned(),
        None => format!("0x{keycode:02X}"),
    }
}

/// Why attaching one configured keyboard failed. Per-device: one
/// keyboard failing does not affect the others.
#[derive(Debug)]
pub enum AttachError {
    /// The filter matched no discovered keyboard.
    NoMatch,
    /// The filter matched more than one; the list is the candidates.
    Ambiguous(Vec<KeyboardInfo>),
    /// Grab mode requested for a device that cannot be grabbed.
    ModeUnsupported,
    /// Open or EVIOCGRAB failed (permissions, busy, gone mid-way).
    Io(std::io::Error),
}

impl std::fmt::Display for AttachError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            AttachError::NoMatch => write!(f, "no keyboard matches the filter"),
            AttachError::Ambiguous(candidates) => {
                write!(f, "filter matches {} keyboards:", candidates.len())?;
                for c in candidates {
                    write!(f, " {} ({:04x}:{:04x}", c.name, c.vid, c.pid)?;
                    if let Some(s) = &c.serial {
                        write!(f, ", {s}")?;
                    }
                    write!(f, ")")?;
                }
                Ok(())
            }
            AttachError::ModeUnsupported => {
                write!(f, "grab mode is not supported for this device")
            }
            AttachError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for AttachError {}

/// Attach one configured keyboard: open the device it matches and
/// return the event stream. The receiver disconnects when the device is
/// unplugged or the thread dies.
#[cfg(target_os = "linux")]
pub fn open(
    id: &str,
    cfg: &crate::config::keyboard::KeyboardConfig,
) -> Result<crossbeam_channel::Receiver<KeyEvent>, AttachError> {
    linux::open(id, cfg)
}

/// Platform stub until a real backend lands: nothing can match.
#[cfg(not(target_os = "linux"))]
pub fn open(
    id: &str,
    cfg: &crate::config::keyboard::KeyboardConfig,
) -> Result<crossbeam_channel::Receiver<KeyEvent>, AttachError> {
    let _ = (id, cfg);
    Err(AttachError::NoMatch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_names_use_hid_names_with_doc_abbreviations() {
        assert_eq!(key_name(0x70), "F21");
        assert_eq!(key_name(0x04), "A"); // "Keyboard a and A" collapsed
        assert_eq!(key_name(0x52), "UpArrow");
        assert_eq!(key_name(0xE0), "LeftControl");
        assert_eq!(key_name(0x2C), "Spacebar");
        // Unknown / reserved usages render as 0xNN.
        assert_eq!(key_name(0x00), "0x00");
        assert_eq!(key_name(0xA5), "0xA5");
        assert_eq!(key_name(0xE8), "0xE8");
    }

    #[test]
    fn modifier_bit_maps_the_usage_range() {
        assert_eq!(modifier_bit(0xE0), Some(0x01));
        assert_eq!(modifier_bit(0xE2), Some(0x04));
        assert_eq!(modifier_bit(0xE7), Some(0x80));
        assert_eq!(modifier_bit(0xE8), None);
        assert_eq!(modifier_bit(0x04), None);
    }

    #[test]
    fn tracker_sets_and_clears_bits() {
        let mut t = ModifierTracker::default();
        // Press LShift then LCtrl: mask accumulates.
        assert_eq!(t.apply(0xE1, KeyAction::Press), 0x02);
        assert_eq!(t.apply(0xE0, KeyAction::Press), 0x03);
        // A regular key leaves the mask alone.
        assert_eq!(t.apply(0x70, KeyAction::Press), 0x03);
        // Release LShift clears only its bit.
        assert_eq!(t.apply(0xE1, KeyAction::Release), 0x01);
        assert_eq!(t.apply(0xE0, KeyAction::Release), 0x00);
    }

    #[test]
    fn modifier_names_render_plus_list() {
        assert_eq!(modifier_names(0), "");
        assert_eq!(modifier_names(0x02), "LShift");
        assert_eq!(modifier_names(0x03), "LCtrl+LShift");
        assert_eq!(modifier_names(0x82), "LShift+RGui");
    }
}
