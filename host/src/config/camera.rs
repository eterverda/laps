use serde::Deserialize;
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PixelConfig {
    Yuyv,
    Mjpeg,
}

impl PixelConfig {
    pub fn as_str(&self) -> &'static str {
        match self {
            PixelConfig::Yuyv => "YUYV",
            PixelConfig::Mjpeg => "MJPEG",
        }
    }
}

impl fmt::Display for PixelConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for PixelConfig {
    type Err = ParseCameraError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.eq_ignore_ascii_case("yuyv") {
            Ok(PixelConfig::Yuyv)
        } else if s.eq_ignore_ascii_case("mjpeg") {
            Ok(PixelConfig::Mjpeg)
        } else {
            Err(ParseCameraError)
        }
    }
}

impl<'de> Deserialize<'de> for PixelConfig {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseCameraError;

impl fmt::Display for ParseCameraError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid camera format, expected: 'Name 1920x1080 @ 30fps [YUYV]'"
        )
    }
}

impl std::error::Error for ParseCameraError {}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct CameraConfig {
    pub name: String,
    pub resolution: ResolutionConfig,
    #[serde(rename = "frame-rate")]
    pub frame_rate: FpsConfig,
    pub format: PixelConfig,
    #[serde(default)]
    pub dvr: DvrConfig,
}

/// Настройки записи камеры. Отсутствие блока в yaml = значения по умолчанию.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
pub struct DvrConfig {
    #[serde(default)]
    pub container: ContainerConfig,
}

/// Контейнер DVR-записи.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContainerConfig {
    #[default]
    Mkv,
    Mp4,
    Mov,
}

impl CameraConfig {
    pub fn new(
        name: impl Into<String>,
        width: u32,
        height: u32,
        fps: u32,
        format: PixelConfig,
    ) -> Self {
        Self {
            name: name.into(),
            resolution: ResolutionConfig { width, height },
            frame_rate: FpsConfig(fps),
            format,
            dvr: DvrConfig::default(),
        }
    }

    pub fn matches(&self, name: &str, width: u32, height: u32, fps: u32, format: &str) -> bool {
        let name_matches = name.to_lowercase().contains(&self.name.to_lowercase());
        let format_matches = self.format.as_str().eq_ignore_ascii_case(format);
        name_matches
            && self.resolution.width == width
            && self.resolution.height == height
            && self.frame_rate.0 == fps
            && format_matches
    }
}

impl fmt::Display for CameraConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {}x{} @ {}fps [{}]",
            self.name,
            self.resolution.width,
            self.resolution.height,
            self.frame_rate.0,
            self.format,
        )
    }
}

impl FromStr for CameraConfig {
    type Err = ParseCameraError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Grammar for `CameraConfig`, anchored: the whole string must match. The
        // pixel format in square brackets is required. Validated at compile
        // time, built once, reused across calls.
        let caps = lazy_regex::lazy_regex!(r"^(.+?) (\d+)x(\d+) @ (\d+)fps \[([A-Za-z0-9]+)\]$")
            .captures(s)
            .ok_or(ParseCameraError)?;
        let name = caps[1].trim().to_string();
        let width = caps[2].parse().map_err(|_| ParseCameraError)?;
        let height = caps[3].parse().map_err(|_| ParseCameraError)?;
        let fps = caps[4].parse().map_err(|_| ParseCameraError)?;
        let format = caps[5].parse()?;
        Ok(CameraConfig::new(name, width, height, fps, format))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolutionConfig {
    pub width: u32,
    pub height: u32,
}

impl<'de> Deserialize<'de> for ResolutionConfig {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        let caps = lazy_regex::lazy_regex!(r"^(\d+)x(\d+)$")
            .captures(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("invalid resolution: {s}")))?;
        Ok(ResolutionConfig {
            width: caps[1].parse().map_err(serde::de::Error::custom)?,
            height: caps[2].parse().map_err(serde::de::Error::custom)?,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FpsConfig(pub u32);

impl<'de> Deserialize<'de> for FpsConfig {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        let caps = lazy_regex::lazy_regex!(r"^(\d+)fps$")
            .captures(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("invalid frame rate: {s}")))?;
        Ok(FpsConfig(
            caps[1].parse().map_err(serde::de::Error::custom)?,
        ))
    }
}

/// A rectangular region of a video frame in normalized 0..1 coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewportConfig {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl Default for ViewportConfig {
    /// The whole frame.
    fn default() -> Self {
        ViewportConfig {
            left: 0.0,
            top: 0.0,
            right: 1.0,
            bottom: 1.0,
        }
    }
}

