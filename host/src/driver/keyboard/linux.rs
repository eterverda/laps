//! Keyboard driver (Linux): discovery and attach of external keyboards over
//! evdev/sysfs. A device is a keyboard when it supplies key presses our
//! driver can consume (EV_KEY): either it declares a HID Keyboard
//! collection (a `-event-kbd` by-id symlink without `if01`) or it is the
//! built-in serio (AT) keyboard with keycodes in the keyboard range.
//! Mice, touchpads, buttons and hotkey handlers are excluded even if
//! they carry a few key bits. No dependencies beyond std::fs:
//! identification reads `/sys/class/input/eventN/device/` (name,
//! id/vendor, id/product, uniq, id/bustype, capabilities/key) plus
//! by-id symlinks in /dev/input. EVIOCGRAB is never issued here —
//! access is probed by a plain open/close of the node.
//!
//! Device backend of the `keyboard` driver (the egui focus keyboard
//! lives in the parent module); shares `KeyPressed` with it.

use super::KeyPressed;
use crate::config::keyboard::ExclusiveKeyboardConfig;
use crossbeam_channel as crossbeam;
use crossbeam_utils::atomic::AtomicCell;
use std::collections::HashSet;
use std::ffi::CString;
use std::fmt::{Display, Formatter, Result as FmtResult};
use std::fs;
use std::io::Error as IoError;
use std::io::ErrorKind as IoErrorKind;
use std::io::Result as IoResult;
use std::mem;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

// EVIOCGRAB: 'E' 0x90, int argument (1 = grab, 0 = release). The grab
// is tied to the fd — closing it (thread end, process death) releases.
nix::ioctl_write_int!(eviocgrab, b'E', 0x90);

/// An exclusive keyboard: a grabbed (EVIOCGRAB) device streaming
/// presses into the shared channel from a reader thread. Owned by
/// the screen (like `Keyboard` in the GUI air): lives while the
/// screen does; dropping the handle stops the reader by disconnecting
/// the receiver.
pub struct ExclusiveKeyboard {
    /// Reader liveness.
    #[allow(dead_code)] // the keyboard-status screen will read this
    state: SharedExclusiveKeyboardState,
}

impl ExclusiveKeyboard {
    /// Discovered exclusive keyboards: the event nodes in
    /// /dev/input passing `is_keyboard_node`, sorted by `sort_key`.
    /// Devices vanishing mid-scan are skipped.
    pub fn list() -> IoResult<Vec<ExclusiveKeyboardInfo>> {
        Ok(scan_devices()?.into_iter().map(|d| d.info).collect())
    }

    /// Attach one configured keyboard: open the matched device and
    /// stream its events into `events` from a reader thread (one
    /// thread per device, lives while the device and the receiver
    /// do). EVIOCGRAB is released by the fd close in the reader.
    pub fn new(
        events: &crossbeam::Sender<KeyPressed>,
        id: &str,
        cfg: &ExclusiveKeyboardConfig,
    ) -> Result<Self, AttachError> {
        let state = attach(events.clone(), id, cfg)?;
        Ok(Self { state })
    }

    /// Connection state: connected while the reader thread is alive.
    #[allow(dead_code)] // the keyboard-status screen will read this
    pub fn state(&self) -> &SharedExclusiveKeyboardState {
        &self.state
    }
}

/// One device: sysfs identity + node path. Returns None when the
/// mandatory reads fail or the node is not a keyboard by
/// `is_keyboard_node`.
/// A discovered exclusive keyboard. Identity fields mirror the matcher
/// of `ExclusiveKeyboardConfig` (name substring, exact vid/pid/serial).
/// No Eq/Ord: matching a config entry is a filter, not equality, and
/// the only ordering need is the deterministic listing (a plain sort
/// key in `scan`).
#[derive(Debug, Clone)]
pub struct ExclusiveKeyboardInfo {
    pub name: String,
    pub vid: u16,
    pub pid: u16,
    pub serial: Option<String>,
}

impl ExclusiveKeyboardInfo {
    /// Mirror of `CameraConfig::matches` (inverted ownership: the
    /// candidate tests itself against the filter): name is a
    /// case-insensitive substring, vid/pid/serial are exact when set.
    /// Requires at least one criterion — an empty config matches
    /// nothing.
    pub fn matches(&self, cfg: &ExclusiveKeyboardConfig) -> bool {
        let has_criterion =
            cfg.name.is_some() || cfg.vid.is_some() || cfg.pid.is_some() || cfg.serial.is_some();
        if !has_criterion {
            return false;
        }
        let name_ok = cfg
            .name
            .as_ref()
            .is_none_or(|n| self.name.to_lowercase().contains(&n.to_lowercase()));
        let vid_ok = cfg.vid.is_none_or(|v| v == self.vid);
        let pid_ok = cfg.pid.is_none_or(|p| p == self.pid);
        let serial_ok = cfg
            .serial
            .as_ref()
            .is_none_or(|s| self.serial.as_deref() == Some(s.as_str()));
        name_ok && vid_ok && pid_ok && serial_ok
    }
}

