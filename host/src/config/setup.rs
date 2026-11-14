use indexmap::IndexMap;
use serde::Deserialize;

use crate::config::camera::{CameraConfig, ViewportConfig};
#[cfg(target_os = "linux")]
use crate::config::keyboard::ExclusiveKeyboardConfig;
use crate::config::keyboard::KeyBind;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Setup {
    pub cameras: IndexMap<String, CameraConfig>,
    pub pads: IndexMap<String, PadConfig>,
    /// Клавиатурные биндинги (уровень сетапа, не пада): каждый несёт
    /// `pad-id` — над каким падом сигнал; без него сигнал уходит
    /// активному паду.
    #[serde(default)]
    pub key_binds: Vec<KeyBind>,
    // Аттачатся гоночной сессией (GUI → Race); парсинг здесь, чтобы
    // сетап-файлы уже могли описывать клавиатуры.
    #[serde(default)]
    #[cfg(target_os = "linux")]
    pub exclusive_keyboards: IndexMap<String, ExclusiveKeyboardConfig>,
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct PadConfig {
    pub label: String,
    pub color: ColorConfig,
    pub fpv: FpvConfig,
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct FpvConfig {
    pub camera_id: String,
    #[serde(default)]
    pub viewport: ViewportConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ColorConfig {
    Red,
    Blue,
    Green,
    Yellow,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_yaml::from_str as from_yaml_string;

    const CAMERAS: &str = "cameras:\n  camera-1:\n    name: C7-1\n    resolution: 1920x1080\n    frame-rate: 30fps\n    format: mjpeg\n";
    const PADS: &str =
        "pads:\n  pad-1:\n    label: CH1\n    color: red\n    fpv:\n      camera-id: camera-1\n";

    #[test]
    fn test_viewport_defaults_to_full_frame() {
        let fpv: FpvConfig = from_yaml_string("camera-id: camera-1\n").unwrap();
        assert_eq!(
            fpv.viewport,
            ViewportConfig {
                left: 0.0,
                top: 0.0,
                right: 1.0,
                bottom: 1.0,
            }
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_keyboards_default_to_empty() {
        // No exclusive keyboards: block — the map defaults to empty.
        let setup: Setup = from_yaml_string(&format!("{CAMERAS}{PADS}")).unwrap();
        assert!(setup.exclusive_keyboards.is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_keyboards_load() {
        let setup: Setup = from_yaml_string(&format!(
            "{CAMERAS}{PADS}exclusive-keyboards:\n  pad-1:\n    name: RED\n    vid: 0x8089\n"
        ))
        .unwrap();
        assert_eq!(setup.exclusive_keyboards.len(), 1);
        assert_eq!(
            setup.exclusive_keyboards["pad-1"].name.as_deref(),
            Some("RED")
        );
    }

    #[test]
    fn test_setup_key_binds() {
        // keybinds parse: key as a yaml number resolves to the usage;
        // уровень сетапа, пад — полем биндинга.
        let setup: Setup = from_yaml_string(
            "cameras: {}\npads: {}\nkey-binds:\n  - exclusive-keyboard-id: key-1\n    pad-id: pad-1\n    keys: 1\n    action: lap\n",
        )
        .unwrap();
        assert_eq!(setup.key_binds.len(), 1);
        assert_eq!(
            setup.key_binds[0].keys,
            crate::driver::keyboard::key_set(&[0x1E])
        );
        assert_eq!(setup.key_binds[0].pad_id.as_deref(), Some("pad-1"));
        #[cfg(target_os = "linux")]
        assert_eq!(
            setup.key_binds[0].exclusive_keyboard_id.as_deref(),
            Some("key-1")
        );
        // Биндинг без пада — легален (сигнал уйдёт активному паду).
        let setup: Setup = from_yaml_string(
            "cameras: {}\npads: {}\nkey-binds:\n  - keys: Spacebar\n    action: lap\n",
        )
        .unwrap();
        assert_eq!(setup.key_binds[0].pad_id, None);
        // Отсутствие блока — пустой список, не ошибка.
        let setup: Setup = from_yaml_string("cameras: {}\npads: {}\n").unwrap();
        assert!(setup.key_binds.is_empty());
        // Неизвестное имя клавиши — ошибка конфига.
        assert!(from_yaml_string::<Setup>(
            "cameras: {}\npads: {}\nkey-binds:\n  - exclusive-keyboard-id: key-1\n    key: banana\n    action: lap\n"
        )
        .is_err());
    }
}
