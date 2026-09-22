use serde::Deserialize;
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PixelFormat {
    Yuyv,
    Mjpeg,
}

impl PixelFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            PixelFormat::Yuyv => "YUYV",
            PixelFormat::Mjpeg => "MJPEG",
        }
    }
}

impl fmt::Display for PixelFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for PixelFormat {
    type Err = ParseCameraError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.eq_ignore_ascii_case("yuyv") {
            Ok(PixelFormat::Yuyv)
        } else if s.eq_ignore_ascii_case("mjpeg") {
            Ok(PixelFormat::Mjpeg)
        } else {
            Err(ParseCameraError)
        }
    }
}

impl<'de> Deserialize<'de> for PixelFormat {
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
pub struct Camera {
    pub name: String,
    pub resolution: Resolution,
    #[serde(rename = "frame-rate")]
    pub frame_rate: Fps,
    pub format: PixelFormat,
    #[serde(default)]
    pub dvr: Dvr,
}

/// Настройки записи камеры. Отсутствие блока в yaml = значения по умолчанию.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
pub struct Dvr {
    #[serde(default)]
    pub container: Container,
}

/// Контейнер DVR-записи. Пока единственный вариант.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Container {
    #[default]
    Avi,
}

impl Camera {
    pub fn new(
        name: impl Into<String>,
        width: u32,
        height: u32,
        fps: u32,
        format: PixelFormat,
    ) -> Self {
        Self {
            name: name.into(),
            resolution: Resolution { width, height },
            frame_rate: Fps(fps),
            format,
            dvr: Dvr::default(),
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

impl fmt::Display for Camera {
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

impl FromStr for Camera {
    type Err = ParseCameraError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Grammar for `Camera`, anchored: the whole string must match. The
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
        Ok(Camera::new(name, width, height, fps, format))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}

impl<'de> Deserialize<'de> for Resolution {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        let caps = lazy_regex::lazy_regex!(r"^(\d+)x(\d+)$")
            .captures(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("invalid resolution: {s}")))?;
        Ok(Resolution {
            width: caps[1].parse().map_err(serde::de::Error::custom)?,
            height: caps[2].parse().map_err(serde::de::Error::custom)?,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Fps(pub u32);

impl<'de> Deserialize<'de> for Fps {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        let caps = lazy_regex::lazy_regex!(r"^(\d+)fps$")
            .captures(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("invalid frame rate: {s}")))?;
        Ok(Fps(caps[1].parse().map_err(serde::de::Error::custom)?))
    }
}

/// A rectangular region of a video frame in normalized 0..1 coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl Default for Viewport {
    /// The whole frame.
    fn default() -> Self {
        Viewport {
            left: 0.0,
            top: 0.0,
            right: 1.0,
            bottom: 1.0,
        }
    }
}

impl Viewport {
    pub fn to_rect(&self) -> egui::Rect {
        egui::Rect::from_min_max(
            egui::pos2(self.left, self.top),
            egui::pos2(self.right, self.bottom),
        )
    }
}

impl<'de> Deserialize<'de> for Viewport {
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
pub const VIEWPORT_2X2_R1C1_NARROW: Viewport = Viewport {
    left: 0.0625,
    top: 0.0,
    right: 0.4375,
    bottom: 0.5,
};
pub const VIEWPORT_2X2_R1C2_NARROW: Viewport = Viewport {
    left: 0.5625,
    top: 0.0,
    right: 0.9375,
    bottom: 0.5,
};
pub const VIEWPORT_2X2_R2C1_NARROW: Viewport = Viewport {
    left: 0.0625,
    top: 0.5,
    right: 0.4375,
    bottom: 1.0,
};
pub const VIEWPORT_2X2_R2C2_NARROW: Viewport = Viewport {
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
        let r: Resolution = serde_yml::from_str("1920x1080").unwrap();
        assert_eq!(
            r,
            Resolution {
                width: 1920,
                height: 1080
            }
        );
        assert!(serde_yml::from_str::<Resolution>("1920 x 1080").is_err());
        assert!(serde_yml::from_str::<Resolution>("1920").is_err());
    }

    #[test]
    fn test_fps() {
        let f: Fps = serde_yml::from_str("30fps").unwrap();
        assert_eq!(f, Fps(30));
        assert!(serde_yml::from_str::<Fps>("30").is_err());
        assert!(serde_yml::from_str::<Fps>("30 FPS").is_err());
    }

    #[test]
    fn test_camera() {
        let cam: Camera = serde_yml::from_str(
            "name: C7-1\nresolution: 1920x1080\nframe-rate: 30fps\nformat: mjpeg\n",
        )
        .unwrap();
        assert_eq!(cam.name, "C7-1");
        assert_eq!(
            cam.resolution,
            Resolution {
                width: 1920,
                height: 1080
            }
        );
        assert_eq!(cam.frame_rate, Fps(30));
        assert_eq!(cam.format, PixelFormat::Mjpeg);
        // dvr в yaml отсутствует — дефолт.
        assert_eq!(cam.dvr.container, Container::Avi);
    }

    #[test]
    fn test_camera_dvr_container() {
        let cam: Camera = serde_yml::from_str(
            "name: C7-1\nresolution: 1920x1080\nframe-rate: 30fps\nformat: mjpeg\ndvr:\n  container: avi\n",
        )
        .unwrap();
        assert_eq!(cam.dvr.container, Container::Avi);

        assert!(serde_yml::from_str::<Camera>(
            "name: C7-1\nresolution: 1920x1080\nframe-rate: 30fps\nformat: mjpeg\ndvr:\n  container: mkv\n",
        )
        .is_err());
    }

    #[test]
    fn test_viewport_preset() {
        let v: Viewport = serde_yml::from_str("2x2-r1c1-narrow").unwrap();
        assert_eq!(v, VIEWPORT_2X2_R1C1_NARROW);
    }

    #[test]
    fn test_viewport_unknown_preset_fails() {
        assert!(serde_yml::from_str::<Viewport>("2x2-r1c1").is_err());
    }

    #[test]
    fn test_parse_canonical() {
        let cam = "C7-1 USB3 Video 1920x1080 @ 60fps [YUYV]"
            .parse::<Camera>()
            .unwrap();
        assert_eq!(cam.name, "C7-1 USB3 Video");
        assert_eq!(
            cam.resolution,
            Resolution {
                width: 1920,
                height: 1080
            }
        );
        assert_eq!(cam.frame_rate, Fps(60));
        assert_eq!(cam.format, PixelFormat::Yuyv);
    }

    #[test]
    fn test_parse_lowercase_format() {
        let cam = "FaceTime HD Camera 1280x720 @ 30fps [mjpeg]"
            .parse::<Camera>()
            .unwrap();
        assert_eq!(cam.name, "FaceTime HD Camera");
        assert_eq!(cam.format, PixelFormat::Mjpeg);
    }

    #[test]
    fn test_parse_missing_format_fails() {
        assert!(
            "C7-1 USB3 Video 1920x1080 @ 60fps"
                .parse::<Camera>()
                .is_err()
        );
    }

    #[test]
    fn test_parse_unknown_format_fails() {
        assert!(
            "C7-1 USB3 Video 1920x1080 @ 60fps [H264]"
                .parse::<Camera>()
                .is_err()
        );
    }

    #[test]
    fn test_parse_spaces_around_x_fails() {
        assert!(
            "C7-1 USB3 Video 1920 x 1080 @ 60fps [YUYV]"
                .parse::<Camera>()
                .is_err()
        );
    }

    #[test]
    fn test_parse_uppercase_fps_fails() {
        assert!(
            "C7-1 USB3 Video 1920x1080 @ 60 FPS [YUYV]"
                .parse::<Camera>()
                .is_err()
        );
    }

    #[test]
    fn test_parse_trailing_junk_fails() {
        assert!(
            "C7-1 USB3 Video 1920x1080 @ 60fps [YUYV] extra"
                .parse::<Camera>()
                .is_err()
        );
    }

    #[test]
    fn test_display() {
        let cam = Camera::new("C7-1 USB3 Video", 3840, 2160, 18, PixelFormat::Mjpeg);
        assert_eq!(cam.to_string(), "C7-1 USB3 Video 3840x2160 @ 18fps [MJPEG]");
    }

    #[test]
    fn test_display_round_trip() {
        let cam = Camera::new("Test Cam", 1920, 1080, 30, PixelFormat::Yuyv);
        let parsed: Camera = cam.to_string().parse().unwrap();
        assert_eq!(parsed, cam);
    }

    #[test]
    fn test_matches() {
        let c7 = Camera::new("C7-1 USB3 Video", 1920, 1080, 60, PixelFormat::Yuyv);
        assert!(c7.matches("C7-1 USB3 Video", 1920, 1080, 60, "YUYV"));
        assert!(c7.matches("C7-1 USB3 Video", 1920, 1080, 60, "yuyv"));
        assert!(!c7.matches("C7-1 USB3 Video", 1920, 1080, 60, "MJPEG"));
        assert!(!c7.matches("FaceTime HD Camera", 1920, 1080, 60, "YUYV"));

        let c7_short = Camera::new("c7", 1920, 1080, 60, PixelFormat::Yuyv);
        assert!(c7_short.matches("C7-1 USB3 Video", 1920, 1080, 60, "YUYV"));

        let c7_wrong_res = Camera::new("C7-1", 1280, 720, 60, PixelFormat::Yuyv);
        assert!(!c7_wrong_res.matches("C7-1 USB3 Video", 1920, 1080, 60, "YUYV"));
    }
}
