//! Keyboard config for setup.yaml: pad keybinds — cross-platform, a key
//! (resolved HID usage) bound to a pad action — and, Linux-only, the
//! exclusive keyboard identity filter (`ExclusiveKeyboardConfig`) for the device backend
//! (device backend of `driver::keyboard`). Attach is always grab (exclusive,
//! EVIOCGRAB) — the mode setting does not exist.

use crate::driver::keyboard::{KeyPressed, combo, key_set, keycode_from_str, modifier_usage};
use fixedbitset::FixedBitSet;
use std::fmt::{Formatter, Result as FmtResult};

/// One pad keybind: a combo of keys (`Ctrl+A`, `1`, `F13+F14`) bound
/// to a pad action. `keys` — a set of HID usages; parsed from a "+"
/// string: universal modifier names (`Ctrl`/`Shift`/`Alt`/`Gui`), keys
/// by canonical names. Matching is by subset: the event matches when
/// every binding key is held.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct KeyBind {
    /// Exclusive keyboard id: a key of `Setup.exclusive_keyboards`.
    /// None (the field absent in yaml) — a binding of the GUI focus
    /// (main) keyboard. Linux-only.
    #[cfg(target_os = "linux")]
    #[serde(
        default,
        rename = "exclusive-keyboard-id",
        skip_serializing_if = "Option::is_none"
    )]
    pub exclusive_keyboard_id: Option<String>,
    /// The held-keys combo: "Ctrl+A", "1", "F13+F14".
    #[serde(deserialize_with = "de_keys", serialize_with = "ser_keys")]
    pub keys: FixedBitSet,
    /// Pad action ("lap", …): сматчившийся биндинг логируется с ним
    /// (Idle и Live-дрены); исполнение действий — следующим шагом.
    pub action: String,
}

impl KeyBind {
    /// true when the binding matches the event: every binding key is
    /// held and (Linux) exclusive_keyboard_id equals exactly — None
    /// with None (main), Some(id) with the same Some(id).
    pub fn matches(&self, event: &KeyPressed) -> bool {
        #[cfg(target_os = "linux")]
        if self.exclusive_keyboard_id != event.exclusive_keyboard_id {
            return false;
        }
        self.keys.is_subset(&event.keys)
    }
}

/// `keys:` — a "+"-joined combo: each token is a universal modifier
/// (`Ctrl`/`Shift`/`Alt`/`Gui` — sided names are rejected) or a
/// canonical key name
/// (`keycode_from_str`); a bare yaml number reads as the digit-key
/// names. Any unknown token is a parse error.
fn de_keys<'de, D: serde::Deserializer<'de>>(d: D) -> Result<FixedBitSet, D::Error> {
    struct Keys;
    impl<'de> serde::de::Visitor<'de> for Keys {
        type Value = FixedBitSet;

        fn expecting(&self, f: &mut Formatter) -> FmtResult {
            f.write_str("key combo (e.g. Ctrl+A, F13+F14) or number")
        }

        fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
            let mut set = FixedBitSet::with_capacity(crate::driver::keyboard::KEY_USAGES);
            for token in v.split('+').map(str::trim) {
                let usage = modifier_usage(token)
                    .or_else(|| keycode_from_str(token))
                    .ok_or_else(|| E::custom(format!("unknown key: {token}")))?;
                set.insert(usage as usize);
            }
            Ok(set)
        }

        fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
            let name = v.to_string();
            keycode_from_str(&name)
                .map(|usage| key_set(&[usage]))
                .ok_or_else(|| E::custom(format!("unknown key number: {v}")))
        }
    }

    d.deserialize_any(Keys)
}

fn ser_keys<S: serde::Serializer>(keys: &FixedBitSet, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&combo(keys))
}