impl ViewportConfig {
    pub fn to_rect(&self) -> egui::Rect {
        egui::Rect::from_min_max(
            egui::pos2(self.left, self.top),
            egui::pos2(self.right, self.bottom),
        )
    }
}

impl<'de> Deserialize<'de> for ViewportConfig {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let name = String::deserialize(d)?;
        match name.as_str() {
            "2x2-r1c1-narrow" => Ok(VIEWPORT_2X2_R1C1_NARROW),
            "2x2-r1c2-narrow" => Ok(VIEWPORT_2X2_R1C2_NARROW),
            "2x2-r2c1-narrow" => Ok(VIEWPORT_2X2_R2C1_NARROW),
            "2x2-r2c2-narrow" => Ok(VIEWPORT_2X2_R2C2_NARROW),
            other => Err(serde::de::Error::custom(format!(
                "unknown viewport preset: {other}"
            ))),
        }
    }
}

/// Named viewport presets, referenced from setup files.
pub const VIEWPORT_2X2_R1C1_NARROW: ViewportConfig = ViewportConfig {
    left: 0.0625,
    top: 0.0,
    right: 0.4375,
    bottom: 0.5,
};
pub const VIEWPORT_2X2_R1C2_NARROW: ViewportConfig = ViewportConfig {
    left: 0.5625,
    top: 0.0,
    right: 0.9375,
    bottom: 0.5,
};
pub const VIEWPORT_2X2_R2C1_NARROW: ViewportConfig = ViewportConfig {
    left: 0.0625,
    top: 0.5,
    right: 0.4375,
    bottom: 1.0,
};
pub const VIEWPORT_2X2_R2C2_NARROW: ViewportConfig = ViewportConfig {
    left: 0.5625,
    top: 0.5,
    right: 0.9375,
    bottom: 1.0,
};

#[cfg(test)]
mod tests {
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
        assert_eq!(cam.dvr.container, ContainerConfig::Mkv);
    }

    #[test]
    fn test_camera_dvr_container() {
        let cam: CameraConfig = serde_yml::from_str(
            "name: C7-1\nresolution: 1920x1080\nframe-rate: 30fps\nformat: mjpeg\ndvr:\n  container: mkv\n",
        )
        .unwrap();
        assert_eq!(cam.dvr.container, ContainerConfig::Mkv);

        let cam: CameraConfig = serde_yml::from_str(
            "name: C7-1\nresolution: 1920x1080\nframe-rate: 30fps\nformat: mjpeg\ndvr:\n  container: mp4\n",
        )
        .unwrap();
        assert_eq!(cam.dvr.container, ContainerConfig::Mp4);

        let cam: CameraConfig = serde_yml::from_str(
            "name: C7-1\nresolution: 1920x1080\nframe-rate: 30fps\nformat: mjpeg\ndvr:\n  container: mov\n",
        )
        .unwrap();
        assert_eq!(cam.dvr.container, ContainerConfig::Mov);

        assert!(serde_yml::from_str::<CameraConfig>(
            "name: C7-1\nresolution: 1920x1080\nframe-rate: 30fps\nformat: mjpeg\ndvr:\n  container: webm\n",
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
}
