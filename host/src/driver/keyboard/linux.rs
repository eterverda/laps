//! Keyboard discovery over evdev/sysfs (Linux). A device is a keyboard
//! when it supplies key presses our driver can consume (EV_KEY): either
//! it declares a HID Keyboard collection (a `-event-kbd` by-id symlink
//! without `if01`) or it is the built-in serio (AT) keyboard with
//! keycodes in the keyboard range. Mice, touchpads, buttons and hotkey
//! handlers are excluded even if they carry a few key bits. No
//! dependencies beyond std::fs: identification reads
//! `/sys/class/input/eventN/device/` (name, id/vendor, id/product, uniq,
//! id/bustype, capabilities/key) plus by-id symlinks in /dev/input.
//! EVIOCGRAB is never issued here — access is probed
//! by a plain open/close of the node.

use super::KeyboardInfo;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Probe the node: open read, then write (no create/truncate), close
/// immediately. Returns (listenable, grabbable). EVIOCGRAB is never
/// issued.
pub fn probe(node: &Path) -> (bool, bool) {
    let listenable = fs::OpenOptions::new().read(true).open(node).is_ok();
    let grabbable = fs::OpenOptions::new().write(true).open(node).is_ok();
    (listenable, grabbable)
}

/// sysfs id/bustype value of the serio bus (built-in AT keyboard).
const BUS_SERIO: u16 = 0x0011;

/// Show decision for an event node: a qualifying `-event-kbd` by-id
/// symlink (HID Keyboard collection, `if01`-less) is sufficient on its
/// own — this is what lets USB pedals/macro pads in and keeps mice and
/// touchpads (mouse collections or no by-id) out. The fallback is the
/// serio path: the built-in AT keyboard has no by-id but keycodes in
/// the keyboard range.
fn is_keyboard_node(has_kbd_by_id: bool, bustype: u16, keyboard_keys: bool) -> bool {
    has_kbd_by_id || (bustype == BUS_SERIO && keyboard_keys)
}

/// Event node names (`eventN`) that have at least one by-id symlink
/// ending in `-event-kbd` without `if01`. Missing by-id dir means an
/// empty set (the serio fallback still covers the AT keyboard).
fn kbd_nodes_by_id() -> std::collections::HashSet<String> {
    let mut nodes = std::collections::HashSet::new();
    let Ok(entries) = fs::read_dir("/dev/input/by-id") else {
        return nodes;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(link) = name.to_str() else { continue };
        if !link.ends_with("-event-kbd") || link.contains("if01") {
            continue;
        }
        if let Ok(target) = fs::read_link(entry.path()) {
            if let Some(node) = target.file_name().and_then(|n| n.to_str()) {
                nodes.insert(node.to_owned());
            }
        }
    }
    nodes
}

/// sysfs capabilities/key is a hex bitmask, one u64 word (kernel
/// unsigned long) per 64 keycodes, least significant bit first. A
/// device counts as a keyboard if any bit in the keyboard keycode range
/// 1..=0xE7 (KEY_ESC..=KEY_MICMUTE) is set; keycode 0 (KEY_RESERVED)
/// does not count.
fn has_keyboard_keys(caps: &str) -> bool {
    caps.split_whitespace().enumerate().any(|(word, hex)| {
        let bits = u64::from_str_radix(hex, 16).unwrap_or(0);
        (0..64).any(|bit| {
            bits & (1 << bit) != 0 && {
                let code = word * 64 + bit;
                (1..=0xE7).contains(&code)
            }
        })
    })
}

/// Deterministic listing order: vid, pid, serial, name. Two keyboards
/// equal in all four are indistinguishable — no further ordering.
fn sort_key(k: &KeyboardInfo) -> (u16, u16, &Option<String>, &String) {
    (k.vid, k.pid, &k.serial, &k.name)
}

/// Enumerate keyboards: event nodes in /dev/input passing
/// `is_keyboard_node`, sorted by `sort_key`. Devices that vanish
/// mid-scan are skipped.
pub fn scan() -> io::Result<Vec<KeyboardInfo>> {
    let mut devices = Vec::new();
    let kbd_by_id = kbd_nodes_by_id();
    for entry in fs::read_dir("/dev/input")? {
        let Ok(entry) = entry else { continue };
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let sys = Path::new("/sys/class/input").join(&name).join("device");
        if let Some(device) = read_device(&sys, entry.path(), kbd_by_id.contains(&name)) {
            devices.push(device);
        }
    }
    devices.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
    Ok(devices)
}

