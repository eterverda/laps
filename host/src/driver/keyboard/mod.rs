//! Keyboard input in one press shape (`KeyPressed`) from every source:
//! `Keyboard` — the focus keyboard wrapped from egui, the only source
//! outside Linux — and, on Linux, the device backend (`linux`, evdev/
//! sysfs) for external keyboards.

#[cfg(target_os = "linux")]
mod linux;

/// Key members of the linux backend; the module itself is private.
#[cfg(target_os = "linux")]
pub use linux::{ExclusiveKeyboard, ExclusiveKeyboardInfo};

use crossbeam_channel as crossbeam;
use fixedbitset::FixedBitSet;
use std::borrow::Cow;
use std::fmt::{Display, Formatter, Result as FmtResult};
use std::time::Instant;

/// Bitset capacity: usages run to 0xE7; the headroom to 0xFF keeps
/// the unknown-code render ("0xNN") from panicking on insert.
/// One key press in the canonical cross-platform shape: `keys` — all
/// keys of the keyboard held at the press front (HID usages, page
/// 0x07; modifiers are keys too, 0xE0..=0xE7). Press fronts only:
/// releases never reach the channel (each source tracks the held set
/// itself: egui — `keys_down`, linux — a set in the reader).
///
/// `exclusive_keyboard_id` (Linux-only): Some — the event came from
/// an exclusive keyboard (an id of `Setup.exclusive_keyboards`),
/// None — the GUI focus keyboard. Absent on other platforms: one
/// source, no identity needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyPressed {
    #[cfg(target_os = "linux")]
    pub exclusive_keyboard_id: Option<String>,
    pub keys: FixedBitSet,
    /// Reception moment, stamped by the driver as the event is created.
    pub at: Instant,
}

/// Linux: "key-1  LCtrl+LShift+A" with the id; main — combo only.
#[cfg(target_os = "linux")]
impl Display for KeyPressed {
    fn fmt(&self, f: &mut Formatter) -> FmtResult {
        match &self.exclusive_keyboard_id {
            Some(id) => write!(f, "{id}  {}", combo(&self.keys)),
            None => write!(f, "{}", combo(&self.keys)),
        }
    }
}

/// Outside Linux there is a single source — combo only.
#[cfg(not(target_os = "linux"))]
impl Display for KeyPressed {
    fn fmt(&self, f: &mut Formatter) -> FmtResult {
        write!(f, "{}", combo(&self.keys))
    }
}

/// A bitset of usages — construction and test helper.
pub fn key_set(usages: &[u32]) -> FixedBitSet {
    let mut set = FixedBitSet::with_capacity(KEY_USAGES);
    for &u in usages {
        set.insert(u as usize);
    }
    set
}

/// The main keyboard: the GUI focus keyboard (egui) as a source of
/// `KeyPressed` for the shared channel — the only source outside
/// Linux. Owned by the screen (Idle/Live), like the channel: the
/// sender clone is taken at its birth, it sends from the UI thread.
/// Sender of the shared keyboard channel: `pump` turns egui input
/// events into `KeyEvent` (exclusive_keyboard_id None — main).
pub struct Keyboard {
    tx: crossbeam::Sender<KeyPressed>,
}

impl Keyboard {
    pub fn new(events: &crossbeam::Sender<KeyPressed>) -> Self {
        Self { tx: events.clone() }
    }

    /// egui input events → `KeyPressed` (main: exclusive_keyboard_id
    /// None) into the shared channel. Physical keycodes — the layout
    /// does not affect the race control keys.
    pub fn pump(&self, ui: &egui::Ui) {
        for event in ui.input(|i| i.events.clone()) {
            if let egui::Event::Key {
                key,
                physical_key,
                pressed,
                repeat,
                ..
            } = event
            {
                // Press fronts only: autorepeat and releases are out.
                if repeat || !pressed {
                    continue;
                }
                // The physical keycode, layout-free; fall back to the
                // logical one when absent (web, legacy backends).
                if egui_key_usage(physical_key.unwrap_or(key)).is_none() {
                    continue; // the front only triggers; only mapped keys enter the set
                }
                let keys = ui.input(|i| {
                    let mut set = FixedBitSet::with_capacity(KEY_USAGES);
                    for usage in i.keys_down.iter().filter_map(|k| egui_key_usage(*k)) {
                        set.insert(usage as usize);
                    }
                    // Modifiers are keys too: from the egui snapshot.
                    let m = i.modifiers;
                    if m.ctrl {
                        set.insert(0xE0);
                    }
                    if m.shift {
                        set.insert(0xE1);
                    }
                    if m.alt {
                        set.insert(0xE2);
                    }
                    if m.mac_cmd {
                        set.insert(0xE3);
                    }
                    set
                });
                let _ = self.tx.send(KeyPressed {
                    #[cfg(target_os = "linux")]
                    exclusive_keyboard_id: None, // main
                    keys,
                    at: Instant::now(),
                });
            }
        }
    }
}

