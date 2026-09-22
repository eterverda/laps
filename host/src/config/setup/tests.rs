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
        super::super::camera::ContainerConfig::Avi
    );
    assert_eq!(setup.pads.len(), 4);

    assert_eq!(setup.pads["pad-1"].label, "CH1");
    assert_eq!(setup.pads["pad-1"].color, ColorConfig::Red);
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
    let fpv: FpvConfig = serde_yml::from_str("camera-id: camera-1\n").unwrap();
    assert_eq!(
        fpv.viewport,
        super::super::camera::ViewportConfig {
            left: 0.0,
            top: 0.0,
            right: 1.0,
            bottom: 1.0,
        }
    );
}
