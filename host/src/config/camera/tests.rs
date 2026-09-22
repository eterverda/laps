use super::*;

#[test]
fn test_resolution() {
    let r: ResolutionConfig = serde_yml::from_str("1920x1080").unwrap();
    assert_eq!(
        r,
        ResolutionConfig {
            width: 1920,
            height: 1080
        }
    );
    assert!(serde_yml::from_str::<ResolutionConfig>("1920 x 1080").is_err());
    assert!(serde_yml::from_str::<ResolutionConfig>("1920").is_err());
}

#[test]
fn test_fps() {
    let f: FpsConfig = serde_yml::from_str("30fps").unwrap();
    assert_eq!(f, FpsConfig(30));
    assert!(serde_yml::from_str::<FpsConfig>("30").is_err());
    assert!(serde_yml::from_str::<FpsConfig>("30 FPS").is_err());
}

#[test]
fn test_camera() {
    let cam: CameraConfig = serde_yml::from_str(
        "name: C7-1\nresolution: 1920x1080\nframe-rate: 30fps\nformat: mjpeg\n",
    )
    .unwrap();
    assert_eq!(cam.name, "C7-1");
    assert_eq!(
        cam.resolution,
        ResolutionConfig {
            width: 1920,
            height: 1080
        }
    );
    assert_eq!(cam.frame_rate, FpsConfig(30));
    assert_eq!(cam.format, PixelConfig::Mjpeg);
    // dvr в yaml отсутствует — дефолт.
    assert_eq!(cam.dvr.container, ContainerConfig::Avi);
}

#[test]
fn test_camera_dvr_container() {
    let cam: CameraConfig = serde_yml::from_str(
        "name: C7-1\nresolution: 1920x1080\nframe-rate: 30fps\nformat: mjpeg\ndvr:\n  container: avi\n",
    )
    .unwrap();
    assert_eq!(cam.dvr.container, ContainerConfig::Avi);

    let cam: CameraConfig = serde_yml::from_str(
        "name: C7-1\nresolution: 1920x1080\nframe-rate: 30fps\nformat: mjpeg\ndvr:\n  container: mkv\n",
    )
    .unwrap();
    assert_eq!(cam.dvr.container, ContainerConfig::Mkv);

    assert!(serde_yml::from_str::<CameraConfig>(
        "name: C7-1\nresolution: 1920x1080\nframe-rate: 30fps\nformat: mjpeg\ndvr:\n  container: mp4\n",
    )
    .is_err());
}

#[test]
fn test_viewport_preset() {
    let v: ViewportConfig = serde_yml::from_str("2x2-r1c1-narrow").unwrap();
    assert_eq!(v, VIEWPORT_2X2_R1C1_NARROW);
}

#[test]
fn test_viewport_unknown_preset_fails() {
    assert!(serde_yml::from_str::<ViewportConfig>("2x2-r1c1").is_err());
}

#[test]
fn test_parse_canonical() {
    let cam = "C7-1 USB3 Video 1920x1080 @ 60fps [YUYV]"
        .parse::<CameraConfig>()
        .unwrap();
    assert_eq!(cam.name, "C7-1 USB3 Video");
    assert_eq!(
        cam.resolution,
        ResolutionConfig {
            width: 1920,
            height: 1080
        }
    );
    assert_eq!(cam.frame_rate, FpsConfig(60));
    assert_eq!(cam.format, PixelConfig::Yuyv);
}

#[test]
fn test_parse_lowercase_format() {
    let cam = "FaceTime HD Camera 1280x720 @ 30fps [mjpeg]"
        .parse::<CameraConfig>()
        .unwrap();
    assert_eq!(cam.name, "FaceTime HD Camera");
    assert_eq!(cam.format, PixelConfig::Mjpeg);
}

#[test]
fn test_parse_missing_format_fails() {
    assert!(
        "C7-1 USB3 Video 1920x1080 @ 60fps"
            .parse::<CameraConfig>()
            .is_err()
    );
}

#[test]
fn test_parse_unknown_format_fails() {
    assert!(
        "C7-1 USB3 Video 1920x1080 @ 60fps [H264]"
            .parse::<CameraConfig>()
            .is_err()
    );
}

#[test]
fn test_parse_spaces_around_x_fails() {
    assert!(
        "C7-1 USB3 Video 1920 x 1080 @ 60fps [YUYV]"
            .parse::<CameraConfig>()
            .is_err()
    );
}

#[test]
fn test_parse_uppercase_fps_fails() {
    assert!(
        "C7-1 USB3 Video 1920x1080 @ 60 FPS [YUYV]"
            .parse::<CameraConfig>()
            .is_err()
    );
}

#[test]
fn test_parse_trailing_junk_fails() {
    assert!(
        "C7-1 USB3 Video 1920x1080 @ 60fps [YUYV] extra"
            .parse::<CameraConfig>()
            .is_err()
    );
}

#[test]
fn test_display() {
    let cam = CameraConfig::new("C7-1 USB3 Video", 3840, 2160, 18, PixelConfig::Mjpeg);
    assert_eq!(cam.to_string(), "C7-1 USB3 Video 3840x2160 @ 18fps [MJPEG]");
}

#[test]
fn test_display_round_trip() {
    let cam = CameraConfig::new("Test Cam", 1920, 1080, 30, PixelConfig::Yuyv);
    let parsed: CameraConfig = cam.to_string().parse().unwrap();
    assert_eq!(parsed, cam);
}

#[test]
fn test_matches() {
    let c7 = CameraConfig::new("C7-1 USB3 Video", 1920, 1080, 60, PixelConfig::Yuyv);
    assert!(c7.matches("C7-1 USB3 Video", 1920, 1080, 60, "YUYV"));
    assert!(c7.matches("C7-1 USB3 Video", 1920, 1080, 60, "yuyv"));
    assert!(!c7.matches("C7-1 USB3 Video", 1920, 1080, 60, "MJPEG"));
    assert!(!c7.matches("FaceTime HD Camera", 1920, 1080, 60, "YUYV"));

    let c7_short = CameraConfig::new("c7", 1920, 1080, 60, PixelConfig::Yuyv);
    assert!(c7_short.matches("C7-1 USB3 Video", 1920, 1080, 60, "YUYV"));

    let c7_wrong_res = CameraConfig::new("C7-1", 1280, 720, 60, PixelConfig::Yuyv);
    assert!(!c7_wrong_res.matches("C7-1 USB3 Video", 1920, 1080, 60, "YUYV"));
}
