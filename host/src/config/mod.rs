use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CameraDescription {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl CameraDescription {
    pub fn new(name: impl Into<String>, width: u32, height: u32, fps: u32) -> Self {
        Self {
            name: name.into(),
            width,
            height,
            fps,
        }
    }

    pub fn matches(&self, name: &str, width: u32, height: u32, fps: u32) -> bool {
        let name_match = name.to_lowercase().contains(&self.name.to_lowercase());
        name_match && self.width == width && self.height == height && self.fps == fps
    }
}

impl fmt::Display for CameraDescription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {}x{} @ {} FPS",
            self.name, self.width, self.height, self.fps
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseCameraDescriptionError;

impl fmt::Display for ParseCameraDescriptionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid camera description format, expected: 'Name 1920x1080 @ 30 FPS'"
        )
    }
}

impl std::error::Error for ParseCameraDescriptionError {}

impl FromStr for CameraDescription {
    type Err = ParseCameraDescriptionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let re =
            regex::Regex::new(r"(.+?)\s+(\d+)\s*[Xx]\s*(\d+)\s*@\s*(\d+)\s*[Ff][Pp][Ss]").unwrap();
        let caps = re.captures(s).ok_or(ParseCameraDescriptionError)?;
        let name = caps[1].trim().to_string();
        let width = caps[2].parse().map_err(|_| ParseCameraDescriptionError)?;
        let height = caps[3].parse().map_err(|_| ParseCameraDescriptionError)?;
        let fps = caps[4].parse().map_err(|_| ParseCameraDescriptionError)?;
        Ok(CameraDescription::new(name, width, height, fps))
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
    fn test_parse_uppercase() {
        let desc = "C7-1 USB3 Video 1920x1080 @ 60 FPS"
            .parse::<CameraDescription>()
            .unwrap();
        assert_eq!(desc.name, "C7-1 USB3 Video");
        assert_eq!(desc.width, 1920);
        assert_eq!(desc.height, 1080);
        assert_eq!(desc.fps, 60);
    }

    #[test]
    fn test_parse_lowercase() {
        let desc = "FaceTime HD Camera 1280 x 720 @ 30 fps"
            .parse::<CameraDescription>()
            .unwrap();
        assert_eq!(desc.name, "FaceTime HD Camera");
        assert_eq!(desc.width, 1280);
        assert_eq!(desc.height, 720);
        assert_eq!(desc.fps, 30);
    }

    #[test]
    fn test_display() {
        let desc = CameraDescription::new("C7-1 USB3 Video", 3840, 2160, 18);
        assert_eq!(desc.to_string(), "C7-1 USB3 Video 3840x2160 @ 18 FPS");
    }

    #[test]
    fn test_serde_json() {
        let desc = CameraDescription::new("Test Cam", 1920, 1080, 30);
        let json = serde_json::to_string(&desc).unwrap();
        assert_eq!(json, "\"Test Cam 1920x1080 @ 30 FPS\"");
        let parsed: CameraDescription = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, desc);
    }

    #[test]
    fn test_serde_yaml() {
        let desc = CameraDescription::new("Test Cam", 1920, 1080, 30);
        let yaml = serde_yaml::to_string(&desc).unwrap();
        assert!(yaml.contains("Test Cam 1920x1080 @ 30 FPS"));
        let parsed: CameraDescription = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, desc);
    }

    #[test]
    fn test_matches() {
        let c7 = CameraDescription::new("C7-1 USB3 Video", 1920, 1080, 60);
        assert!(c7.matches("C7-1 USB3 Video", 1920, 1080, 60));
        assert!(!c7.matches("FaceTime HD Camera", 1920, 1080, 60));

        let c7_short = CameraDescription::new("c7", 1920, 1080, 60);
        assert!(c7_short.matches("C7-1 USB3 Video", 1920, 1080, 60));
        assert!(!c7_short.matches("FaceTime HD Camera", 1920, 1080, 60));

        let c7_wrong_res = CameraDescription::new("C7-1", 1280, 720, 60);
        assert!(!c7_wrong_res.matches("C7-1 USB3 Video", 1920, 1080, 60));
        assert!(!c7_wrong_res.matches("FaceTime HD Camera", 1280, 720, 60));
    }
}