/// Why attaching one configured keyboard failed. Per-device: one
/// keyboard failing does not affect the others.
#[derive(Debug)]
pub enum AttachError {
    /// The filter matched no discovered keyboard.
    NoMatch,
    /// The filter matched more than one; the list is the candidates.
    AmbiguousMatch(Vec<ExclusiveKeyboardInfo>),
    /// Open or EVIOCGRAB failed (permissions, busy, gone mid-way).
    /// Attach is grab-only: no fallback, a failure fails the keyboard.
    Io(IoError),
}

impl Display for AttachError {
    fn fmt(&self, f: &mut Formatter) -> FmtResult {
        match self {
            AttachError::NoMatch => write!(f, "no keyboard matches the filter"),
            AttachError::AmbiguousMatch(candidates) => {
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
            AttachError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for AttachError {}

/// Keyboard state: `connected` — the reader thread is alive (the
/// device is in place); cleared by the reader on exit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExclusiveKeyboardState {
    pub connected: bool,
}

impl Default for ExclusiveKeyboardState {
    fn default() -> Self {
        Self { connected: true }
    }
}

/// The keyboard state handle, shared by the reader and the GUI.
pub type SharedExclusiveKeyboardState = Arc<AtomicCell<ExclusiveKeyboardState>>;

/// Attach one configured keyboard: open the matched device and stream
/// its events into `events` from a reader thread (one thread per
/// device, lives while the device and the receiver do).
fn attach(
    events: crossbeam::Sender<KeyPressed>,
    id: &str,
    cfg: &ExclusiveKeyboardConfig,
) -> Result<SharedExclusiveKeyboardState, AttachError> {
    let devices = scan_devices().map_err(AttachError::Io)?;
    let matched: Vec<&Device> = devices.iter().filter(|d| d.info.matches(cfg)).collect();
    let device = match matched.len() {
        0 => return Err(AttachError::NoMatch),
        1 => matched[0],
        _ => {
            return Err(AttachError::AmbiguousMatch(
                matched.into_iter().map(|d| d.info.clone()).collect(),
            ));
        }
    };
    // Attach is always a grab: EVIOCGRAB needs the node opened
    // O_RDWR. A failed open/grab fails the keyboard — no fallback.
    let fd = open_node(&device.node, libc::O_RDWR).map_err(AttachError::Io)?;
    if unsafe { eviocgrab(fd, 1) }.is_err() {
        let err = IoError::last_os_error();
        unsafe { libc::close(fd) };
        return Err(AttachError::Io(err));
    }
    let exclusive_keyboard_id = id.to_owned();
    let state: SharedExclusiveKeyboardState = Arc::default();
    let thread_state = state.clone();
    thread::spawn(move || read_events(fd, exclusive_keyboard_id, events, thread_state));
    Ok(state)
}

fn open_node(path: &Path, flags: libc::c_int) -> IoResult<RawFd> {
    let cpath = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| IoError::new(IoErrorKind::InvalidInput, "node path with NUL"))?;
    let fd = unsafe { libc::open(cpath.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd == -1 {
        Err(IoError::last_os_error())
    } else {
        Ok(fd)
    }
}

/// Reader thread body: blocking reads on the node fd, decoded events
/// pushed into the channel. EINTR retries; any other read error
/// (ENODEV on unplug, EIO) ends the stream — the fd close also releases
/// the grab, and the dropped sender disconnects the receiver.
fn read_events(
    fd: RawFd,
    exclusive_keyboard_id: String,
    tx: crossbeam_channel::Sender<KeyPressed>,
    state: SharedExclusiveKeyboardState,
) {
    let mut pressed = fixedbitset::FixedBitSet::with_capacity(0x100);
    let mut buf = [0u8; INPUT_EVENT_SIZE * 64];
    let mut tail = 0usize;
    'read: loop {
        let n = unsafe { libc::read(fd, buf[tail..].as_mut_ptr().cast(), buf.len() - tail) };
        if n == 0 {
            break;
        }
        if n < 0 {
            let err = IoError::last_os_error();
            if err.kind() == IoErrorKind::Interrupted {
                continue;
            }
            log::debug!("keyboard {exclusive_keyboard_id}: read ended: {err}");
            break;
        }
        let filled = tail + n as usize;
        let complete = filled / INPUT_EVENT_SIZE * INPUT_EVENT_SIZE;
        for event in decode(&buf[..complete], &mut pressed, &exclusive_keyboard_id) {
            if tx.send(event).is_err() {
                break 'read; // receiver gone
            }
        }
        buf.copy_within(complete..filled, 0);
        tail = filled - complete;
    }
    unsafe { libc::close(fd) };
    // The reader ended — the device is gone (or the receiver left):
    // the state is cleared on every exit.
    state.store(ExclusiveKeyboardState { connected: false });
}

/// Decode raw `input_event` bytes (a multiple of 24) into KeyEvents.
/// Only EV_KEY is consumed: value 0/1 map to Release/Press, autorepeat
/// (value 2) is dropped, other event types (MSC_SCAN and friends) are
/// ignored. Unmapped codes are skipped with a warning. `pressed` —
/// the set of all held keys: a front inserts, a release removes;
/// each event carries a snapshot of the set.
fn decode(
    buf: &[u8],
    pressed: &mut fixedbitset::FixedBitSet,
    exclusive_keyboard_id: &str,
) -> Vec<KeyPressed> {
    debug_assert_eq!(buf.len() % INPUT_EVENT_SIZE, 0);
    let mut events = Vec::new();
    for chunk in buf.as_chunks::<INPUT_EVENT_SIZE>().0 {
        let kind = u16::from_le_bytes([chunk[16], chunk[17]]);
        if kind != EV_KEY {
            continue;
        }
        let code = u16::from_le_bytes([chunk[18], chunk[19]]);
        let value = i32::from_le_bytes([chunk[20], chunk[21], chunk[22], chunk[23]]);
        let press = match value {
            1 => true,
            0 => false,
            _ => continue, // autorepeat — filtered by the driver
        };
        let Some(usage) = EVDEV_TO_HID[code as usize] else {
            log::warn!(
                "keyboard {exclusive_keyboard_id}: unmapped evdev code {code}, event skipped"
            );
            continue;
        };
        if press {
            pressed.insert(usage as usize);
            events.push(KeyPressed {
                exclusive_keyboard_id: Some(exclusive_keyboard_id.to_owned()),
                keys: pressed.clone(),
                at: Instant::now(),
            });
        } else {
            pressed.remove(usage as usize);
        }
    }
    events
}

/// sysfs id/bustype value of the serio bus (built-in AT keyboard).
/// Deterministic listing order: vid, pid, serial, name. Two keyboards
/// equal in all four are indistinguishable — no further ordering.
fn sort_key(k: &ExclusiveKeyboardInfo) -> (u16, u16, &Option<String>, &String) {
    (k.vid, k.pid, &k.serial, &k.name)
}

/// Enumerate keyboard devices (info + node path), sorted by `sort_key`.
fn scan_devices() -> IoResult<Vec<Device>> {
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
    devices.sort_by(|a, b| sort_key(&a.info).cmp(&sort_key(&b.info)));
    Ok(devices)
}

/// A discovered keyboard plus its evdev node path. `ExclusiveKeyboardInfo` is
/// platform-neutral and deliberately carries no path; attach needs it.
struct Device {
    info: ExclusiveKeyboardInfo,
    node: PathBuf,
}

fn read_device(sys: &Path, node: PathBuf, has_kbd_by_id: bool) -> Option<Device> {
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
    Some(Device {
        info: ExclusiveKeyboardInfo {
            name: name.trim().to_owned(),
            vid,
            pid,
            serial,
        },
        node,
    })
}

fn parse_hex(s: &str) -> Option<u16> {
    u16::from_str_radix(s.trim(), 16).ok()
}

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
fn kbd_nodes_by_id() -> HashSet<String> {
    let mut nodes = HashSet::new();
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

/// Wire size of one `struct input_event` on x86_64 (16-byte timeval +
/// type/code/value). `decode` parses the bytes manually as little-endian;
/// the repr(C) mirror pins the layout assumption at compile time.
const INPUT_EVENT_SIZE: usize = 24;

#[repr(C)]
struct InputEventLayout {
    sec: i64,
    usec: i64,
    kind: u16,
    code: u16,
    value: i32,
}
const _: () = assert!(mem::size_of::<InputEventLayout>() == INPUT_EVENT_SIZE);

const EV_KEY: u16 = 1;

/// evdev code -> HID usage (page 0x07): inversion of the kernel's
/// `hid_keyboard[]` table (drivers/hid/hid-input.c), first HID usage
/// wins on duplicates. Codes the kernel maps to KEY_UNKNOWN (or nowhere)
/// are None and skipped with a warning.
const EVDEV_TO_HID: [Option<u8>; 256] = [
    None,
    Some(0x29),
    Some(0x1E),
    Some(0x1F),
    Some(0x20),
    Some(0x21),
    Some(0x22),
    Some(0x23),
    Some(0x24),
    Some(0x25),
    Some(0x26),
    Some(0x27),
    Some(0x2D),
    Some(0x2E),
    Some(0x2A),
    Some(0x2B),
    Some(0x14),
    Some(0x1A),
    Some(0x08),
    Some(0x15),
    Some(0x17),
    Some(0x1C),
    Some(0x18),
    Some(0x0C),
    Some(0x12),
    Some(0x13),
    Some(0x2F),
    Some(0x30),
    Some(0x28),
    Some(0xE0),
    Some(0x04),
    Some(0x16),
    Some(0x07),
    Some(0x09),
    Some(0x0A),
    Some(0x0B),
    Some(0x0D),
    Some(0x0E),
    Some(0x0F),
    Some(0x33),
    Some(0x34),
    Some(0x35),
    Some(0xE1),
    Some(0x31),
    Some(0x1D),
    Some(0x1B),
    Some(0x06),
    Some(0x19),
    Some(0x05),
    Some(0x11),
    Some(0x10),
    Some(0x36),
    Some(0x37),
    Some(0x38),
    Some(0xE5),
    Some(0x55),
    Some(0xE2),
    Some(0x2C),
    Some(0x39),
    Some(0x3A),
    Some(0x3B),
    Some(0x3C),
    Some(0x3D),
    Some(0x3E),
    Some(0x3F),
    Some(0x40),
    Some(0x41),
    Some(0x42),
    Some(0x43),
    Some(0x53),
    Some(0x47),
    Some(0x5F),
    Some(0x60),
    Some(0x61),
    Some(0x56),
    Some(0x5C),
    Some(0x5D),
    Some(0x5E),
    Some(0x57),
    Some(0x59),
    Some(0x5A),
    Some(0x5B),
    Some(0x62),
    Some(0x63),
    None,
    Some(0x94),
    Some(0x64),
    Some(0x44),
    Some(0x45),
    Some(0x87),
    Some(0x92),
    Some(0x93),
    Some(0x8A),
    Some(0x88),
    Some(0x8B),
    Some(0x8C),
    Some(0x58),
    Some(0xE4),
    Some(0x54),
    Some(0x46),
    Some(0xE6),
    None,
    Some(0x4A),
    Some(0x52),
    Some(0x4B),
    Some(0x50),
    Some(0x4F),
    Some(0x4D),
    Some(0x51),
    Some(0x4E),
    Some(0x49),
    Some(0x4C),
    None,
    Some(0x7F),
    Some(0x81),
    Some(0x80),
    Some(0x66),
    Some(0x67),
    None,
    Some(0x48),
    None,
    Some(0x85),
    Some(0x90),
    Some(0x91),
    Some(0x89),
    Some(0xE3),
    Some(0xE7),
    Some(0x65),
    Some(0x78),
    Some(0x79),
    Some(0x76),
    Some(0x7A),
    Some(0x77),
    Some(0x7C),
    Some(0x74),
    Some(0x7D),
    Some(0x7E),
    Some(0x7B),
    Some(0x75),
    None,
    Some(0xFB),
    None,
    Some(0xF8),
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    Some(0xF0),
    None,
    Some(0xF9),
    None,
    None,
    None,
    None,
    None,
    Some(0xF1),
    Some(0xF2),
    None,
    Some(0xEC),
    None,
    Some(0xEB),
    Some(0xE8),
    Some(0xEA),
    Some(0xE9),
    None,
    None,
    None,
    None,
    None,
    None,
    Some(0xFA),
    None,
    None,
    Some(0xF7),
    Some(0xF5),
    Some(0xF6),
    Some(0xB6),
    Some(0xB7),
    None,
    None,
    Some(0x68),
    Some(0x69),
    Some(0x6A),
    Some(0x6B),
    Some(0x6C),
    Some(0x6D),
    Some(0x6E),
    Some(0x6F),
    Some(0x70),
    Some(0x71),
    Some(0x72),
    Some(0x73),
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
];

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
        let dev = |vid: u16, pid: u16, serial: Option<&str>, name: &str| ExclusiveKeyboardInfo {
            name: name.to_owned(),
            vid,
            pid,
            serial: serial.map(str::to_owned),
        };
        let mut devices = [
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
    #[test]
    fn evdev_to_hid_checkpoints() {
        // Inversion of the kernel's hid_keyboard[] table.
        assert_eq!(EVDEV_TO_HID[30], Some(0x04)); // KEY_A
        assert_eq!(EVDEV_TO_HID[44], Some(0x1D)); // KEY_Z
        assert_eq!(EVDEV_TO_HID[2], Some(0x1E)); // KEY_1
        assert_eq!(EVDEV_TO_HID[11], Some(0x27)); // KEY_0
        assert_eq!(EVDEV_TO_HID[59], Some(0x3A)); // KEY_F1
        assert_eq!(EVDEV_TO_HID[88], Some(0x45)); // KEY_F12
        assert_eq!(EVDEV_TO_HID[191], Some(0x70)); // KEY_F21
        assert_eq!(EVDEV_TO_HID[194], Some(0x73)); // KEY_F24
        assert_eq!(EVDEV_TO_HID[105], Some(0x50)); // KEY_LEFT
        assert_eq!(EVDEV_TO_HID[106], Some(0x4F)); // KEY_RIGHT
        assert_eq!(EVDEV_TO_HID[103], Some(0x52)); // KEY_UP
        assert_eq!(EVDEV_TO_HID[108], Some(0x51)); // KEY_DOWN
        assert_eq!(EVDEV_TO_HID[1], Some(0x29)); // KEY_ESC
        assert_eq!(EVDEV_TO_HID[28], Some(0x28)); // KEY_ENTER
        assert_eq!(EVDEV_TO_HID[57], Some(0x2C)); // KEY_SPACE
        assert_eq!(EVDEV_TO_HID[14], Some(0x2A)); // KEY_BACKSPACE
        assert_eq!(EVDEV_TO_HID[15], Some(0x2B)); // KEY_TAB
        // Modifiers: LCtrl..RGui -> 0xE0..=0xE7.
        for (code, usage) in [
            (29, 0xE0),
            (42, 0xE1),
            (56, 0xE2),
            (125, 0xE3),
            (97, 0xE4),
            (54, 0xE5),
            (100, 0xE6),
            (126, 0xE7),
        ] {
            assert_eq!(EVDEV_TO_HID[code], Some(usage));
        }
        // KEY_RESERVED and KEY_UNKNOWN map to nothing.
        assert_eq!(EVDEV_TO_HID[0], None);
        assert_eq!(EVDEV_TO_HID[240], None);
    }

    /// One raw input_event, little-endian.
    fn raw_event(kind: u16, code: u16, value: i32) -> [u8; INPUT_EVENT_SIZE] {
        let mut b = [0u8; INPUT_EVENT_SIZE];
        b[16..18].copy_from_slice(&kind.to_le_bytes());
        b[18..20].copy_from_slice(&code.to_le_bytes());
        b[20..24].copy_from_slice(&value.to_le_bytes());
        b
    }

    #[test]
    fn decode_maps_and_tracks_pressed_set() {
        let mut pressed = fixedbitset::FixedBitSet::with_capacity(0x100);
        // Press LShift (evdev 42), press F21 (evdev 191), repeat F21,
        // release LShift, release F21.
        let mut buf = Vec::new();
        for ev in [
            raw_event(EV_KEY, 42, 1),
            raw_event(EV_KEY, 191, 1),
            raw_event(EV_KEY, 191, 2), // autorepeat: dropped
            raw_event(4, 4, 16),       // MSC_SCAN: ignored
            raw_event(EV_KEY, 42, 0),
            raw_event(EV_KEY, 191, 0),
        ] {
            buf.extend_from_slice(&ev);
        }
        let events = decode(&buf, &mut pressed, "key-1");
        // Релизы в канал не идут: только фронты, с снимком сета.
        let summary: Vec<String> = events
            .iter()
            .map(|e| super::super::combo(&e.keys))
            .collect();
        assert_eq!(summary, ["LShift", "LShift+F21"]);
        assert!(
            events
                .iter()
                .all(|e| e.exclusive_keyboard_id.as_deref() == Some("key-1"))
        );
        assert!(pressed.ones().next().is_none()); // всё отпущено
    }

    #[test]
    fn decode_skips_unmapped_codes() {
        let mut pressed = fixedbitset::FixedBitSet::with_capacity(0x100);
        let mut buf = Vec::new();
        buf.extend_from_slice(&raw_event(EV_KEY, 240, 1)); // KEY_UNKNOWN
        buf.extend_from_slice(&raw_event(EV_KEY, 0, 1)); // KEY_RESERVED
        assert!(decode(&buf, &mut pressed, "key-1").is_empty());
    }
}
