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
    type Err = ParseCameraDescriptionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.eq_ignore_ascii_case("yuyv") {
            Ok(PixelFormat::Yuyv)
        } else if s.eq_ignore_ascii_case("mjpeg") {
            Ok(PixelFormat::Mjpeg)
        } else {
            Err(ParseCameraDescriptionError)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CameraDescription {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub format: PixelFormat,
}

impl CameraDescription {
    pub fn new(
        name: impl Into<String>,
        width: u32,
        height: u32,
        fps: u32,
        format: PixelFormat,
    ) -> Self {
        Self {
            name: name.into(),
            width,
            height,
            fps,
            format,
        }
    }

    pub fn matches(&self, name: &str, width: u32, height: u32, fps: u32, format: &str) -> bool {
        let name_matches = name.to_lowercase().contains(&self.name.to_lowercase());
        let format_matches = self.format.as_str().eq_ignore_ascii_case(format);
        name_matches
            && self.width == width
            && self.height == height
            && self.fps == fps
            && format_matches
    }
}

impl fmt::Display for CameraDescription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {}x{} @ {}fps [{}]",
            self.name, self.width, self.height, self.fps, self.format,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseCameraDescriptionError;

impl fmt::Display for ParseCameraDescriptionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid camera description format, expected: 'Name 1920x1080 @ 30fps [YUYV]'"
        )
    }
}

impl std::error::Error for ParseCameraDescriptionError {}

impl FromStr for CameraDescription {
    type Err = ParseCameraDescriptionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Grammar for `CameraDescription`, anchored: the whole string must
        // match. The pixel format in square brackets is required. Validated
        // at compile time, built once, reused across calls.
        let caps = lazy_regex::lazy_regex!(r"^(.+?) (\d+)x(\d+) @ (\d+)fps \[([A-Za-z0-9]+)\]$")
            .captures(s)
            .ok_or(ParseCameraDescriptionError)?;
        let name = caps[1].trim().to_string();
        let width = caps[2].parse().map_err(|_| ParseCameraDescriptionError)?;
        let height = caps[3].parse().map_err(|_| ParseCameraDescriptionError)?;
        let fps = caps[4].parse().map_err(|_| ParseCameraDescriptionError)?;
        let format = caps[5].parse()?;
        Ok(CameraDescription::new(name, width, height, fps, format))
    }
}

impl serde::Serialize for CameraDescription {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> serde::Deserialize<'de> for CameraDescription {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        CameraDescription::from_str(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_canonical() {
        let desc = "C7-1 USB3 Video 1920x1080 @ 60fps [YUYV]"
            .parse::<CameraDescription>()
            .unwrap();
        assert_eq!(desc.name, "C7-1 USB3 Video");
        assert_eq!(desc.width, 1920);
        assert_eq!(desc.height, 1080);
        assert_eq!(desc.fps, 60);
        assert_eq!(desc.format, PixelFormat::Yuyv);
    }

    #[test]
    fn test_parse_lowercase_format() {
        let desc = "FaceTime HD Camera 1280x720 @ 30fps [mjpeg]"
            .parse::<CameraDescription>()
            .unwrap();
        assert_eq!(desc.name, "FaceTime HD Camera");
        assert_eq!(desc.format, PixelFormat::Mjpeg);
    }

    #[test]
    fn test_parse_missing_format_fails() {
        assert!(
            "C7-1 USB3 Video 1920x1080 @ 60fps"
                .parse::<CameraDescription>()
                .is_err()
        );
    }

    #[test]
    fn test_parse_unknown_format_fails() {
        assert!(
            "C7-1 USB3 Video 1920x1080 @ 60fps [H264]"
                .parse::<CameraDescription>()
                .is_err()
        );
    }

    #[test]
    fn test_parse_spaces_around_x_fails() {
        assert!(
            "C7-1 USB3 Video 1920 x 1080 @ 60fps [YUYV]"
                .parse::<CameraDescription>()
                .is_err()
        );
    }

    #[test]
    fn test_parse_uppercase_fps_fails() {
        assert!(
            "C7-1 USB3 Video 1920x1080 @ 60 FPS [YUYV]"
                .parse::<CameraDescription>()
                .is_err()
        );
    }

    #[test]
    fn test_parse_trailing_junk_fails() {
        assert!(
            "C7-1 USB3 Video 1920x1080 @ 60fps [YUYV] extra"
                .parse::<CameraDescription>()
                .is_err()
        );
    }

    #[test]
    fn test_display() {
        let desc = CameraDescription::new("C7-1 USB3 Video", 3840, 2160, 18, PixelFormat::Mjpeg);
        assert_eq!(
            desc.to_string(),
            "C7-1 USB3 Video 3840x2160 @ 18fps [MJPEG]",
        );
    }

    #[test]
    fn test_serde_json() {
        let desc = CameraDescription::new("Test Cam", 1920, 1080, 30, PixelFormat::Yuyv);
        let json = serde_json::to_string(&desc).unwrap();
        assert_eq!(json, "\"Test Cam 1920x1080 @ 30fps [YUYV]\"");
        let parsed: CameraDescription = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, desc);
    }

    #[test]
    fn test_serde_yaml() {
        let desc = CameraDescription::new("Test Cam", 1920, 1080, 30, PixelFormat::Yuyv);
        let yaml = serde_yaml::to_string(&desc).unwrap();
        assert!(yaml.contains("Test Cam 1920x1080 @ 30fps [YUYV]"));
        let parsed: CameraDescription = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, desc);
    }

    #[test]
    fn test_matches() {
        let c7 = CameraDescription::new("C7-1 USB3 Video", 1920, 1080, 60, PixelFormat::Yuyv);
        assert!(c7.matches("C7-1 USB3 Video", 1920, 1080, 60, "YUYV"));
        assert!(c7.matches("C7-1 USB3 Video", 1920, 1080, 60, "yuyv"));
        assert!(!c7.matches("C7-1 USB3 Video", 1920, 1080, 60, "MJPEG"));
        assert!(!c7.matches("FaceTime HD Camera", 1920, 1080, 60, "YUYV"));

        let c7_short = CameraDescription::new("c7", 1920, 1080, 60, PixelFormat::Yuyv);
        assert!(c7_short.matches("C7-1 USB3 Video", 1920, 1080, 60, "YUYV"));

        let c7_wrong_res = CameraDescription::new("C7-1", 1280, 720, 60, PixelFormat::Yuyv);
        assert!(!c7_wrong_res.matches("C7-1 USB3 Video", 1920, 1080, 60, "YUYV"));
    }
}
