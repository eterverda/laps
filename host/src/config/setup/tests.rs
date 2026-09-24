use super::*;

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
