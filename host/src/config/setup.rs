use serde::Deserialize;
use std::collections::BTreeMap;

use crate::config::camera::{CameraConfig, ViewportConfig};
#[cfg(target_os = "linux")]
use crate::config::keyboard::ExclusiveKeyboardConfig;
use crate::config::keyboard::KeyBind;

#[derive(Debug, Clone, Deserialize)]
pub struct Setup {
    pub cameras: BTreeMap<String, CameraConfig>,
    pub pads: BTreeMap<String, PadConfig>,
    // Аттачатся гоночной сессией (GUI → Race); парсинг здесь, чтобы
    // сетап-файлы уже могли описывать клавиатуры.
    #[serde(default)]
    #[cfg(target_os = "linux")]
    pub exclusive_keyboards: BTreeMap<String, ExclusiveKeyboardConfig>,
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct PadConfig {
    pub label: String,
    pub color: ColorConfig,
    pub fpv: FpvConfig,
    /// Клавиши настроенных клавиатур, привязанные к паду (пусто —
    /// пад без клавиатурных биндов).
    #[serde(default)]
    pub keybinds: Vec<KeyBind>,
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct FpvConfig {
    #[serde(rename = "camera-id")]
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

impl ColorConfig {
    pub fn to_color32(self) -> egui::Color32 {
        match self {
            ColorConfig::Red => egui::Color32::from_rgb(0xFF, 0x33, 0x33),
            ColorConfig::Blue => egui::Color32::from_rgb(0x33, 0x33, 0xFF),
            ColorConfig::Green => egui::Color32::from_rgb(0x33, 0xCC, 0x33),
            ColorConfig::Yellow => egui::Color32::from_rgb(0xCC, 0x88, 0x00),
        }
    }
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
    fn test_pad_keybinds() {
        // keybinds parse: key as a yaml number resolves to the usage.
        let pad: PadConfig = from_yaml_string(
            "label: CH1\ncolor: red\nfpv:\n  camera-id: camera-1\nkeybinds:\n  - exclusive-keyboard-id: key-1\n    keys: 1\n    action: lap\n",
        )
        .unwrap();
        assert_eq!(pad.keybinds.len(), 1);
        assert_eq!(
            pad.keybinds[0].keys,
            crate::driver::keyboard::key_set(&[0x1E])
        );
        #[cfg(target_os = "linux")]
        #[cfg(target_os = "linux")]
        assert_eq!(
            pad.keybinds[0].exclusive_keyboard_id.as_deref(),
            Some("key-1")
        );
        // Отсутствие блока — пустой список, не ошибка.
        let pad: PadConfig =
            from_yaml_string("label: CH1\ncolor: red\nfpv:\n  camera-id: camera-1\n").unwrap();
        assert!(pad.keybinds.is_empty());
        // Неизвестное имя клавиши — ошибка конфига.
        assert!(from_yaml_string::<PadConfig>(
            "label: CH1\ncolor: red\nfpv:\n  camera-id: camera-1\nkeybinds:\n  - exclusive-keyboard-id: key-1\n    key: banana\n    action: lap\n"
        )
        .is_err());
    }
}
