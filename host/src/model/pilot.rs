#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Pilot {
    pub name: String,
}

impl Pilot {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}