/// vid/pid are 0x-prefixed hex scalars in setup files (`vid: 0x8089`):
/// the yaml parser decodes the hex into an integer, so `visit_u64` takes
/// it as is — a bare number also arrives here, read as decimal (there
/// is no way to tell `8089` from a hex spelling after parsing, so the
/// 0x prefix is the only reliable hex form). A quoted string is parsed
/// with a mandatory 0x prefix — quoted is accepted because serde_yaml
/// quotes such strings on serialization to keep types stable, and
/// rejecting them would break our own round trip. Non-0x strings
/// (`"000c"`) are errors.
#[cfg(target_os = "linux")]
fn de_opt_hex<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u16>, D::Error> {
    struct Hex;
    impl<'de> serde::de::Visitor<'de> for Hex {
        type Value = u16;

        fn expecting(&self, f: &mut Formatter) -> FmtResult {
            f.write_str("0x-prefixed hex id")
        }

        fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<u16, E> {
            u16::try_from(v).map_err(|_| E::custom("hex id out of u16 range"))
        }

        fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<u16, E> {
            let digits = v
                .trim()
                .strip_prefix("0x")
                .ok_or_else(|| E::custom("hex id needs a 0x prefix"))?;
            u16::from_str_radix(digits, 16).map_err(E::custom)
        }
    }

    struct OptHex;
    impl<'de> serde::de::Visitor<'de> for OptHex {
        type Value = Option<u16>;

        fn expecting(&self, f: &mut Formatter) -> FmtResult {
            f.write_str("optional 0x-prefixed hex id")
        }

        fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_some<D: serde::Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
            d.deserialize_any(Hex).map(Some)
        }
    }

    d.deserialize_option(OptHex)
}

#[cfg(target_os = "linux")]
fn ser_hex<S: serde::Serializer>(v: &Option<u16>, s: S) -> Result<S::Ok, S::Error> {
    match v {
        Some(v) => s.serialize_some(&format!("0x{v:04x}")),
        None => s.serialize_none(),
    }
}

