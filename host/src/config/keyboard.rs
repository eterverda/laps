//! Keyboard identity filter for setup.yaml (see `driver::keyboard` for
//! the device side). All identification fields are optional and AND-ed;
//! an empty filter matches nothing, so a missing filter can never attach
//! every keyboard (fail-closed).

/// How the driver attaches the device. Grab is exclusive (the system
/// and other apps see nothing); listen is passive. Serialized lowercase
/// (`mode: grab`). Required — a missing mode is an error, never a
/// default: no silent grab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyboardMode {
    Listen,
    Grab,
}

impl Default for KeyboardMode {
    fn default() -> Self {
        KeyboardMode::Listen
    }
}

/// Selector for one configurable keyboard. The map key in
/// `Setup.keyboards` is the keyboard id delivered in `KeyEvent`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct KeyboardConfig {
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
    /// Attach mode, required.
    pub mode: KeyboardMode,
}

impl KeyboardConfig {
    /// Mirror of `CameraConfig::matches`: name is a case-insensitive
    /// substring, vid/pid/serial are exact when set. Requires at least
    /// one criterion — an empty config matches nothing.
    pub fn matches(&self, info: &crate::driver::keyboard::KeyboardInfo) -> bool {
        let has_criterion = self.name.is_some()
            || self.vid.is_some()
            || self.pid.is_some()
            || self.serial.is_some();
        if !has_criterion {
            return false;
        }
        let name_ok = self.name.as_ref().map_or(true, |n| {
            info.name.to_lowercase().contains(&n.to_lowercase())
        });
        let vid_ok = self.vid.map_or(true, |v| v == info.vid);
        let pid_ok = self.pid.map_or(true, |p| p == info.pid);
        let serial_ok = self
            .serial
            .as_ref()
            .map_or(true, |s| info.serial.as_deref() == Some(s.as_str()));
        name_ok && vid_ok && pid_ok && serial_ok
    }
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
fn de_opt_hex<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u16>, D::Error> {
    struct Hex;
    impl<'de> serde::de::Visitor<'de> for Hex {
        type Value = u16;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
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

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
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

fn ser_hex<S: serde::Serializer>(v: &Option<u16>, s: S) -> Result<S::Ok, S::Error> {
    match v {
        Some(v) => s.serialize_some(&format!("0x{v:04x}")),
        None => s.serialize_none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize() {
        let kb: KeyboardConfig = serde_yaml::from_str(
            "name: RED\nvid: 0x8089\npid: 0x000c\nserial: 03CDAB114071BC4FA8FFFFFFFF00\nmode: grab\n",
        )
        .unwrap();
        assert_eq!(kb.name.as_deref(), Some("RED"));
        assert_eq!(kb.vid, Some(0x8089));
        assert_eq!(kb.pid, Some(0x000c));
        assert_eq!(kb.serial.as_deref(), Some("03CDAB114071BC4FA8FFFFFFFF00"));
        assert_eq!(kb.mode, KeyboardMode::Grab);
        let kb: KeyboardConfig = serde_yaml::from_str("name: RED\nmode: listen\n").unwrap();
        assert_eq!(kb.mode, KeyboardMode::Listen);
    }

    #[test]
    fn test_serialize_hex_is_0x_prefixed() {
        let kb: KeyboardConfig = serde_yaml::from_str("vid: 0x8089\nmode: listen\n").unwrap();
        let yaml = serde_yaml::to_string(&kb).unwrap();
        // The exact spelling of the hex scalar on output is the
        // emitter's call (it may quote the string to keep round-trip
        // type-stable); the 0x prefix is ours.
        assert!(yaml.contains("0x8089"), "{yaml}");
    }

    #[test]
    fn test_quoted_hex_round_trips() {
        // Quoted input is accepted: serde_yaml quotes 0x-looking strings
        // on output to keep types stable, and our own serialization must
        // parse back.
        let kb: KeyboardConfig = serde_yaml::from_str("vid: \"0x8089\"\nmode: listen\n").unwrap();
        assert_eq!(kb.vid, Some(0x8089));
        // Non-0x strings are errors.
        assert!(serde_yaml::from_str::<KeyboardConfig>("vid: \"000c\"\nmode: listen\n").is_err());
    }

    #[test]
    fn test_bare_number_is_decimal() {
        // A bare scalar arrives as an already-parsed integer; without
        // the 0x prefix there is no hex to speak of.
        let kb: KeyboardConfig = serde_yaml::from_str("vid: 8089\nmode: listen\n").unwrap();
        assert_eq!(kb.vid, Some(8089));
    }

    #[test]
    fn test_deserialize_empty() {
        let kb: KeyboardConfig = serde_yaml::from_str("name: RED\nmode: listen\n").unwrap();
        assert_eq!(kb.vid, None);
    }

    #[test]
    fn test_mode_is_required() {
        // No silent default from the file side: a keyboard entry without
        // `mode` is an error (the enum Default exists only for
        // programmatic `KeyboardConfig::default()`, and it is Listen).
        assert!(serde_yaml::from_str::<KeyboardConfig>("name: RED\n").is_err());
    }

    #[test]
    fn test_deserialize_bad_hex_fails() {
        assert!(serde_yaml::from_str::<KeyboardConfig>("vid: zz\n").is_err());
    }

    mod matching {
        use super::*;
        use crate::driver::keyboard::KeyboardInfo;

        fn info(name: &str, vid: u16, pid: u16, serial: Option<&str>) -> KeyboardInfo {
            KeyboardInfo {
                name: name.to_owned(),
                vid,
                pid,
                serial: serial.map(str::to_owned),
                listenable: true,
                grabbable: true,
            }
        }

        #[test]
        fn name_is_case_insensitive_substring() {
            let cfg: KeyboardConfig = serde_yaml::from_str("name: red\nmode: listen\n").unwrap();
            assert!(cfg.matches(&info("SayoDevice RED", 0x8089, 0x000c, None)));
            assert!(cfg.matches(&info("sayoDevice red", 0x8089, 0x000c, None)));
            assert!(!cfg.matches(&info("SayoDevice BLUE", 0x8089, 0x000c, None)));
        }

        #[test]
        fn exact_fields_when_set() {
            let cfg: KeyboardConfig =
                serde_yaml::from_str("vid: 0x8089\npid: 0x000c\nmode: grab\n").unwrap();
            assert!(cfg.matches(&info("SayoDevice RED", 0x8089, 0x000c, None)));
            assert!(cfg.matches(&info("SayoDevice BLUE", 0x8089, 0x000c, None)));
            assert!(!cfg.matches(&info("SayoDevice 2x3B", 0x8089, 0x0008, None)));
            // Serial distinguishes two keyboards with the same vid:pid.
            let cfg: KeyboardConfig = serde_yaml::from_str(
                "vid: 0x8089\nserial: 03CDAB114071BC4FA8FFFFFFFF00\nmode: listen\n",
            )
            .unwrap();
            assert!(cfg.matches(&info(
                "SayoDevice RED",
                0x8089,
                0x000c,
                Some("03CDAB114071BC4FA8FFFFFFFF00")
            )));
            assert!(!cfg.matches(&info(
                "SayoDevice BLUE",
                0x8089,
                0x000c,
                Some("03CDABE24071BC20A9FFFFFFFF00")
            )));
            // Device without serial never matches a serial criterion.
            assert!(!cfg.matches(&info("Some Keyboard", 0x8089, 0x000c, None)));
        }

        #[test]
        fn empty_config_matches_nothing() {
            let cfg = KeyboardConfig::default();
            assert!(!cfg.matches(&info("SayoDevice RED", 0x8089, 0x000c, Some("x"))));
        }
    }
}