/// egui keycode → HID usage (page 0x07): letters, digits, F1-F24,
/// navigation — the race control keys. egui punctuation is
/// layout-dependent (the variant name is the produced glyph), no
/// reliable physical mapping — None.
/// Boot-protocol modifier bits with their short names.
const MODIFIER_NAMES: [(u32, &str); 8] = [
    (0x01, "LCtrl"),
    (0x02, "LShift"),
    (0x04, "LAlt"),
    (0x08, "LGui"),
    (0x10, "RCtrl"),
    (0x20, "RShift"),
    (0x40, "RAlt"),
    (0x80, "RGui"),
];

/// "LCtrl+LShift+A" — the held-keys combo joined by "+": modifiers
/// first (`MODIFIER_NAMES` order), the rest by ascending usage.
/// An empty set renders as an empty string.
pub fn combo(keys: &FixedBitSet) -> String {
    let mut parts: Vec<Cow<'static, str>> = Vec::new();
    for (i, (_, name)) in MODIFIER_NAMES.iter().enumerate() {
        if keys.contains(0xE0 + i) {
            parts.push(Cow::Borrowed(name));
        }
    }
    for usage in keys.ones() {
        if !(0xE0..0xE8).contains(&usage) {
            parts.push(key_name(usage as u32));
        }
    }
    parts.join("+")
}

/// The modifier usage (0xE0..=0xE7) for a name: full names (`LCtrl`)
/// and short aliases (`Ctrl` — the left one). Non-modifiers — None.
pub fn modifier_usage(name: &str) -> Option<u32> {
    if let Some(i) = MODIFIER_NAMES.iter().position(|(_, n)| *n == name) {
        return Some(0xE0 + i as u32);
    }
    match name {
        "Ctrl" => Some(0xE0),
        "Shift" => Some(0xE1),
        "Alt" => Some(0xE2),
        "Gui" => Some(0xE3),
        _ => None,
    }
}

/// Key name for combo rendering: a known usage renders canonically,
/// anything else as "0xNN". Allocates only for unknown codes.
fn key_name(keycode: u32) -> Cow<'static, str> {
    match KEY_NAMES.get(keycode as usize).copied().flatten() {
        Some(name) => Cow::Borrowed(name),
        None => Cow::Owned(format!("0x{keycode:02X}")),
    }
}

/// Reverse of `keycode_to_str`: the HID usage for a canonical name, exact
/// case, no folding ("1" -> 0x1E, "F21" -> 0x70). Anything `keycode_to_str`
/// would not produce ("a", "0x1E") is rejected.
pub fn keycode_from_str(name: &str) -> Option<u32> {
    KEY_NAMES
        .iter()
        .position(|entry| *entry == Some(name))
        .map(|usage| usage as u32)
}

const KEY_USAGES: usize = 0x100;