/// One device: sysfs identity + node path. Returns None when the
/// mandatory reads fail or the node is not a keyboard by
/// `is_keyboard_node`.
fn read_device(sys: &Path, node: PathBuf, has_kbd_by_id: bool) -> Option<KeyboardInfo> {
    let name = fs::read_to_string(sys.join("name")).ok()?;
    let vid = parse_hex(&fs::read_to_string(sys.join("id/vendor")).ok()?)?;
    let pid = parse_hex(&fs::read_to_string(sys.join("id/product")).ok()?)?;
    let bustype = parse_hex(&fs::read_to_string(sys.join("id/bustype")).ok()?)?;
    let caps = fs::read_to_string(sys.join("capabilities/key")).ok()?;
    if !is_keyboard_node(has_kbd_by_id, bustype, has_keyboard_keys(&caps)) {
        return None;
    }
    let serial = fs::read_to_string(sys.join("uniq"))
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    let (listenable, grabbable) = probe(&node);
    Some(KeyboardInfo {
        name: name.trim().to_owned(),
        vid,
        pid,
        serial,
        listenable,
        grabbable,
    })
}

fn parse_hex(s: &str) -> Option<u16> {
    u16::from_str_radix(s.trim(), 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_range_detected() {
        // Real AT keyboard bitmap: keys across 1..=0xE7.
        let at = "402000007 ff803078f800d001 feffffdfffcfffff fffffffffffffffe";
        assert!(has_keyboard_keys(at));
        assert!(has_keyboard_keys("2")); // KEY_ESC (1) only
    }

    #[test]
    fn reserved_bit_alone_is_not_a_keyboard() {
        assert!(!has_keyboard_keys("0"));
        assert!(!has_keyboard_keys("1")); // keycode 0 (KEY_RESERVED)
        assert!(!has_keyboard_keys("0 0 0 0"));
    }

    #[test]
    fn empty_or_broken_caps_is_not_a_keyboard() {
        assert!(!has_keyboard_keys(""));
        assert!(!has_keyboard_keys("zzzz"));
    }

    #[test]
    fn bits_beyond_keyboard_range_do_not_count() {
        // Bit 256 only (word 4, bit 0) — above 0xE7.
        assert!(!has_keyboard_keys("0 0 0 0 1"));
    }

    #[test]
    fn show_decision() {
        // -event-kbd by-id is sufficient on its own (USB pedal/macro pad).
        assert!(is_keyboard_node(true, 0x0003, false));
        // Serio fallback: built-in AT keyboard has keycodes but no by-id.
        assert!(is_keyboard_node(false, BUS_SERIO, true));
        // Serio without keyboard keycodes: not a keyboard.
        assert!(!is_keyboard_node(false, BUS_SERIO, false));
        // USB without a keyboard by-id (mouse, touchpad): key bits alone
        // do not qualify.
        assert!(!is_keyboard_node(false, 0x0003, true));
        // Buttons/hotkeys on other buses (0x0019) are out as well.
        assert!(!is_keyboard_node(false, 0x0019, true));
    }

    #[test]
    fn sorting_is_by_identity_vid_pid_serial_name() {
        let dev = |vid: u16, pid: u16, serial: Option<&str>, name: &str| KeyboardInfo {
            name: name.to_owned(),
            vid,
            pid,
            serial: serial.map(str::to_owned),
            listenable: true,
            grabbable: true,
        };
        let mut devices = vec![
            dev(0x8089, 0x000c, None, "zz"),
            dev(0x8089, 0x000c, Some("b"), "aa"),
            dev(0x8089, 0x0008, None, "mm"),
            dev(0x0001, 0x0001, None, "aa"),
            dev(0x8089, 0x000c, Some("a"), "zz"),
        ];
        devices.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
        let keys: Vec<_> = devices
            .iter()
            .map(|d| (d.vid, d.pid, d.serial.as_deref(), d.name.as_str()))
            .collect();
        assert_eq!(
            keys,
            [
                (0x0001, 0x0001, None, "aa"),
                (0x8089, 0x0008, None, "mm"),
                (0x8089, 0x000c, None, "zz"), // serial: None < Some(_)
                (0x8089, 0x000c, Some("a"), "zz"),
                (0x8089, 0x000c, Some("b"), "aa"),
            ]
        );
    }
}