/// Selector for one configurable keyboard. The map key in
/// `Setup.exclusive_keyboards` is the keyboard id delivered in `KeyPressed`.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct ExclusiveKeyboardConfig {
    /// Substring of the evdev name, case-insensitive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// USB id as a 0x-prefixed hex scalar (`vid: 0x8089`); quoted is
    /// accepted too — see `de_opt_hex`.
    #[serde(
        default,
        deserialize_with = "de_opt_hex",
        serialize_with = "ser_hex",
        skip_serializing_if = "Option::is_none"
    )]
    pub vid: Option<u16>,
    /// Product id, same 0x-hex rule as `vid` (`pid: 0x000c`).
    #[serde(
        default,
        deserialize_with = "de_opt_hex",
        serialize_with = "ser_hex",
        skip_serializing_if = "Option::is_none"
    )]
    pub pid: Option<u16>,
    /// Serial (sysfs `uniq`), exact match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_yaml::from_str as from_yaml_string;
    use serde_yaml::to_string as to_yaml_string;

    #[cfg(target_os = "linux")]
    #[test]
    fn test_deserialize() {
        #[cfg(target_os = "linux")]
        {
            let kb: ExclusiveKeyboardConfig = from_yaml_string(
                "name: RED\nvid: 0x8089\npid: 0x000c\nserial: 03CDAB114071BC4FA8FFFFFFFF00\n",
            )
            .unwrap();
            assert_eq!(kb.name.as_deref(), Some("RED"));
            assert_eq!(kb.vid, Some(0x8089));
            assert_eq!(kb.pid, Some(0x000c));
            assert_eq!(kb.serial.as_deref(), Some("03CDAB114071BC4FA8FFFFFFFF00"));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_serialize_hex_is_0x_prefixed() {
        let kb: ExclusiveKeyboardConfig = from_yaml_string("vid: 0x8089\n").unwrap();
        let yaml = to_yaml_string(&kb).unwrap();
        // The exact spelling of the hex scalar on output is the
        // emitter's call (it may quote the string to keep round-trip
        // type-stable); the 0x prefix is ours.
        assert!(yaml.contains("0x8089"), "{yaml}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_quoted_hex_round_trips() {
        // Quoted input is accepted: serde_yaml quotes 0x-looking strings
        // on output to keep types stable, and our own serialization must
        // parse back.
        let kb: ExclusiveKeyboardConfig = from_yaml_string("vid: \"0x8089\"\n").unwrap();
        assert_eq!(kb.vid, Some(0x8089));
        // Non-0x strings are errors.
        assert!(from_yaml_string::<ExclusiveKeyboardConfig>("vid: \"000c\"\n").is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_bare_number_is_decimal() {
        // A bare scalar arrives as an already-parsed integer; without
        // the 0x prefix there is no hex to speak of.
        let kb: ExclusiveKeyboardConfig = from_yaml_string("vid: 8089\n").unwrap();
        assert_eq!(kb.vid, Some(8089));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_deserialize_empty() {
        let kb: ExclusiveKeyboardConfig = from_yaml_string("name: RED\n").unwrap();
        assert_eq!(kb.vid, None);
    }

    #[cfg(target_os = "linux")]
    #[cfg(target_os = "linux")]
    #[test]
    fn test_deserialize_bad_hex_fails() {
        assert!(from_yaml_string::<ExclusiveKeyboardConfig>("vid: zz\n").is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_keybind_key_forms() {
        // Комбо: короткий модификатор + клавиша.
        let bind: KeyBind = from_yaml_string(
            "exclusive-keyboard-id: key-1
keys: Ctrl+A
action: lap
",
        )
        .unwrap();
        assert_eq!(bind.keys, key_set(&[0xE0, 0x04]));
        assert_eq!(bind.exclusive_keyboard_id.as_deref(), Some("key-1"));
        // Универсальные имена модификаторов.
        let bind: KeyBind = from_yaml_string(
            "keys: Ctrl+Shift
action: lap
",
        )
        .unwrap();
        assert_eq!(bind.keys, key_set(&[0xE0, 0xE1]));
        // Bare yaml number — read as the number-key name.
        let bind: KeyBind = from_yaml_string(
            "keys: 1
action: lap
",
        )
        .unwrap();
        assert_eq!(bind.keys, key_set(&[0x1E]));
        // Несколько F-клавиш.
        let bind: KeyBind = from_yaml_string(
            "keys: F13+F14
action: lap
",
        )
        .unwrap();
        assert_eq!(bind.keys, key_set(&[0x68, 0x69]));
        // Сереализация пишет комбо и round-trips.
        let yaml = to_yaml_string(&bind).unwrap();
        let back: KeyBind = from_yaml_string(&yaml).unwrap();
        assert_eq!(back, bind);
    }

    #[test]
    fn test_keybind_unknown_key_fails() {
        for keys in ["0x1E", "banana", "999", "f21", "Ctrl+"] {
            let yaml = format!(
                "keys: {keys}
action: lap
"
            );
            assert!(
                from_yaml_string::<KeyBind>(&yaml).is_err(),
                "keys {keys} must be rejected"
            );
        }
    }

    /// Cross-platform: the subject is the keys only. An id-less
    /// (main) binding against main-keyboard events.
    #[test]
    fn keybind_matches_is_subset() {
        use crate::driver::keyboard::KeyPressed;
        use std::time::Instant;
        let bind: KeyBind = from_yaml_string("keys: Ctrl+A\naction: lap\n").unwrap();
        let event = |keys: &[u32]| KeyPressed {
            #[cfg(target_os = "linux")]
            exclusive_keyboard_id: None,
            keys: key_set(keys),
            at: Instant::now(),
        };
        assert!(bind.matches(&event(&[0xE0, 0x04]))); // exactly the binding
        assert!(bind.matches(&event(&[0xE0, 0xE1, 0x04]))); // a wider set is fine
        assert!(!bind.matches(&event(&[0x04]))); // Ctrl missing
        assert!(!bind.matches(&event(&[]))); // nothing held
    }

    /// Linux: the id matches by exact option equality — None with
    /// None, Some(id) with the same Some(id); a foreign id and a
    /// None/Some cross do not.
    #[cfg(target_os = "linux")]
    #[test]
    fn keybind_matches_by_exclusive_keyboard_id() {
        use crate::driver::keyboard::KeyPressed;
        use std::time::Instant;
        let event = |id: Option<&str>, keys: &[u32]| KeyPressed {
            exclusive_keyboard_id: id.map(str::to_owned),
            keys: key_set(keys),
            at: Instant::now(),
        };
        let pad: KeyBind =
            from_yaml_string("exclusive-keyboard-id: key-1\nkeys: A\naction: lap\n").unwrap();
        assert!(pad.matches(&event(Some("key-1"), &[0x04])));
        assert!(!pad.matches(&event(Some("key-2"), &[0x04]))); // a foreign keyboard
        assert!(!pad.matches(&event(None, &[0x04]))); // main misses the pad bind
        assert!(!pad.matches(&event(Some("key-1"), &[]))); // keys are required too

        let main_bind: KeyBind = from_yaml_string("keys: A\naction: lap\n").unwrap();
        assert!(main_bind.exclusive_keyboard_id.is_none());
        assert!(main_bind.matches(&event(None, &[0x04]))); // main on main
        assert!(!main_bind.matches(&event(Some("key-1"), &[0x04]))); // pad misses
    }

    #[cfg(target_os = "linux")]
    mod matching {
        use super::*;
        use crate::driver::keyboard::ExclusiveKeyboardInfo;

        fn info(name: &str, vid: u16, pid: u16, serial: Option<&str>) -> ExclusiveKeyboardInfo {
            ExclusiveKeyboardInfo {
                name: name.to_owned(),
                vid,
                pid,
                serial: serial.map(str::to_owned),
            }
        }

        #[test]
        fn name_is_case_insensitive_substring() {
            let cfg: ExclusiveKeyboardConfig = from_yaml_string("name: red\n").unwrap();
            assert!(info("SayoDevice RED", 0x8089, 0x000c, None).matches(&cfg));
            assert!(info("sayoDevice red", 0x8089, 0x000c, None).matches(&cfg));
            assert!(!info("SayoDevice BLUE", 0x8089, 0x000c, None).matches(&cfg));
        }

        #[test]
        fn exact_fields_when_set() {
            let cfg: ExclusiveKeyboardConfig =
                from_yaml_string("vid: 0x8089\npid: 0x000c\n").unwrap();
            assert!(info("SayoDevice RED", 0x8089, 0x000c, None).matches(&cfg));
            assert!(info("SayoDevice BLUE", 0x8089, 0x000c, None).matches(&cfg));
            assert!(!info("SayoDevice 2x3B", 0x8089, 0x0008, None).matches(&cfg));
            // Serial distinguishes two keyboards with the same vid:pid.
            let cfg: ExclusiveKeyboardConfig =
                from_yaml_string("vid: 0x8089\nserial: 03CDAB114071BC4FA8FFFFFFFF00\n").unwrap();
            assert!(
                info(
                    "SayoDevice RED",
                    0x8089,
                    0x000c,
                    Some("03CDAB114071BC4FA8FFFFFFFF00")
                )
                .matches(&cfg)
            );
            assert!(
                !info(
                    "SayoDevice BLUE",
                    0x8089,
                    0x000c,
                    Some("03CDABE24071BC20A9FFFFFFFF00")
                )
                .matches(&cfg)
            );
            // Device without serial never matches a serial criterion.
            assert!(!info("Some Keyboard", 0x8089, 0x000c, None).matches(&cfg));
        }

        #[test]
        fn empty_config_matches_nothing() {
            let cfg = ExclusiveKeyboardConfig::default();
            assert!(!info("SayoDevice RED", 0x8089, 0x000c, Some("x")).matches(&cfg));
        }
    }
}