const X: Option<&str> = None;
// Keyboard/keyboard page (0x07) usages 0x00..=0xE7, one entry per usage:
// canonical HID name with the doc's abbreviations ("Keyboard " prefix
// dropped, "a and A" forms collapsed to the uppercase letter). None for
// reserved/error usages — they render as "0xNN".
const KEY_NAMES: [Option<&str>; 0xE8] = [
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
const _: () = assert!(KEY_NAMES.len() == 0xE8);

fn egui_key_usage(key: egui::Key) -> Option<u32> {
    use egui::Key as K;
    Some(match key {
        K::A => 0x04,
        K::B => 0x05,
        K::C => 0x06,
        K::D => 0x07,
        K::E => 0x08,
        K::F => 0x09,
        K::G => 0x0A,
        K::H => 0x0B,
        K::I => 0x0C,
        K::J => 0x0D,
        K::K => 0x0E,
        K::L => 0x0F,
        K::M => 0x10,
        K::N => 0x11,
        K::O => 0x12,
        K::P => 0x13,
        K::Q => 0x14,
        K::R => 0x15,
        K::S => 0x16,
        K::T => 0x17,
        K::U => 0x18,
        K::V => 0x19,
        K::W => 0x1A,
        K::X => 0x1B,
        K::Y => 0x1C,
        K::Z => 0x1D,
        K::Num1 => 0x1E,
        K::Num2 => 0x1F,
        K::Num3 => 0x20,
        K::Num4 => 0x21,
        K::Num5 => 0x22,
        K::Num6 => 0x23,
        K::Num7 => 0x24,
        K::Num8 => 0x25,
        K::Num9 => 0x26,
        K::Num0 => 0x27,
        K::Enter => 0x28,
        K::Escape => 0x29,
        K::Backspace => 0x2A,
        K::Tab => 0x2B,
        K::Space => 0x2C,
        K::F1 => 0x3A,
        K::F2 => 0x3B,
        K::F3 => 0x3C,
        K::F4 => 0x3D,
        K::F5 => 0x3E,
        K::F6 => 0x3F,
        K::F7 => 0x40,
        K::F8 => 0x41,
        K::F9 => 0x42,
        K::F10 => 0x43,
        K::F11 => 0x44,
        K::F12 => 0x45,
        K::F13 => 0x68,
        K::F14 => 0x69,
        K::F15 => 0x6A,
        K::F16 => 0x6B,
        K::F17 => 0x6C,
        K::F18 => 0x6D,
        K::F19 => 0x6E,
        K::F20 => 0x6F,
        K::F21 => 0x70,
        K::F22 => 0x71,
        K::F23 => 0x72,
        K::F24 => 0x73,
        K::Insert => 0x49,
        K::Home => 0x4A,
        K::PageUp => 0x4B,
        K::Delete => 0x4C,
        K::End => 0x4D,
        K::PageDown => 0x4E,
        K::ArrowRight => 0x4F,
        K::ArrowLeft => 0x50,
        K::ArrowDown => 0x51,
        K::ArrowUp => 0x52,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_names_use_hid_names_with_doc_abbreviations() {
        let set = key_set;
        // "Keyboard a and A" collapsed, doc abbreviations ("UpArrow").
        assert_eq!(combo(&set(&[0x70])), "F21");
        assert_eq!(combo(&set(&[0x04])), "A");
        assert_eq!(combo(&set(&[0x52])), "UpArrow");
        assert_eq!(combo(&set(&[0xE0])), "LCtrl"); // короткие имена модификаторов
        assert_eq!(combo(&set(&[0x2C])), "Spacebar");
        // Unknown / reserved usages render as 0xNN.
        assert_eq!(combo(&set(&[0x00])), "0x00");
        assert_eq!(combo(&set(&[0xA5])), "0xA5");
        assert_eq!(combo(&set(&[0xE8])), "0xE8");
    }

    #[test]
    fn key_usage_reverses_key_name() {
        assert_eq!(keycode_from_str("1"), Some(0x1E));
        assert_eq!(keycode_from_str("F21"), Some(0x70));
        assert_eq!(keycode_from_str("UpArrow"), Some(0x52));
        assert_eq!(keycode_from_str("LeftControl"), Some(0xE0));
        // Round trip over the whole table.
        for usage in 0..=0xE7u32 {
            if let Some(name) = KEY_NAMES[usage as usize] {
                assert_eq!(keycode_from_str(name), Some(usage));
            }
        }
        // Non-canonical spellings are rejected: no folding, no 0xNN.
        assert_eq!(keycode_from_str("a"), None);
        assert_eq!(keycode_from_str("f21"), None);
        assert_eq!(keycode_from_str("0x1E"), None);
        assert_eq!(keycode_from_str("nope"), None);
    }

    #[test]
    fn combo_renders_modifiers_first_then_keys() {
        let set = key_set;
        assert_eq!(combo(&set(&[])), "");
        assert_eq!(combo(&set(&[0x04])), "A");
        assert_eq!(combo(&set(&[0xE0, 0x04])), "LCtrl+A");
        assert_eq!(combo(&set(&[0xE1, 0xE0, 0x04])), "LCtrl+LShift+A");
        assert_eq!(combo(&set(&[0x68, 0x69])), "F13+F14");
        assert_eq!(combo(&set(&[0xE7, 0x04])), "RGui+A"); // модификаторы всегда первыми
        assert_eq!(combo(&set(&[0xA5])), "0xA5");
    }

    #[test]
    fn modifier_usage_parses_full_and_short_names() {
        assert_eq!(modifier_usage("LCtrl"), Some(0xE0));
        assert_eq!(modifier_usage("RGui"), Some(0xE7));
        assert_eq!(modifier_usage("Ctrl"), Some(0xE0));
        assert_eq!(modifier_usage("Shift"), Some(0xE1));
        assert_eq!(modifier_usage("A"), None);
    }
}
