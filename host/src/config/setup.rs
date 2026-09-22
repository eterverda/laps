use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Deserialize)]
pub struct Setup {
    pub cameras: BTreeMap<String, super::camera::Camera>,
    pub pads: BTreeMap<String, Pad>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Pad {
    pub label: String,
    pub color: Color,
    pub fpv: Fpv,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Fpv {
    #[serde(rename = "camera-id")]
    pub camera_id: String,
    #[serde(default)]
    pub viewport: super::camera::Viewport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Color {
    Red,
    Blue,
    Green,
    Yellow,
}

impl Color {
    pub fn to_color32(self) -> egui::Color32 {
        match self {
            Color::Red => egui::Color32::from_rgb(0xFF, 0x33, 0x33),
            Color::Blue => egui::Color32::from_rgb(0x33, 0x33, 0xFF),
            Color::Green => egui::Color32::from_rgb(0x33, 0xCC, 0x33),
            Color::Yellow => egui::Color32::from_rgb(0xCC, 0x88, 0x00),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_setup_yaml() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/setup.yaml");
        let text = std::fs::read_to_string(path).unwrap();
        let setup: Setup = serde_yml::from_str(&text).unwrap();

        assert_eq!(setup.cameras.len(), 1);
        assert_eq!(setup.cameras["camera-1"].name, "C7-1");
        assert_eq!(
            setup.cameras["camera-1"].dvr.container,
            super::super::camera::Container::Avi
        );
        assert_eq!(setup.pads.len(), 4);

        assert_eq!(setup.pads["pad-1"].label, "CH1");
        assert_eq!(setup.pads["pad-1"].color, Color::Red);
        assert_eq!(
            setup.pads["pad-1"].fpv.viewport,
            super::super::camera::VIEWPORT_2X2_R1C1_NARROW
        );
    }

    #[test]
    fn test_camera_references_resolve() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/setup.yaml");
        let text = std::fs::read_to_string(path).unwrap();
        let setup: Setup = serde_yml::from_str(&text).unwrap();

        for (id, pad) in &setup.pads {
            assert!(
                setup.cameras.contains_key(&pad.fpv.camera_id),
                "pad {id} references unknown camera '{}'",
                pad.fpv.camera_id,
            );
        }
    }

    #[test]
    fn test_viewport_defaults_to_full_frame() {
        let fpv: Fpv = serde_yml::from_str("camera-id: camera-1\n").unwrap();
        assert_eq!(
            fpv.viewport,
            super::super::camera::Viewport {
                left: 0.0,
                top: 0.0,
                right: 1.0,
                bottom: 1.0,
            }
        );
    }
}
