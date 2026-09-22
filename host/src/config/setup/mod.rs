use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Deserialize)]
pub struct Setup {
    pub cameras: BTreeMap<String, super::camera::CameraConfig>,
    pub pads: BTreeMap<String, PadConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PadConfig {
    pub label: String,
    pub color: ColorConfig,
    pub fpv: FpvConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FpvConfig {
    #[serde(rename = "camera-id")]
    pub camera_id: String,
    #[serde(default)]
    pub viewport: super::camera::ViewportConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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
mod tests;
